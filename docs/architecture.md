# Architecture

`merge-mining-monitor` is a Rust/Postgres service that collects evidence about Bitcoin stale blocks from merge-mined AuxPoW child chains, live Bitcoin Core observations, and recovered historical datasets.
It turns heterogeneous source evidence into normalized base observations, then derives a read model for the API and frontend.

## System Flow

<p align="center">
  <img src="img/system-data-flow.png" width="920" alt="Three-band Merge Mining Monitor data flow from capture sources through base evidence, read-model reconciliation, and read-only serving" />
</p>

The diagram shows the same ownership model as the text below: producers capture base evidence, the read-model reconciler is the only writer of derived tables, and the API serves those derived projections without writing capture state.

```text
1. CAPTURE          offline parsers authenticate; producers write base tables
──────────────────────────────────────────────────────────────────────
   child-chain source ──> parser / verifier ─┬─> merge_mining_event
                                             │   chain sidecar tables
                                             │   event_pool_attribution
                                             └─> capture_error
                                                 (malformed proofs on hold-policy chains;
                                                  Terracoin height capture failures)

   historical publication ──> preflight / database-state comparison
                                  matching ──> no-op before Core lock
                                  mismatch ──> merge_mining_event
                                                historical_event_provenance
                                                historical_reconcile_queue

   operator import (import-known-stales) ──────> known_stale_block

2. RECONCILE        read-model rebuilds derived tables from base evidence
──────────────────────────────────────────────────────────────────────
   base tables ───────────────────────────────┐
   known_stale_block ─────────────────────────┼──> read-model ──> block
   Bitcoin Core (backbone + classifier) ──────┤    reconciler     attestation_proof
   bitcoin_core_reconcile_queue ──────────────┘    (known-stale   source_health
                                                     exclusion     protocol_commitment
                                                     gate)

3. SERVE            the API projects derived tables to the frontend
──────────────────────────────────────────────────────────────────────
   derived tables ──────┬─> axum API (`serve`) ──> static frontend in www/
   capture_error ───────┘   (source capture-error state)
```

1. **CAPTURE (`mmm-capture`, `mmm-producers`, `mmm-store`)**: Offline parsers authenticate child-chain proofs and the Bitcoin lineage gate refuses non-Bitcoin SHA-256 parents before storage (`docs/capture.md`).
   Producers and importers write only base evidence (`merge_mining_event`, sidecars, `event_pool_attribution`, `historical_event_provenance`, `known_stale_block`) plus producer operational state (`poll_cursor`, `capture_error`, `child_chain_head`) and durable reconcile queues (`docs/historical-ingest.md`).
2. **RECONCILE (`mmm-bitcoin-core`, `mmm-read-model`)**: `mmm-read-model` is the sole writer of derived tables (`block`, `attestation_proof`, `source_health`, and the `protocol_commitment` tables decoded from stored Bitcoin coinbase outputs).
   It combines base evidence, `known_stale_block` exclusion gates, the pinned error catalogue, and Bitcoin Core classification / suffix replacement, draining `historical_reconcile_queue` and `bitcoin_core_reconcile_queue` in bounded transactions (`docs/data-model.md`).
3. **SERVE (`mmm-api`)**: The read-only HTTP server projects the derived tables, `capture_error` status, and the persisted Core-header cache to `/api/v1/*` and serves the static `www/` frontend without calling Bitcoin Core RPC.

## Crates

| Crate | Role |
|---|---|
| `mmm-pg` | Postgres connection configuration. No domain SQL. |
| `mmm-capture` | Offline parsing, normalization, pool resolution, source registry, and the Bitcoin lineage rule with its nBits and orphan helpers. No network or database I/O in normal builds. |
| `mmm-rpc` | Shared HTTP transport policy for child-chain clients. |
| `mmm-bitcoin-core` | The only crate that links `corepc-client`; wraps Bitcoin Core RPC and parent classification. |
| `mmm-store` | SQL for producer base tables: events, sidecars, cursors, capture errors, and seed helpers. It also exposes the read-only Core-header-cache loader shared by reconciliation and the API. |
| `mmm-read-model` | Sole writer of derived tables: `block`, `attestation_proof`, `source_health`, and the `protocol_commitment` tables. Owns the shared capture seam (`capture_in_txn`) and its Bitcoin lineage gate. |
| `mmm-producers` | Runtime engines: chain pollers/backfills, historical importer, Bitcoin Core backbone sync, and pool reclassification. |
| `mmm-api` | Read-only API views plus static frontend serving. |
| `merge-mining-monitor` | CLI wiring, generator binaries, and cross-crate integration tests. |

## Repository Layout

```text
crates/            # Rust workspace members (see the crate table above)
data/              # committed release-domain data
                   #   (pools/, consensus/, sources/, findings/, historical/)
migrations/        # squashed schema baseline (0001_) plus generated source seed (0002_)
fixtures/          # shared JSON API fixtures and per-chain parser samples
www/               # static frontend served by the read API (index.html, css/, js/, vendor/, assets/)
docs/              # this documentation set
scripts/           # migrate-safe, research-pin and catalogue generators,
                   # historical-source manifest checks, arch-lint, live-test helper
compose.yaml       # Postgres 16 (docker compose v2)
justfile           # db, build, test, lint, serve, sync, poll, and backfill targets
```

## Boundaries

- Producers do not write derived tables.
- `mmm-api` does not import producer internals.
- `mmm-api` may read the persisted Core-header cache through `mmm-store`; it never calls Core RPC.
- Bitcoin Core access is isolated in `mmm-bitcoin-core`.
- Hash byte order follows rust-bitcoin newtypes: store `to_byte_array()` bytes directly and use display/RPC hex only at presentation boundaries.
- Adding a Namecoin-family source should extend the source registry and shared producer path, not clone a sibling chain module.

## Public Interfaces

- CLI commands are exposed by `crates/merge-mining-monitor/src/main.rs` and documented through `justfile`.
- Runtime API behavior is documented in `docs/api-contract.md`.
- Product and UI behavior live in `docs/ui-model.md`.
- Generated source metadata is owned by `crates/mmm-capture/src/source_registry` and emitted into `migrations/0002_seed_sources.sql` and `www/js/source-registry.generated.js`.
- The generated findings corpus is owned by `crates/mmm-capture/src/findings_registry/mod.rs`, which validates the hand-authored `data/findings/` files and emits `www/js/findings.generated.js`.
- Strict classic AuxPoW authentication is owned by `mmm-capture::auxpow`; the producer family spec independently selects RPC dialect, raw proof authentication, and capture-failure policy.
