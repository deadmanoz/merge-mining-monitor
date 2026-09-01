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
- `just import-known-stales` / `just reclassify-known-stales` - known-stale
  membership import and retroactive demotion.
- `just import-all` / `just import-dataset CHAIN` - pinned normalized
  historical publication import.
- `just gen-research-publication-pins` - refresh both Research pins from one
  committed revision, manifest first.
- `just reclassify-unknown-parents`, `just reclassify-pools`,
  `just reconcile-read-model` - repair and enrichment commands.

## Architecture Rules

- The workspace is split by ownership: `mmm-pg` opens connections,
  `mmm-capture` owns offline parsing/resolution, `mmm-rpc` owns HTTP transport,
  `mmm-bitcoin-core` is the only Core RPC linker, `mmm-store` writes producer
  base tables, `mmm-read-model` writes derived tables, `mmm-producers` owns
  engines, and `mmm-api` serves read-only HTTP views.
- `data/consensus/error_blocks.csv` is a pinned compact mirror of the research
  catalogue. A proof-of-work-valid match is an `error_block`, never stale or
  orphan evidence; reconciliation persists its catalogue height and rejection
  reason in the derived `block` row. Refresh it and the historical manifest
  together via `just gen-research-publication-pins`; the manifest consumes
  Research's canonical observation-chain inventory.
- Producers write only `merge_mining_event` plus 1:1 chain sidecars and
  attribution rows. Historical ingest also attaches
  `historical_event_provenance`. The further base table,
  `known_stale_block`, is operator-imported via `import-known-stales`
  (written through `mmm-store`, never by capture producers). `block`,
  `attestation_proof`, and `source_health` are derived through
  `mmm-read-model`.
- Treat child height, hash, header, time, and `nBits` as independent optional
  evidence. Never store a scan counter, placeholder hash, parent timestamp, or
  zero in place of unavailable child evidence.
- Historical and partial source imports are authoritative snapshots. Live
  source publication imports are additive. Keep this lifecycle distinction in
  the shared source registry, not in per-chain schema branches.
- `import-all` determines work by comparing normalized publication-owned fields
  with non-operator historical provenance and base events across research pins.
  Artifact SHA values verify bytes only. A complete match must return before
  taking the Bitcoin Core cache lock; pending derived work takes the lock and
  finalizes without replaying source rows.
- Historical base/provenance writes enqueue affected parents in the same
  transaction. Drain `historical_reconcile_queue` in bounded parent
  transactions and retain changed-hash seeds until dependent cascades succeed;
  never hold every parent advisory lock across a chain import.
- Bitcoin Core follow mode repairs bounded near-tip reorgs and divergent
  lagged cursors. Capture a tip-pinned backward header view and, when needed, a
  second bounded view ending at the persisted cursor. Replace only the complete
  divergent suffix atomically, retain displaced blocks as stale evidence, and
  enqueue every affected old and new hash in `bitcoin_core_reconcile_queue`
  before commit. Core-backed
  classifiers and reconcilers take the shared header-cache lock before the
  shared canonical-view barrier. Suffix replacement takes the shared cache lock
  before the canonical barrier exclusively; ordinary canonical-row writers and
  sync bookkeeping take only the exclusive canonical barrier. Drain the queue's
  durable parent and expansion phases before cache refresh or later sync work,
  and fail closed when no common ancestor exists inside the configured window.
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
- Never hand-edit generated runtime artifacts such as `data/pools/current.json`,
  `www/js/source-registry.generated.js`, or `www/js/findings.generated.js`;
  regenerate them through the documented `just` targets.

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

A release version bump is the one exception to the test half of that rule.
`fixtures/api/version.json` is asserted deep-equal to the payload the code
actually serves (`mmm_api::version_payload_json`), so regenerating the fixture
from `/api/v1/version` is sufficient and the contract test needs no edit; it is
what catches a fixture that was not regenerated. Hand-editing per-release
expectations into that test would defeat the guard.
