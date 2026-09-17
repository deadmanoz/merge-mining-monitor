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

## Frontend Tests

Playwright tests exercise the static frontend against stubbed or live API
responses. Start `just serve` on a free local port when a test needs the real
server, set `PLAYWRIGHT_BASE_URL`, and stop the server afterward.
