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
| `merge_mining_event` | Append-only source evidence keyed by source, child block, and Bitcoin parent header. |
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
`block.btc_orphan_class`, set only after a Core-absence-attested verdict and
the offline strict/weak orphan classifier.

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

After a migration has reached a persistent database, do not edit it. Add a new
forward migration. Real database migration runs go through `just db-migrate-dev`
or `just db-migrate-deploy`, both of which use the backup-first wrapper in
`scripts/migrate-safe.sh`.
