# Testing

The project has fast Rust tests, Postgres-backed integration tests, API fixture
checks, and Playwright frontend smoke tests.

## Commands

| Command | Scope |
|---|---|
| `just build` | Build the workspace. |
| `just test` | Fast workspace tests (`cargo test --workspace`) plus historical-manifest, research-pin, and live-test-deployment self-checks. Does not enable `db-integration`. |
| `just test-integration` | Compose Postgres plus the `db_integration` and `api_db_integration` binaries (serial). |
| `just lint` | `cargo fmt --check`, clippy with warnings denied, and `scripts/arch-lint.sh`. |
| `just arch-lint` | Architecture lint plus advisory `clippy::cognitive_complexity`. |
| `just test-e2e` | Playwright smoke tests under `e2e/`. Not run in CI. |

CI (`.github/workflows/ci.yml`) runs fmt, clippy, and architecture lint as
separate jobs, then a test job that applies migrations and runs
`cargo test --workspace --all-features -- --test-threads=1` plus the
publication-artifact script checks. That workspace test includes the
`db-integration` feature. CI does not run `just test-e2e` or
`scripts/live-test-deployment.sh`.

## Fixtures

- `fixtures/namecoin/` - raw block bytes plus expected JSON sidecars.
- `fixtures/syscoin/` - real raw Syscoin Core block samples.
- `fixtures/rsk/` - RSKj block and uncle JSON responses.
- `fixtures/fractal/` - Fractal AuxPoW and child-block samples.
- `fixtures/hathor/` - Hathor REST transaction samples.
- `fixtures/elastos/` - Elastos RPC and AuxPoW samples.
- `fixtures/qbit/` - Qbit mainnet extended-header controls and a native
  synthetic-parent proof.
- `fixtures/xaya/` - pinned Xaya publication row used to check PowData
  parent-work acceptance (zero header `nBits`, non-zero effective target).
- `fixtures/rod/` - pinned ROD publication sample for the same PowData
  target contract.
- `fixtures/api/` - shared API examples listed in `fixtures/api/manifest.json`.
- `crates/mmm-bitcoin-core/src/parent_classifier/core_fixture.rs` - a
  scripted Bitcoin Core JSON-RPC server built from a canned header chain, so
  the production classifier runs in unit tests and its request pattern is
  pinned through the client's `RpcMetrics` snapshot.

API fixtures are contract examples, not exhaustive endpoint tests. Endpoint and
route tests cover behavior; fixture tests keep examples parseable and
manifested.

## Integration Tests

DB integration tests create isolated schemas, apply migrations, and tear down
even when test bodies fail. Keep tests that assert table layout close to direct
SQL seed helpers; use scenario helpers when the behavior should flow through
production mutation paths. The DB-backed test binaries run one test at a time:
PostgreSQL advisory locks are database-wide, so separate schemas do not isolate
the Core-cache barrier or its timing assertions. Concurrent tasks inside each
test still exercise the production locking behavior.

## Round-Trip Budgets

Every remote client (the Bitcoin Core client in `mmm-bitcoin-core` and the
child-chain clients over the `mmm-rpc` transport) owns an `RpcMetrics`
handle that counts dispatched HTTP attempts, JSON-RPC elements, retries,
failures, and latency at the transport boundary, readable in tests through
the client's `metrics().snapshot()`. An attempt is one dispatched request,
counted at dispatch and timed through its interpreted response (or until
the caller abandons it), and a window's mean latency is over the attempts
that completed in it; a retry and a failure belong to the client's retry
loop, and a failure is a call that gave up. The scripted Core RPC fixture
(`crates/mmm-bitcoin-core/src/parent_classifier/core_fixture.rs`) serves
`getblockhash`, `getblockheader`, and `getblock` from a canned header chain
so the production classifier can be exercised, and counted, for the
canonical, stale-competitor, and median-time-past cases. Tests that pin a
remote-call count per item use these counters; `FakeParentClassifier`'s call
count measures how often the classifier is invoked, not how many requests it
makes, and is kept for that purpose only.

`strict_import_classification_rpc_budget` pins the strict aggregate-import
classification profile at 15 requests per uncached parent with a locally known
predecessor, or 18 without one, including the eleven-header MTP walk. It covers
ten distinct parents per profile and asserts both client and server counters.
Run its optional latency measurement with
`MMM_TEST_RPC_DELAY_MS=340 cargo test -p mmm-bitcoin-core strict_import_classification_rpc_budget -- --nocapture`.
On 2026-09-22, the local scripted transport measured 150 requests in 52.172
seconds and 180 requests in 62.537 seconds respectively, with no retries.
Each profile recorded ten expected Core not-found responses. This measures
classification with a 340 ms response delay, not a complete database import or
real Core block-download throughput. A changed 49-parent aggregate can classify
all 49 parents again: its conservative no-retry bound is 882 requests, about
300 seconds of round-trip latency, before database and other import work.

Batch operations log progress through `ProgressReporter` and end, finished
or aborted, with a `job ended` summary and one line per RPC client. Before a
change to a batch operation ships, it is timed at the production round-trip
time: the development VM with that latency injected on traffic to Core and
the child nodes, against the production-copy database (the rehearsal target
that automates this is planned for the next release; until it lands, the
timing is taken by hand on that VM). The receipt (items, round trips, wall
time) goes in the PR beside its `Round-trip budget:` line.

## Frontend Tests

Playwright tests exercise the static frontend against stubbed or live API
responses. Start `just serve` on a free local port when a test needs the real
server, set `PLAYWRIGHT_BASE_URL`, and stop the server afterward.
