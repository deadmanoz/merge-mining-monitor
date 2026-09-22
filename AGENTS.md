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
  `namecoin`, `rsk`, `syscoin`, `fractal`, `hathor`, `elastos`, and `qbit`.
- `just import-known-stales` / `just reclassify-known-stales` - known-stale
  membership import and retroactive demotion.
- `just import-all` / `just import-dataset CHAIN` - pinned normalized
  historical publication import.
- `just gen-research-publication-pins` - refresh the Research manifest and error catalogue from
  one committed revision, manifest first.
- `just reclassify-unknown-parents`, `just reclassify-pools`,
  `just reconcile-read-model` - repair and enrichment commands.

Database-backed test binaries run serially in `just test-integration` and CI
because schema isolation does not isolate PostgreSQL advisory locks. Preserve
concurrent tasks inside each locking test.

## Architecture Rules

- The workspace is split by ownership: `mmm-pg` opens connections,
  `mmm-capture` owns offline parsing/resolution, `mmm-rpc` owns HTTP transport,
  `mmm-bitcoin-core` is the only Core RPC linker, `mmm-store` writes producer
  base tables, `mmm-read-model` writes derived tables, `mmm-producers` owns
  engines, and `mmm-api` serves read-only HTTP views.
- `data/consensus/error_blocks.csv` is a pinned compact mirror of the research
  catalogue. A proof-of-work-valid match is an `error_block`, never stale or
  orphan evidence; reconciliation persists its catalogue height and rejection
  reason in the derived `block` row. Refresh it with the historical manifest
  via `just gen-research-publication-pins`; the
  manifest consumes Research's canonical observation-chain inventory.
- Producers write only `merge_mining_event` plus 1:1 chain sidecars,
  attribution rows, and their own operational state through `mmm-store`:
  `poll_cursor`, `poll_pending_reconcile`, `capture_error` (one row per height
  a producer could not capture; written before the failing height returns,
  cleared only when that same height is reprocessed successfully, and
  projected by `/api/v1/sources` as the earliest unresolved height), and
  `child_chain_head` (the block a chain last carried at each processed
  height, written in the capture transaction, read only by the producer's own
  trailing rescan, never by the read model or the API). Historical ingest
  also attaches `historical_event_provenance`. The operator imports
  `known_stale_block` through `import-known-stales`. Reviewed body-invalid
  parents use the error catalogue; migration `0029` removes the obsolete
  annotation table.
  Body-invalid verdicts use `kind=error_block` and `error_block_reason`; retired
  annotation fields and compatibility types are removed. `block`,
  `attestation_proof`, and `source_health` are derived through
  `mmm-read-model`.
- Treat child height, hash, header, time, and `nBits` as independent optional
  evidence. Never store a scan counter, placeholder hash, parent timestamp, or
  zero in place of unavailable child evidence.
- Historical and partial source imports are authoritative snapshots. Live
  source publication imports are additive. Import changed error witnesses before
  ordinary snapshot cleanup to preserve events moving out of stale inventories.
  Keep this lifecycle distinction in
  the shared source registry, not in per-chain schema branches.
- The current Research pin is generated from committed revision `e6dc40a` and
  covers 29 event artifacts plus the stale-descendant and error-observation
  aggregates, 31 artifacts and 1,286,512 rows in total. Refresh both pins
  (manifest and error catalogue) with
  `just gen-research-publication-pins`; a refreshed pin documents import
  readiness, not a completed database import or deploy.
- Historical describes the recovered dataset, not whether its native chain is
  still active. ROD has no live Monitor producer. The registry's
  `ChildTargetLocation` also owns the target contract: Xaya and ROD use
  `PowData`, with zero pure-header `nBits` and a non-zero effective target
  supplied by the pinned Research publication. The importer checks parent work
  against that target; the pure header alone cannot authenticate it.
- Hathor strict BIP34 evidence requires a full parent coinbase transaction
  whose input script matches the retained script. Live capture validates and
  stores that transaction; legacy script-only observations remain weaker
  until normal replay/import enriches them. Historical import, writer and API
  height selection share the validator in `mmm-capture::btc_orphan`.
