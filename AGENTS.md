# Merge Mining Monitor

Postgres-backed Rust service for collecting Bitcoin stale-block attribution
evidence from merge-mined AuxPoW child chains, live Bitcoin Core observations,
and recovered historical datasets.

Human-facing project documentation lives in `docs/`; start with
`docs/README.md`.

## Build And Test

Use `just` targets, not raw commands, when a target exists:

- `just build` - build the workspace.
- `just test` - fast workspace tests and lightweight script checks.
- `just test-integration` - compose Postgres plus DB/API integration tests.
- `just lint` - `cargo fmt --check`, clippy, and architecture lint.
- `just format` - format Rust code.
- `just db-up` / `just db-migrate-dev` / `just db-migrate-deploy` /
  `just db-backup` - local DB and backup-first migration workflow.
- `just serve` - read API plus static `www/` frontend.
- `just poll-CHAIN` / `just backfill-CHAIN START END` - chain capture for
  `namecoin`, `rsk`, `syscoin`, `fractal`, `hathor`, and `elastos`.
- `just reclassify-unknown-parents`, `just reclassify-pools`,
  `just reconcile-read-model` - repair and enrichment commands.

## Architecture Rules

- The workspace is split by ownership: `mmm-pg` opens connections,
  `mmm-capture` owns offline parsing/resolution, `mmm-rpc` owns HTTP transport,
  `mmm-bitcoin-core` is the only Core RPC linker, `mmm-store` writes producer
  base tables, `mmm-read-model` writes derived tables, `mmm-producers` owns
  engines, and `mmm-api` serves read-only HTTP views.
- Producers write only `merge_mining_event` plus 1:1 chain sidecars and
  attribution rows. `block`, `attestation_proof`, and `source_health` are
  derived through `mmm-read-model`.
- Do not copy a sibling chain module to add a Namecoin-family source. Extend
  the shared source registry, chain spec, config, AuxPoW-family parser, poller,
  and write paths.
- `crates/mmm-api/` must not import producer internals. Cross-layer data needs
  an explicit shared boundary type or API.
- Hash byte order is fixed: store rust-bitcoin `to_byte_array()` bytes directly;
  use display/RPC hex only at presentation boundaries.
- SQL migrations are append-only after they reach a persistent database. Add a
  new migration; do not edit historical migrations.
- Real database migrations go only through `just db-migrate-dev` or
  `just db-migrate-deploy`.
- Never hand-edit generated runtime artifacts such as `data/pools/current.json`
  or `www/js/source-registry.generated.js`; regenerate them through the
  documented `just` targets.

## Repository Etiquette

- Keep changes scoped to the requested work.
- For non-trivial implementation work, use a dedicated worktree unless the user
  explicitly says to work in the current checkout.
- Land every change to `main` through a pull request with the required checks
  passing. Do not push commits directly to `main`.
- Commit only when explicitly requested.
- Commit messages use conventional format and must not include AI attribution.
- `just arch-lint` red is fixed by refactoring, not by relaxing thresholds or
  adding allowlists.

## Documentation

- `docs/architecture.md` - system structure and crate boundaries.
- `docs/data-model.md` - schema, read model, migrations, and classification.
- `docs/capture.md` - live and historical source capture model.
- `docs/attribution.md` - pool attribution and child identity registries.
- `docs/configuration.md` - environment variables.
- `docs/operations.md` - local operation and deployment workflow.
- `docs/historical-ingest.md` - recovered historical AuxPoW imports.
- `docs/testing.md` - test surfaces and fixtures.
- `docs/release-versioning.md` - version source of truth and release flow.
- `docs/api-contract.md`, `docs/product-brief.md`, `docs/ui-model.md` -
  public API, product, and UI contracts.
- `docs/tree-semantics.md` - implementation notes for deriving `/api/v1/tree`
  and orphan navigator responses (compact context, orphan placement, tree
  reduction).

When API fixtures change, update `fixtures/api/manifest.json`,
`docs/api-contract.md`, and
`crates/merge-mining-monitor/tests/api_fixture_contract.rs` together.
