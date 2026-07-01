# mmm-store

Producer-side base-table SQL for the merge-mining monitor. This crate owns every
producer-facing `INSERT`/`UPSERT`/maintenance statement against the append-only
*base* tables: `merge_mining_event` and its 1:1 chain sidecars, the
`event_pool_attribution` provenance vector, the `pool`/`pool_identity` seed rows,
and the `poll_cursor` live-progress table.

## The invariant

`mmm-store` writes base tables only. It never writes a derived table. The
reconciler-owned tables (`block`, `attestation_proof`, `source_health`) and the
reconciler-authorized lockstep mutations of `merge_mining_event` belong
exclusively to `mmm-read-model`, reachable only through its `read_model::mutation`
entry points. The workspace dependency graph enforces this boundary: producers
call `mmm-store` helpers with data,
`mmm-store` turns that data into base-table SQL, and the read model derives
everything else.

Because it is pure SQL over a caller-supplied Postgres client, the crate links
neither `corepc` (Bitcoin Core), `reqwest` (child-chain RPC), nor any chain
adapter. Its only dependencies are `mmm-pg` (connections), `mmm-capture` (offline
evidence types and the source-code constants), and the Postgres driver.

## Layout

`lib.rs` re-exports the stable public API (`mmm_store::fn`).
Shared, table-generic SQL lives in root modules; chain-specific SQL lives under
`chains/<chain>.rs`. A new merge-mined chain is a new `chains/<chain>.rs`, never
an append to one god file.

Shared, table-generic modules:

| Module | Responsibility |
|--------|----------------|
| `event` | `merge_mining_event` upserts, the `event_pool_attribution` provenance writes (with and without stale-attribution cleanup), and the NULL-preserving child-coinbase field fill. |
| `pool` | Pool snapshot upserts, the generic registry-only pool seeding, and the namespace `pool_identity` seeding and lookup helper. |
| `poll_cursor` | The `poll_cursor` live-progress table: source-id lookup, cursor load, and monotonic upsert (with optional observed target). Backfills never move the cursor. |
| `pending_reconcile` | The pending-reconcile work-queue rows: list, upsert, attempt-bump, revocation-reason retag, and delete. |

Per-chain modules under `chains/` (each chain's SQL in one place):

| Module | Responsibility |
|--------|----------------|
| `chains::rsk` | The RSK event + `rsk_merge_mining_evidence` sidecar capture writer, the RSK pool / `pool_identity` adapters over the `pool` helpers, and the `rsk_merge_mining_evidence.pool_identity_id` late-fill helper. |
| `chains::hathor` | The Hathor event + `hathor_merge_mining_evidence` sidecar capture writer, the per-height event read, and the DB-only reward-address replay loads / audit updates. |
| `chains::elastos` | The Elastos event-row-only capture writer (no sidecar; scoped revoke/reactivate) and the per-height active-event read. |

## See also

- `docs/architecture.md` - the workspace crate map and dependency-enforced invariants.
- `docs/data-model.md` - the derived-table mutation boundary that complements this crate.
- `docs/capture.md` - how chain producers call into mmm-store.
