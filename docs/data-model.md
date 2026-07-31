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
| chain sidecars | One-to-one evidence details for chains with extra structured data, such as RSK and Hathor. |
| `event_pool_attribution` | Attribution rows connecting an event to a pool with source/provenance details. |
| `poll_cursor` | Live poll progress. Backfills never move the cursor. |
| `block` | Derived Bitcoin parent block state: canonical, stale, near, or unknown. |
| `attestation_proof` | Derived proof rows supporting a block. |
| `source_health` | Per-source rollup counters for UI/API health reporting. |

## Parent Classification

`btc_parent_kind` is one of:

- `near` - parent header fails Bitcoin target validation.
- `unknown` - parent header passes target validation, but no Bitcoin-chain
  membership proof is available.
- `canonical` - Bitcoin Core proves the parent is on the active chain.
- `stale` - Bitcoin Core proves the parent is a valid off-chain Bitcoin block.

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

## Read-Model Rules

- Derived rows are written through `mmm-read-model` mutation entry points.
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
