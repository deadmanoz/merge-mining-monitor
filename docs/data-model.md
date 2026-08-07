# Data Model

The database separates captured evidence from derived presentation state.
Producers append or update base evidence; the read model computes the Bitcoin
tree view from that evidence.

## Core Tables

| Table | Purpose |
|---|---|
| `source` | Registered evidence sources such as `auxpow:namecoin`, `auxpow:rsk`, or `live-chaintip:bitcoin:core`. |
| `pool` | Stable pool identities loaded from `data/pools/current.json`. |
| `pool_identity` | Native child-chain identities that map to a pool, such as RSK miner addresses or child reward addresses. |
| `merge_mining_event` | Source evidence keyed by exact or partial authenticated child identity and its Bitcoin parent header. |
| `historical_event_provenance` | Publication-side chain, source row, classification, validation, and relevance provenance attached to imported events; multiple source rows can map to one event. |
| `historical_reconcile_queue` | Durable parent rebuild state and dependent-cascade seeds for resumable historical imports. |
| chain sidecars | One-to-one evidence details for chains with extra structured data, such as RSK and Hathor. |
| `event_pool_attribution` | Attribution rows connecting an event to a pool with source/provenance details. |
| `poll_cursor` | Live poll progress. Backfills never move the cursor. |
| `block` | Derived Bitcoin parent block state: canonical, stale, catalogued consensus-invalid error block, or unknown. |
| `attestation_proof` | Derived proof rows supporting a block. |
| `source_health` | Per-source rollup counters for UI/API health reporting. |

## Parent Classification

`btc_parent_kind` is one of:

- `near` - parent header fails Bitcoin target validation.
- `unknown` - parent header passes target validation, but no Bitcoin-chain
  membership proof is available.
- `canonical` - Bitcoin Core proves the parent is on the active chain.
- `stale` - Bitcoin Core proves the parent is a valid off-chain Bitcoin block.
- `error_block` - parent hash meets Bitcoin's target but matches the pinned
  research catalogue of reproducibly validated consensus-invalid headers. It is
  neither stale nor an orphan; `block.error_block_reason` records its primary
  rejection token.

Orphan status is not a `btc_parent_kind`. It is the derived
`block.btc_orphan_class` (`strict_btc_orphan`, `weak_btc_orphan`, or
`excluded`; NULL while pending), set only after a Core-absence-attested verdict
and the offline strict/weak orphan classifier. Before the strict/weak
resolution runs, the classifier consults the operator-imported
`known_stale_block` membership (loaded by `import-known-stales` from the
upstream `bitcoin-data/stale-blocks` dataset): a catalogued stale is `excluded`
outright, never labelled strict/weak, and `reclassify-known-stales`
retroactively demotes rows classified before the membership was imported.
Published direct-stale and stale-descendant provenance is also an exclusion
from strict/weak orphan classification while a branch remains derived
`unknown`.

## Child Observation Identity

Child height, hash, header, time, and `nBits` are nullable, independent evidence
fields. Missing source evidence remains `NULL`; producers and importers do not
fabricate substitutes.

- A real child hash is exact identity under `(source_id, child_block_hash)`.
- One exact identity has one stored Bitcoin parent. A second observation with
  the same source and child hash but a different parent is contradictory
  source evidence and fails closed.
- Historical child hashes use the exact bytes stored by live capture. For
  SHA256d child headers this is raw `sha256d::Hash::to_byte_array()` order, not
  reversed display/RPC order.
- A hashless observation uses
  `(source_id, child_height, btc_parent_header_hash)`.
- Every event has at least a child hash or a child height.
- A later exact observation can promote one unambiguous partial event in place.
- A partial observation represented by one exact event reuses that event.
- Non-null contradictions and ambiguous refinement fail rather than choosing a
  writer.
- The store returns the resulting inserted, updated, promoted, or
  exact-satisfied disposition with the event id so import accounting cannot
  diverge from identity resolution.

The API exposes these fields as nullable values and additionally surfaces an
authenticated `child_header_hex` and `child_nbits` when present.

### What `child_block_time` Means

`child_block_time` is the child block's own claimed timestamp, taken from
whatever the chain commits: the header `nTime` for Namecoin-family chains, the
RSK block timestamp, the Hathor block transaction timestamp. The pool sets it
when it builds that child template. It is not the time the child block was
broadcast, and it is not a second opinion on when the Bitcoin block was found.

Once set it is fixed, and the invariant that fixes it is format-neutral: every
AuxPoW scheme commits child data into the Bitcoin coinbase, so the child stamp
is settled before the Bitcoin work exists and cannot be refreshed when the
proof is finally submitted to the child network. Changing it by one second
changes the committed child data, the coinbase, the Bitcoin merkle root, and
voids the proof of work. Note what this does not say: an unchanged child
template can be committed into several successive Bitcoin jobs, so the stamp
marks when the template was last rebuilt, not the last coinbase that carried
it.

The Namecoin-family form of that commitment is the one the stored `aux_proof`
describes:

```
child header (nTime at byte offset 68)
  -> sha256d -> leaf at the chain's aux merkle slot
  -> aux merkle root, written into the Bitcoin coinbase scriptSig
  -> coinbase txid -> coinbase merkle branch
  -> Bitcoin merkle root -> Bitcoin header -> proof of work
```

`blockchain_branch` proves the child block hash sits at `slot_index` in the aux
merkle tree, and `coinbase_branch` proves the coinbase sits in the Bitcoin
transaction tree. RSK and Hathor commit differently and carry no such pair:
RSKIP-92 midstate compression leaves RSK unable to recover the complete
coinbase (a `coinbase_tail` is retained, and exposed as `coinbase_tail_hex`),
and Hathor uses the RFC 0006 split header (see `docs/capture.md`). The sealing
invariant above still holds for both.

