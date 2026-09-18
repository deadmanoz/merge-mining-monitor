# Configuration

The monitor is configured with environment variables. `just` loads `.env`
automatically via `set dotenv-load`; the binary itself does not.

## Per-Chain Variables

Prefixes: `NAMECOIN`, `RSK`, `SYSCOIN`, `FRACTAL`, `HATHOR`, `ELASTOS`, `QBIT`, `TERRACOIN`.

| Variable | Applies to | Contract |
|---|---|---|
| `<PREFIX>_RPC_URL` | all | Endpoint. Required for Namecoin, RSK, Syscoin, Fractal, Qbit, and Terracoin; defaults exist for Hathor and Elastos. |
| `<PREFIX>_RPC_USER` / `<PREFIX>_RPC_PASSWORD` | all but Hathor | Auth policy is chain-specific and pinned by tests. Set both unless that chain explicitly allows unauthenticated access or cookie auth. |
| `<PREFIX>_RPC_COOKIEFILE` | Syscoin, Fractal, Qbit | Bitcoin Core-style `user:password` cookie file used when the user/password pair is unset. |
| `<PREFIX>_RPC_TIMEOUT_SECS` | all | Whole-request HTTP timeout, default 15 seconds. |
| `<PREFIX>_START_HEIGHT` | all | Explicit live cursor seed override. Use once for first deploy or controlled reset, then remove. |
| `<PREFIX>_POLL_INTERVAL_SECONDS` | all | Live tick interval, default 30 seconds. |
| `<PREFIX>_BATCH_SIZE` | all | Maximum number of new heights the cursor advances per tick (default 100); a tick attempts up to `reorg_depth + batch_size` heights in total. |
| `<PREFIX>_REORG_DEPTH` | all | Trailing rescan window in blocks: each tick re-processes this many heights ending at and including the persisted cursor, then advances the cursor by up to the batch size, so the window ends at the tip only once the cursor has caught up. A rescanned height whose block changed marks the earlier event displaced (see `docs/data-model.md`, Child Displacement); every live producer except RSK records it. The compiled defaults are 0 except RSK and Terracoin (64), and Hathor (20); a deployment sets the depth per chain. For Hathor, a depth above 32 rescans that far, but the displacement floor stays bounded at a 32-block fork, so a replacement across a deeper fork may be captured without being recorded as the chain's block. |
| `<PREFIX>_MAX_BACKFILL_RANGE` | Hathor, Elastos | Backfill range cap. |
| `<PREFIX>_ALLOW_LARGE_BACKFILL` | Hathor, Elastos | Exact `"1"` boolean to lift the range cap. |
| `<PREFIX>_RPC_BACKFILL_DELAY_MS` | Hathor, Elastos | Per-height backfill delay. |

Chain-specific extras:

| Variable | Contract |
|---|---|
| `HATHOR_RPC_FALLBACK_URL` | Optional fallback REST endpoint; an empty value disables fallback. |
| `HATHOR_BACKFILL_SKIP_HOLDS` | Exact `"1"` boolean to count absent/transient holds as logged skips during backfill. Core-cache-horizon holds still stop the run. |
| `RSK_BACKFILL_FETCH_CONCURRENCY` | Bounded prefetch width, default 16, clamped to at least 1. |

## Shared Variables

| Variable | Purpose |
|---|---|
| `PGHOST` / `PGPORT` / `PGUSER` / `PGPASSWORD` / `PGDATABASE` | Postgres connection. |
| `BITCOIN_RPC_URL` / `BITCOIN_RPC_USER` / `BITCOIN_RPC_PASSWORD` | Required Bitcoin **mainnet** Core classifier and header source for capture, import, and reconciliation commands, including database-only maintenance modes. Each command refreshes the persisted Core-header cache through the synced tip before work begins. |
| `BITCOIN_RPC_TIMEOUT_SECS` / `BITCOIN_RPC_MAX_CONCURRENCY` | Bitcoin Core client controls. Transient transport failures and brief Bitcoin Core warmup responses use five attempts with capped exponential backoff; node readiness remains a deployment prerequisite. Other RPC, authentication, decoding, and integrity errors are not retried. |
| `BITCOIN_CORE_SYNC_FOLLOW_INTERVAL_SECS` | Follow-mode poll interval in seconds for `sync-bitcoin-core --follow`. Default 60; must be greater than 0. |
| `BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS` | Near-tip reorg repair window in follow mode. Default 64; must be at least 16 (the tree's default live-tip window). Also the maximum automatically repaired reorg depth. |
| `BITCOIN_CORE_SYNC_DELAY_MS` | Optional per-height RPC throttle during Core sync, in milliseconds. Default 0. |
| `SERVE_BIND_ADDR` / `SERVE_DB_POOL_SIZE` / `SERVE_WWW_DIR` | Read API and static frontend serving. |
| `MMM_POOLS_DIR` | Optional local `bitcoin-data/mining-pools/pools` checkout used by `just gen-pool-snapshot` when no path argument is provided. |
| `MERGE_MINING_RESEARCH_DIR` | Local `merge-mining-research` checkout at the pinned publication commit, used by `import-all`, `import-dataset`, and manifest generation. |

See `.env.example` for a complete starter file.
