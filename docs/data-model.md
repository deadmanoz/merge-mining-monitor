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

`child_block_time` is the child block's own claimed timestamp as its chain
reports it: the header `nTime` for Namecoin-family chains, the RSK block
timestamp, the Hathor block transaction timestamp. Whoever builds the child
template writes it. It is not a broadcast time, not a capture time, and not a
second opinion on when the Bitcoin block was found.

It is fixed once set, whatever the AuxPoW format. The child data is committed
into the Bitcoin coinbase before the Bitcoin work exists, so altering the stamp
would void the proof of work; it cannot be refreshed when the proof is later
submitted to the child chain.

How far a stamp is evidence depends on the row, not on its source. The first
two levels come apart in both directions, so check them separately:

- `child_header_hex` present: the stamp is re-derivable from stored bytes. The
  normalized publication contract carries this column and the importer
  authenticates it, so an imported row is re-derivable even though no import
  writes an AuxPoW proof.
- `aux_proof` present: the Bitcoin block is proven to have committed to the
  child data. Live Namecoin-family capture and Elastos carry this. Elastos
  verifies the commitment without storing a header, so a missing header does
  not imply an unproven stamp.
- Neither: the value is the source's reported number. RSK and Hathor capture
  sit here, RSK permanently, because its proof format discards the parent
  coinbase.

An imported row is therefore proof-less, not evidence-less: it is re-derivable
but nothing shows the Bitcoin side committed to it.

Two reading rules follow, both bounded by the same fact: the database holds two
claimed timestamps and nothing else, and both are miner-set.

- A NEGATIVE offset is the ordinary case and is not lateness. A child header is
  reused across successive Bitcoin jobs, so it is stamped earlier than the
  parent that finally carries it. The magnitude reflects the refresh and
  stamping policy behind each stamp, and the two need not share an operator,
  since a Bitcoin pool may proxy child-chain operation through another (see
  `docs/attribution.md`). Do not derive a verdict about a Bitcoin timestamp
  from it.
- A POSITIVE offset is an ordering disagreement worth investigating. Where the
  commitment was verified, the disagreement is internal to the block; otherwise
  it says only that the source's reported child time exceeds the Bitcoin header
  time. It is never proof that either number is wrong.

Neither direction can speak to block withholding. Every child stamp is sealed
before the block is found, so nothing inside the block records when it was
published. `event_discovered_at` does not answer it either: it is never
advanced after the first write. For an event the backfill or import created it
is therefore the import date rather than a first-observation time; where live
capture inserted the event first and an import only refined it, the value stays
the earlier live-ingestion time.

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
