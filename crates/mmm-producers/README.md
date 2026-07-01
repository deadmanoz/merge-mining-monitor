# mmm-producers

The ingest and maintenance engines the CLI dispatches: the chain registry and its
producers, the live poller, the Bitcoin Core backbone sync, the historical dataset
ingest, and the repair/reattribution commands. This is the only workspace crate
that combines child-chain RPC with database writes.

## The invariant

`mmm-producers` owns no base-table or derived-table SQL of its own. It calls
`mmm-store` helpers with data (which turn it into base-table SQL) and routes every
derived-state mutation through `mmm-read-model`'s `read_model::mutation` entry
points. Producers write only `merge_mining_event` and its 1:1 chain sidecars;
`block`, `attestation_proof`, and `source_health` are reconciler-derived. Live
poll progress is the monotonic `poll_cursor` table, never
`MAX(child_height)`, and backfills never move the cursor.

A new merge-mined chain is a `chains::spec` row plus a shared-family extension,
never a cloned sibling module. A bitcoind-family chain (Namecoin, Syscoin,
Fractal) is a spec row served by the shared `chains::auxpow_family` runner over
the shared `chains::bitcoind_rpc` client, with no capture module of its own. A genuinely
divergent chain (RSK, Hathor, Elastos) additionally gets one module dir under
`chains/`; the crate-root command facade dispatches to those modules by
`ChainId`. Chain-specific repair and report code lives beside the rest of that
chain's code under `chains/<chain>/`.

## Layout

`lib.rs` is the crate-root dispatch layer. In normal builds the public producer
API consists of the crate-root CLI command facades, Bitcoin Core backbone sync,
historical ingest, pool reclassification, and runtime helpers the binary
dispatches. The producer
implementation modules, the spec table module, and the generic poller are
crate-internal in normal builds and are reopened only by the `db-integration`
feature for integration tests that intentionally exercise parser and state
machine details.

Cross-cutting modules:

| Module | Responsibility |
|--------|----------------|
| `chains::spec` | The static `CHAINS` table: per chain a `ChainId`, env prefix, activation floor, poller/reorg defaults, optional `FamilySpec` (auth, fetch strategy, repair scope), and an optional `RangeCap`. The producer command facade resolves a spec row, then dispatches by `ChainId`. |
| `chains::config` | The only env-reading module under `chains/`: spec-driven poller and per-chain RPC config, each chain's auth and parsing contract pinned by tests. |
| `chains::auxpow_family` + `chains::bitcoind_rpc` | The shared bitcoind-family capture/poll/backfill runner and the one thin JSON-RPC client serving Namecoin, Syscoin, and Fractal. |
| `poller` | The crate-internal `ChainPoller` trait and the generic `Poller<C>` driver: cursor seeding, the trailing rescan window, batch advance, startup read-model repair, shutdown, and bounded tip-fetch retry. |
| `bitcoin_core_backbone` | The durable Bitcoin spine sync (one-shot batch plus the follow daemon), the live-tip window maintenance, and the structural integrity guards in `integrity.rs`. |
| `producer_runtime` | The shared runtime: `ProducerRuntime` (`PG*` + `BITCOIN_RPC_*`), the composed `ProducerContext`, `connect_from_env`, the post-backfill repair hook, and the classifier-enabled-backfill warning. |
| `historical_ingest` | The CSV-backed historical stale-block dataset ingest, with zero public-API calls. |

Per-chain divergent modules under `chains/`:

| Module | Responsibility |
|--------|----------------|
| `chains::rsk` | RSKj `eth_*` capture with canonical-plus-uncle traversal and RLP child headers, plus the DB-only miner-identity reclassification tail (`identity_reresolve`). |
| `chains::hathor` | Public-REST capture with reconstructed coinbase and nBits-horizon hold semantics, the cache-backed historical ingest, and the reward-address replay (`reward_replay`). |
| `chains::elastos` | Dual-endpoint self-verifying capture with RPC-observed reward and minerinfo identities. |

Cross-chain repair code stays at the crate root: `reclassify_pools` and
`reclassify_child_payout`.

## See also

- `docs/capture.md` - the shared runners, poll-cursor semantics, and per-chain parser/verification model.
- `docs/architecture.md` - the workspace crate map and dependency-enforced invariants.
- `docs/data-model.md` - the derived-table mutation boundary producers route through.
