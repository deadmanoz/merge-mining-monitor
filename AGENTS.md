# Merge Mining Monitor

Postgres-backed Rust service for collecting Bitcoin stale-block attribution evidence from merge-mined AuxPoW child chains, live Bitcoin Core observations, and recovered historical datasets.

Human-facing project documentation lives in `docs/`; start with `docs/README.md`.
Task-specific agent references live in `agent_docs/`; start with `agent_docs/README.md`.

## Build And Test

Use `just` targets, not raw commands, when a target exists:

- `just build` / `just test` / `just test-integration` / `just lint` / `just format` - build, fast tests, serial Postgres integration tests (advisory locks are not schema-isolated), clippy + `arch-lint`, and rustfmt.
- `just db-up` / `just db-migrate-dev` / `just db-migrate-deploy` / `just db-backup` - local DB and backup-first migration workflow.
- `just serve` - read API plus static `www/` frontend.
- `just poll-CHAIN` / `just backfill-CHAIN START END` - chain capture for `namecoin`, `rsk`, `syscoin`, `fractal`, `hathor`, `elastos`, `qbit`, and `terracoin`.
- `just import-known-stales` / `just reclassify-known-stales` - known-stale membership import and retroactive demotion.
- `just import-all` / `just import-dataset CHAIN` / `just gen-research-publication-pins` - pinned historical publication import and Research manifest + error-catalogue pin refresh.
- `just reclassify-unknown-parents`, `just reclassify-pools`, `just reconcile-read-model` - repair and enrichment commands.

## Architecture Rules

- **Crate ownership:** `mmm-pg` opens connections, `mmm-capture` owns offline parsing/resolution, `mmm-rpc` owns HTTP transport, `mmm-bitcoin-core` is the only Core RPC linker, `mmm-store` writes producer base tables, `mmm-read-model` writes derived tables, `mmm-producers` owns engines, and `mmm-api` serves read-only HTTP views without importing producer internals.
- **Base vs derived tables (`docs/data-model.md`):** Producers write only `merge_mining_event`, `child_block_observation` (with its `child_miner_identity` dimension), 1:1 chain sidecars, attribution rows, and operational state (`poll_cursor`, `poll_pending_reconcile`, `capture_error`, and `child_chain_head`, pruned to each source's rescan window) through `mmm-store`; historical ingest attaches `historical_event_provenance` and operators import `known_stale_block`.
  `block`, `attestation_proof`, and `source_health` are derived exclusively through `mmm-read-model`.
- **Error catalogue (`data/consensus/error_blocks.csv`):** A PoW-valid match (including reviewed body-invalid parents) is an `error_block` with `error_block_reason`, never stale or orphan evidence.
  Refresh both the historical manifest and error catalogue together via `just gen-research-publication-pins`.
- **Bitcoin lineage gate (`docs/capture.md`):** Every stored parent must be a Bitcoin header decided solely by `mmm_capture::lineage::bitcoin_lineage` via `mmm_read_model::capture_in_txn` (with strict/weak orphan verdicts as its projection).
  Never add per-producer lineage or `nBits` checks.
- **Optional child evidence:** Treat child height, hash, header, time, and `nBits` as independent optional fields; never store scan counters, placeholder hashes, parent timestamps, or zeroes for unavailable child evidence.
  Xaya and ROD use `ChildTargetLocation::PowData` (zero pure-header `nBits` and an external target from the pinned Research publication), and Hathor strict BIP34 evidence requires a full parent coinbase transaction validated by `mmm_capture::btc_orphan`.
- **Historical imports (`docs/historical-ingest.md`):** Manifest-backed imports replace Research provenance across lifecycles: Historical and Partial sources remove omitted events while Live sources retain their events, proofs, and attribution (preserving operator and error-observation provenance, and importing changed error witnesses before snapshot cleanup).
  `import-all` compares normalized publication state against stored rows before taking the Core cache lock, reuses compatible proven `block` classifications, and drains `historical_reconcile_queue` in bounded parent transactions.
- **Bitcoin Core follow mode and lock order (`docs/data-model.md`):** Follow mode atomically replaces bounded divergent suffixes (near-tip or at a lagged cursor), retains displaced blocks as stale evidence, and drains `bitcoin_core_reconcile_queue` before cache refresh or later sync work.
  Core-backed classifiers and reconcilers take the shared header-cache lock before the shared canonical-view barrier; suffix replacement takes the shared cache lock before the exclusive canonical barrier; ordinary canonical writers take only the exclusive canonical barrier.
- **AuxPoW family extension (`docs/capture.md`, `docs/attribution.md`):** Never copy a sibling chain module to add a Namecoin-family source; extend the shared source registry, chain spec, config, AuxPoW-family parser, poller, and write paths.
  Classic full-block authentication lives in `mmm_capture::auxpow`; Qbit full-block capture authenticates the child body merkle root and coinbase height before child tag and witness-v2/32-byte `qb` payout attribution (keeping child tags, child payouts, and Bitcoin-parent attribution independent, with optional per-entry payout confidence applied at bootstrap).
- **Byte order:** Store `rust-bitcoin` `to_byte_array()` bytes directly in Postgres; use display/RPC hex only at presentation boundaries.
- **Migrations and generated artifacts:** SQL migrations are append-only via `just db-migrate-dev` or `just db-migrate-deploy` (except regenerating the `0002` fresh/reset seed alongside an idempotent forward migration when adding a source; see `migrations/README.md`).
  Never hand-edit `data/pools/current.json`, `www/js/source-registry.generated.js`, or `www/js/findings.generated.js`.

## Remote Round Trips And Batch Work

Production round-trip latency to Bitcoin Core, child-chain nodes, and the research VM is ~340 ms (see `agent_docs/round-trip-budget.md` and `agent_docs/testing.md`).
Any loop over rows, heights, or candidates that performs a remote call or per-item SQL statement must:

- State a `Round-trip budget:` line in the PR description (`none` when adding no remote or per-item work) for any producer, store, or read-model change.
- Skip remote calls when local tables (`block`, `child_chain_head`) already hold the answer, batch where transport allows, and bound per-tick call counts.
- Run bounded or cursor-resumable work only (never unbounded work in startup or per-tick refresh), releasing global advisory locks between batches.
- Report progress and transport counters via `ProgressReporter` and `RpcMetrics`.
- Pin remote-call counts at the transport boundary in tests, time remote loops at ~340 ms latency, and verify per-item SQL loops with `EXPLAIN` against production row counts.

## Repository Etiquette

- Keep changes scoped to the requested work and use a dedicated worktree for non-trivial changes unless asked otherwise.
- Land every change to `main` through a PR with required checks passing and a `Round-trip budget:` line when touching producer, store, or read-model crates.
- Commit only when explicitly requested, using conventional commit messages without AI attribution.
- Fix `just arch-lint` failures by refactoring, never by relaxing thresholds or adding allowlists.

## Documentation

Human-facing documentation is indexed in `docs/README.md` and task-specific agent docs in `agent_docs/README.md`.
When API fixtures change, update `fixtures/api/manifest.json`, `docs/api-contract.md`, and `crates/merge-mining-monitor/tests/api_fixture_contract.rs` together (for a release bump, regenerating `fixtures/api/version.json` via `just gen-source-artifacts` is sufficient because the contract test asserts deep equality against `mmm_api::version_payload_json`).
