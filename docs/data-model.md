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

`child_block_time` is the child block's own timestamp, taken from whatever the
chain commits: the header `nTime` for Namecoin-family chains, the RSK block
timestamp, the Hathor block transaction timestamp. It records when the pool
last committed that child template into the Bitcoin coinbase. It is not the
time the child block was broadcast, and it is not a second opinion on when the
Bitcoin block was found.

The invariant that makes this true is format-neutral: every AuxPoW scheme
commits child data into the Bitcoin coinbase, so the child stamp is fixed
before the Bitcoin work exists and cannot be refreshed when the proof is
finally submitted to the child network. Changing it by one second changes the
committed child data, the coinbase, the Bitcoin merkle root, and voids the
proof of work.

The Namecoin-family form of that chain is the one the stored `aux_proof`
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
RSK discards the coinbase under RSKIP-92, and Hathor uses the RFC 0006 split
header (see `docs/capture.md`). The sealing invariant above still holds for
both.

Two asymmetric reading rules follow, and both assume the stamp is
authenticated. Live capture derives it from the committed child data. A
historical import may carry it from the publication's own column with no child
header to authenticate it against, because `validate_child_bundle` only
compares a supplied timestamp when a header is present. Treat an unauthenticated
historical stamp as the publisher's claim, not as decoded evidence.

- **A negative offset from the Bitcoin header time is re-commitment age, not
  lateness.** Every refresh of a child template costs a new Bitcoin job, so
  pools re-commit fast chains continuously and slow chains about once per job,
  reusing one child header across many jobs. A chain committed at job start
  carries an offset of roughly minus the Bitcoin block interval, which is a
  property of Bitcoin's luck rather than of the block, the pool, or the child
  chain. This is an inference from template cadence, not a measurement, and the
  magnitude depends on the pool's own refresh policy. Do not derive a verdict
  about a Bitcoin timestamp from it.
- **A positive offset is an internal inconsistency worth looking at.** Template
  cadence can only push a child stamp earlier, and the Bitcoin block provably
  contains the child data, so a child stamp later than the Bitcoin stamp means
  the producing pool stamped the two inconsistently. That is not automatically
  a clock fault: child chains accept headers some distance into the future, so
  a pool may stamp ahead deliberately and still have the block accepted. Read
  it as a flag on the pool's own pair of stamps, never as proof against an
  external clock, since the same operator sets both.

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
