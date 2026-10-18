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
- `fixtures/terracoin/` - pre-activation, activation, five June refresh candidates and the August 2026 canonical control, with RPC provenance.
- `fixtures/qbit/` - Qbit mainnet extended-header controls and a native
  synthetic-parent proof, plus native full blocks 78058 and 135331 for child
  coinbase authentication and attribution.
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
real Core block-download throughput. The current error aggregate contains 121
witnesses for 53 parents. A changed aggregate can classify all 53 parents again:
its conservative no-retry bound is 954 requests, about 324 seconds of round-trip
latency, before database and other import work.

`terracoin_round_trips_are_pinned_at_the_transport_boundary` (the
`db_integration` `auxpow_family` module) drives the Terracoin capture path
through the real `BitcoindRpcClient` against a scripted endpoint and pins its
per-height cost at `RpcMetrics`: two requests for a new height (`getblockhash`
and raw `getblock`), one for an unchanged final rescan height, one for the
genesis check a poller start or backfill run makes, and no retries. A failed
rescan records `height_capture_failed`, and the next rescan takes the full
capture (two requests) and clears it. The per-tick `getblockcount` and the
shared Core cache refresh are outside this test. Time it with
`MMM_TEST_RPC_DELAY_MS=340 cargo test -p merge-mining-monitor --features db-integration --test db_integration terracoin_round_trips_are_pinned_at_the_transport_boundary -- --test-threads=1 --nocapture`
against the integration database. On 2026-09-25 the genesis check took 0.35
seconds, a new AuxPoW and a new non-AuxPoW height 0.81 and 0.72 seconds, each
unchanged rescan 0.36 seconds, a failed rescan 0.36 seconds and the capture
that cleared it 0.78 seconds, with no retries. This measures RPC round trips
through the real client at 340 ms, not database work or node throughput.

Batch operations log progress through `ProgressReporter` and end, finished
or aborted, with a `job ended` summary and one line per RPC client. Before a
change to a batch operation ships, it is timed at the production round-trip
time: the development VM with that latency injected on traffic to Core and
the child nodes, against the production-copy database (the rehearsal target
that automates this is planned for the next release; until it lands, the
timing is taken by hand on that VM). The receipt (items, round trips, wall
time) goes in the PR beside its `Round-trip budget:` line.

The Qbit scenario
`an_open_capture_error_takes_the_full_capture_despite_a_final_head` also uses
the real transport. It covers child coinbase attribution, legacy evidence
enrichment, malformed-body recovery and native hash rejection. Run it with
`MMM_TEST_RPC_DELAY_MS=340` using the same integration-test command above,
substituting that test name. On 2026-10-17 the dev VM measured one genesis
request in 0.343 seconds, recovered capture in two requests/0.806 seconds,
unchanged rescan in one request/0.460 seconds, and body recovery in two
requests/0.827 seconds, with no retries. These are scenario intervals,
including intervening DB/API checks, with a 340 ms fixture response delay.
They do not measure production block-download throughput.

Qbit attribution adds no child-node RPC calls. A recognized child tag adds
two local SQL statements per captured event: source-scoped attribution cleanup
and attribution upsert. Read-only production query plans on 2026-10-17 used
indexes beginning with `(event_id, side)` for cleanup and pool collapse;
the planner estimated roughly 23 million events and 25 million attribution
rows. No new scan across stored events or producer startup job is added.

Qbit payout attribution adds no node RPC calls. With the currently observed
single reward address, live capture adds one payout upsert and one source
cleanup DELETE; existing child-pool refreshes are unchanged. Stored-output
replay adds one payout upsert and uses the existing two child-pool refresh
UPDATEs per changed event, plus existing per-parent transaction and lock work.
The CLI's existing Core-cache startup is a separate cost. An unchanged replay
makes no attribution writes.

The main replay query now materializes its bounded candidate page before
loading attribution details. On 2026-10-18, read-only plans on the dev dataset
(planner estimates 23,257,826 events and 24,895,572 attribution rows) used
source and event/side indexes. Each of the three detail aggregates ran 500
times for a 500-event page, rather than 13,463 times before the change. A
same-statement comparison found identical values for all 500 returned rows.
Observed first-page execution was 4,248 ms before and 226 ms after, with cache
warming uncontrolled; this is not a complete replay or production benchmark.
Eligibility still depends on the remaining source rows. No index or additional
query was added.

The payout-aware Qbit transport scenario also passed with a 340 ms response
delay on 2026-10-18: genesis 1 request/342 ms, recovered capture 2/787 ms,
unchanged rescan 1/363 ms, repeated capture 2/745 ms and 2/742 ms, and body
recovery 2/752 ms, with no retries or failures. These scenario intervals include
database/API checks and do not measure production block-download throughput.

## Frontend Tests

Playwright tests exercise the static frontend against stubbed or live API
responses. Start `just serve` on a free local port when a test needs the real
server, set `PLAYWRIGHT_BASE_URL`, and stop the server afterward.