- `import-all` determines work by comparing normalized publication-owned fields
  with non-operator historical provenance and base events across research pins.
  Artifact SHA values verify bytes only. A complete match must return before
  taking the Bitcoin Core cache lock; pending derived work takes the lock and
  finalizes without replaying source rows. For a changed artifact, reuse a
  compatible Core-attested canonical or structurally complete stale
  classification already proven in `block`;
  unknown, absent, publication-incompatible, and dedicated error-observation
  state must still use strict live Core classification.
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
  new migration; do not edit historical migrations. The documented exception
  is the registry-generated `0002` fresh/reset seed: regenerate it when adding
  a source and also add an idempotent forward migration for existing databases
  (see `migrations/README.md`).
- Real database migrations go only through `just db-migrate-dev` or
  `just db-migrate-deploy`.
- Never hand-edit generated runtime artifacts such as `data/pools/current.json`,
  `www/js/source-registry.generated.js`, or `www/js/findings.generated.js`;
  regenerate them through the documented `just` targets.

## Remote Round Trips And Batch Work

Production runs far from its data: the round trip from the production host to
Bitcoin Core, to every child-chain node, and to the research VM is about
340 ms, while the development VM sees well under 1 ms to the same hosts. A
loop that makes one remote call per row costs a thousand times more in
production than anywhere it is tested, and three production incidents came
from exactly that shape: a call per item that nothing had budgeted, bounded,
or timed. Any operation that iterates over rows, heights, or candidates and
performs a remote call or a per-item statement must:

- State its round-trip budget in the PR description: expected items times
  remote round trips per item, at the production round-trip time. Every PR
  that changes a producer, store, or read-model crate carries a
  `Round-trip budget:` line, `none` when it adds no remote or per-item work,
  so a reviewer never has to guess whether the author looked; a PR without
  one is incomplete.
- Make no remote call for an item whose answer a local table already holds
  (the `block` table holds every canonical Bitcoin height; the child-chain
  head record holds the block the chain last carried at a height), and batch
  calls where the transport allows it. A call per item is acceptable only
  where it is irreducible (a height's block hash at its node, a candidate's
  strict Core classification) and the budget, the bound, and the timing
  receipt below justify the count; on a path something waits on, such as a
  tick, the count is the window the budget states and no more.
- Be bounded, or resumable from a cursor persisted in the database, and
  never run unbounded work inside producer startup or a per-tick refresh.
  Scheduled work is recorded as pending and consumed by an explicit job.
- Hold no global advisory lock across more than one batch. A batch is
  processed under the lock; the lock is released before the next batch.
- Log progress through `ProgressReporter` (done, total, rate, ETA at a
  fixed interval, and a `job ended` summary with each RPC client's
  `RpcMetrics` line whether the job finished or aborted). A live poller
  reports the same counters as per-tick deltas on its `poll tick` line.
- Carry a test that pins its remote-call count per item at the transport
  boundary (an `RpcMetrics` snapshot, the scripted Core fixture in
  `docs/testing.md`), not at a fake classifier's call count.
- Be timed at production round-trip latency before it ships, with the
  receipt (items, round trips, wall time) in the PR (`docs/testing.md`).

The last two apply to remote calls. A loop whose per-item cost is a database
statement (Postgres runs beside the service, so the cost is the statement's
plan times its count, the shape of the quadratic `reclassify-pools` RSK scan
in issue #23) states statements per item in its budget, meets the bound,
lock, and progress requirements as written, and is reviewed by its plan
(`EXPLAIN` against production row counts) rather than by a counter: the
counters and the timing recipe do not cover statements today.

Red on any of these is fixed by restructuring the operation, never by an
allowlist or a larger timeout.

## Repository Etiquette

- Keep changes scoped to the requested work.
- For non-trivial implementation work, use a dedicated worktree unless the user
  explicitly says to work in the current checkout.
- Land every change to `main` through a pull request with the required checks
  passing. Do not push commits directly to `main`. A PR that changes a
  producer, store, or read-model crate carries a `Round-trip budget:` line,
  `none` when it adds no remote or per-item work
  (see Remote Round Trips And Batch Work).
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
