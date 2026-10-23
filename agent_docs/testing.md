# Testing

The project has fast Rust tests, Postgres-backed integration tests, API fixture checks, and Playwright frontend smoke tests.

## Commands

| Command | Scope |
|---|---|
| `just build` | Build the workspace. |
| `just test` | Fast workspace tests (`cargo test --workspace`) plus historical-manifest, research-pin, and live-test-deployment self-checks. Does not enable `db-integration`. |
| `just test-integration` | Compose Postgres plus the `db_integration` and `api_db_integration` binaries (serial). |
| `just lint` | `cargo fmt --check`, clippy with warnings denied, and `scripts/arch-lint.sh`. |
| `just arch-lint` | Architecture lint plus advisory `clippy::cognitive_complexity`. |
| `just test-e2e` | Playwright smoke tests under `e2e/`. Not run in CI. |

CI (`.github/workflows/ci.yml`) runs fmt, clippy, and architecture lint as separate jobs, then a test job that applies migrations and runs `cargo test --workspace --all-features -- --test-threads=1` plus the publication-artifact script checks.
That workspace test includes the `db-integration` feature.
CI does not run `just test-e2e` or `scripts/live-test-deployment.sh`.

## Fixtures

- `fixtures/namecoin/` - raw block bytes plus expected JSON sidecars.
- `fixtures/syscoin/` - real raw Syscoin Core block samples.
- `fixtures/rsk/` - RSKj block and uncle JSON responses.
- `fixtures/fractal/` - Fractal AuxPoW and child-block samples.
- `fixtures/hathor/` - Hathor REST transaction samples.
- `fixtures/elastos/` - Elastos RPC and AuxPoW samples.
- `fixtures/terracoin/` - pre-activation, activation, five June refresh candidates and the August 2026 canonical control, with RPC provenance.
- `fixtures/qbit/` - Qbit mainnet extended-header controls and a native synthetic-parent proof, plus native full blocks 78058 and 135331 for child coinbase authentication and attribution.
- `fixtures/xaya/` - pinned Xaya publication row used to check PowData parent-work acceptance (zero header `nBits`, non-zero effective target).
- `fixtures/rod/` - pinned ROD publication sample for the same PowData target contract.
- `fixtures/api/` - shared API examples listed in `fixtures/api/manifest.json`.
- `crates/mmm-bitcoin-core/src/parent_classifier/core_fixture.rs` - a scripted Bitcoin Core JSON-RPC server built from a canned header chain, so the production classifier runs in unit tests and its request pattern is pinned through the client's `RpcMetrics` snapshot.

API fixtures are contract examples, not exhaustive endpoint tests.
Endpoint and route tests cover behavior; fixture tests keep examples parseable and manifested.

## Integration Tests

DB integration tests create isolated schemas, apply migrations, and tear down even when test bodies fail.
Keep tests that assert table layout close to direct SQL seed helpers; use scenario helpers when the behavior should flow through production mutation paths.
The DB-backed test binaries run one test at a time: PostgreSQL advisory locks are database-wide, so separate schemas do not isolate the Core-cache barrier or its timing assertions.
Concurrent tasks inside each test still exercise the production locking behavior.

`core_recovery_cli` also creates disposable databases and runs the actual child/Core producer binaries against loopback RPC fixtures. Database setup uses `just db-migrate-dev`, including its automatic backup, and teardown drops only the database the test created. These tests need local `just`, PostgreSQL client tools and permission to create databases. They exercise more than five failed cycles, automatic recovery, a tick longer than the heartbeat expiry, delayed status writes, startup/backoff/publication-lock shutdown, fatal authentication and held Hathor work. They deliberately use the production retry and heartbeat intervals.

Run `MMM_TEST_RPC_DELAY_MS=340 RUST_TEST_NOCAPTURE=1 just test-integration` to retain transport-count and latency receipts. The Core outage and cold-cache scenarios use that injected delay; the blackhole and long-tick scenarios use their own explicit delays. No production RPC endpoint is used by these scenarios.

## Round-Trip Budgets

See [`round-trip-budget.md`](round-trip-budget.md) for the ~340 ms production round-trip rules, `RpcMetrics` (`metrics().snapshot()`) transport-boundary counters, `core_fixture.rs` vs `FakeParentClassifier` test pinning, and `MMM_TEST_RPC_DELAY_MS=340` timing receipts.

## Frontend Tests

Playwright tests exercise the static frontend against stubbed or live API responses.
`just test-e2e` builds the workspace and launches `just serve` automatically via `playwright.config.js`'s `webServer` when `PLAYWRIGHT_BASE_URL` is unset; set `PLAYWRIGHT_BASE_URL` to point Playwright at an already-running external server instead.
