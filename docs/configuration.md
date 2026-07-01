# Configuration

The monitor is configured with environment variables. `just` loads `.env`
automatically via `set dotenv-load`; the binary itself does not.

## Per-Chain Variables

Prefixes: `NAMECOIN`, `RSK`, `SYSCOIN`, `FRACTAL`, `HATHOR`, `ELASTOS`.

| Variable | Applies to | Contract |
|---|---|---|
| `<PREFIX>_RPC_URL` | all | Endpoint. Required for Namecoin, RSK, Syscoin, and Fractal; defaults exist for Hathor and Elastos. |
| `<PREFIX>_RPC_USER` / `<PREFIX>_RPC_PASSWORD` | all but Hathor | Auth policy is chain-specific and pinned by tests. Set both unless that chain explicitly allows unauthenticated access or cookie auth. |
| `<PREFIX>_RPC_COOKIEFILE` | Syscoin, Fractal | Bitcoin Core-style `user:password` cookie file used when the user/password pair is unset. |
| `<PREFIX>_RPC_TIMEOUT_SECS` | all | Whole-request HTTP timeout, default 15 seconds. |
| `<PREFIX>_START_HEIGHT` | all | Explicit live cursor seed override. Use once for first deploy or controlled reset, then remove. |
| `<PREFIX>_POLL_INTERVAL_SECONDS` | all | Live tick interval, default 30 seconds. |
| `<PREFIX>_BATCH_SIZE` | all | Per-tick height budget, default 100. |
| `<PREFIX>_REORG_DEPTH` | all but Elastos | Trailing rescan window. Elastos rejects this variable because the producer is monotonic. |
| `<PREFIX>_MAX_BACKFILL_RANGE` | Hathor, Elastos | Backfill range cap. |
| `<PREFIX>_ALLOW_LARGE_BACKFILL` | Hathor, Elastos | Exact `"1"` boolean to lift the range cap. |
| `<PREFIX>_RPC_BACKFILL_DELAY_MS` | Hathor, Elastos | Per-height backfill delay. |

Chain-specific extras:

| Variable | Contract |
|---|---|
| `HATHOR_RPC_FALLBACK_URL` | Optional fallback REST endpoint; an empty value disables fallback. |
| `HATHOR_BACKFILL_SKIP_HOLDS` | Exact `"1"` boolean to count absent/transient holds as logged skips during backfill. nBits table-horizon holds still stop the run. |
| `RSK_BACKFILL_FETCH_CONCURRENCY` | Bounded prefetch width, default 16, clamped to at least 1. |

## Shared Variables

| Variable | Purpose |
|---|---|
| `PGHOST` / `PGPORT` / `PGUSER` / `PGPASSWORD` / `PGDATABASE` | Postgres connection. |
| `BITCOIN_RPC_URL` / `BITCOIN_RPC_USER` / `BITCOIN_RPC_PASSWORD` | Optional Bitcoin Core classifier and backbone source. Unset disables parent classification for child-chain capture. |
| `BITCOIN_RPC_TIMEOUT_SECS` / `BITCOIN_RPC_MAX_CONCURRENCY` | Bitcoin Core client controls. |
| `SERVE_BIND_ADDR` / `SERVE_DB_POOL_SIZE` / `SERVE_WWW_DIR` | Read API and static frontend serving. |
| `MMM_POOLS_DIR` | Optional local `bitcoin-data/mining-pools/pools` checkout used by `just gen-pool-snapshot` when no path argument is provided. |
| `MERGE_MINING_RESEARCH_DIR` | Local `merge-mining-research` checkout used by historical manifest generation and import-dataset default CSV discovery. |
| `MERGE_MINING_ARCHIVE_DIR` | Optional local classified stale-block archive root used by import-dataset default CSV discovery. |

See `.env.example` for a complete starter file.