How firmly the stamp is evidence varies by source, and `child_header_hex` is a
weaker discriminator than it looks. It answers one question only: whether the
stamp can be re-derived from stored bytes. Among live capture paths only the
Namecoin-family AuxPoW parse stores child header bytes; RSK, Hathor, and
Elastos all write `child_header_bytes: None`, so their stamps are readable only
as the column the source supplied. Historical import is not restricted that
way: the shared CSV importer persists a validated `header_bytes` for any
`HistoricalChainSpec` that supplies one, so an imported non-Namecoin event may
carry a header where its live counterpart would not.

That is not the same question as whether the Bitcoin block committed to the
stamp, and the two answers come apart in both directions. Elastos capture
stores no header, yet reconstructs the 84-byte Elastos header (Bitcoin-shaped
80-byte prefix carrying `time`, plus the height), checks its hash against the
RPC-reported one, and verifies that hash against the CAuxPow commitment before
writing, so its stamp is bound into the committed data. A stored header, by
contrast, does not by itself prove commitment: `validate_child_bundle` checks
that the header hashes to `child_block_hash` and that the supplied timestamp
matches the header's, which is internal consistency, not a parent-side proof.
RSK and Hathor capture copy their chain's RPC timestamp, and a historical
import may carry the publication's own column with nothing to check it against
at all, because that validation only compares a timestamp when a header is
present. Read `child_header_hex` as "re-derivable from stored bytes", check
`aux_proof` or the chain's own capture path for commitment, and treat a stamp
with neither as the source's reported value.

Two asymmetric reading rules follow, and both are bounded by the same fact:
the database holds two claimed timestamps and nothing else. Neither the moment
a child template was built, nor the moment a Bitcoin job was created, nor the
moment the block was found is recorded anywhere. Both stamps are miner-set, the
Bitcoin `nTime` bounded only by median-time-past and future-drift tolerance, so
the offset relates two claims and measures nothing against a reference clock.
The two are not even guaranteed to come from one operator: `docs/attribution.md`
notes that a Bitcoin pool may outsource or proxy child-chain operation through
another operator's endpoint, which is why parent and child attribution are
modelled separately here. Treat the pair as independently controlled unless the
attribution rows evidence common control.

- **A negative offset is an upper bound on child template age, not lateness.**
  Every refresh of a child template costs a new Bitcoin job, so pools re-commit
  fast chains continuously and slow chains about once per job, reusing one child
  header across many jobs. The offset is therefore how stale the committed
  template was relative to the parent's own claimed time, which is an upper
  bound on nothing more than that. The familiar "roughly minus the Bitcoin block
  interval" reading needs a further assumption the data does not carry: that the
  pool rolled `nTime` forward while reusing the template, so the parent stamp
  tracks the solve and the child stamp does not. Where `nTime` was instead fixed
  at job creation alongside the template, the same practice yields an offset near
  zero. Read the magnitude as a property of the pool's refresh and stamping
  policy, and do not derive a verdict about a Bitcoin timestamp from it.
- **A positive offset is an inconsistency worth looking at.** What the
  commitment order fixes is the real-time sequence, not either number: the child
  template is built first, and only then does a Bitcoin job commit to it. So if
  both stamps were honest readings of one clock, the child one could not be the
  later of the two. A positive offset means at least one of them is not the wall
  clock it purports to be. It does not say which. The child may have been
  stamped ahead, which child chains tolerate within their future-drift
  allowance, or the Bitcoin `nTime` left behind the job that carries the
  template, which is equally legal as long as it clears median-time-past. Read
  it as a flag on the pair, never as proof against an external clock; where the
  two stamps came from different operators it is not even an internal
  inconsistency, only a disagreement between two independent claims.

Neither direction can speak to block withholding. Every child witness is sealed
into the block before it is found, so nothing inside the block testifies to
when it was published. `event_discovered_at` does not answer it either: it is
the wall clock at ingestion, so it is close to first observation only for live
polling, and is the import date for backfills and historical publications.

Historical parent-coinbase evidence is also lossless. Structured full
transactions populate the normal txid, script, and serialized-output fields,
while the complete transaction bytes and normalized publication output text
remain attached to the event. The text field preserves address, value/script,
and raw scriptPubKey forms that cannot be represented as a value-complete
`Vec<TxOut>`.

## Read-Model Rules

- Derived rows are written through `mmm-read-model` mutation entry points.
- Historical base snapshots enqueue parents transactionally. Primary parent
  rebuilds commit one at a time, store their changed-hash seeds transactionally,
  and delete queue work only after the dependent cascade succeeds.
- A transient classifier `unknown` never demotes a previously proven canonical
  or stale row.
- Bad evidence is removed with explicit event revocation, then the read model
  recomputes the affected parent state.
- Bitcoin Core backbone rows are written by `sync-bitcoin-core` and are required
  for tree windows the UI should browse.

## Migrations

Public migration history starts with:

- `0001_canonical_schema.sql` - squashed baseline schema.
- `0002_seed_sources.sql` - generated source seed for fresh databases.

Later schema changes are appended as new numbered forward migrations.

`0007_support_partial_child_evidence.sql` makes child evidence nullable, adds
authenticated child header and `nBits` storage, replaces the old composite
identity with exact and partial unique indexes, and adds historical publication
provenance. Existing event values are preserved by the migration. The migration
fails before altering the schema if legacy rows contain duplicate exact
`(source_id, child_block_hash)` identities; see the audit query in
`docs/operations.md`.

After a migration has reached a persistent database, do not edit it. Add a new
forward migration. Real database migration runs go through `just db-migrate-dev`
or `just db-migrate-deploy`, both of which use the backup-first wrapper in
`scripts/migrate-safe.sh`.
