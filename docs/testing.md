# Testing

The project has fast Rust tests, Postgres-backed integration tests, API fixture
checks, and Playwright frontend smoke tests.

## Commands

| Command | Scope |
|---|---|
| `just build` | Build the workspace. |
| `just test` | Workspace Rust tests plus manifest and shell self-checks. |
| `just test-integration` | Compose Postgres plus DB/API integration test binaries. |
| `just lint` | `cargo fmt --check`, clippy with warnings denied, and architecture lint. |
| `just arch-lint` | File-size, duplication, and advisory complexity gates. |
| `just test-e2e` | Playwright smoke tests under `e2e/`. |

## Fixtures

- `fixtures/namecoin/` - raw block bytes plus expected JSON sidecars.
- `fixtures/syscoin/` - real raw Syscoin Core block samples.
- `fixtures/rsk/` - RSKj block and uncle JSON responses.
- `fixtures/fractal/` - Fractal AuxPoW and child-block samples.
- `fixtures/hathor/` - Hathor REST transaction samples.
- `fixtures/elastos/` - Elastos RPC and AuxPoW samples.
- `fixtures/api/` - shared API examples listed in `fixtures/api/manifest.json`.

API fixtures are contract examples, not exhaustive endpoint tests. Endpoint and
route tests cover behavior; fixture tests keep examples parseable and
manifested.

## Integration Tests

DB integration tests create isolated schemas, apply migrations, and tear down
even when test bodies fail. Keep tests that assert table layout close to direct
SQL seed helpers; use scenario helpers when the behavior should flow through
production mutation paths.

## Frontend Tests

Playwright tests exercise the static frontend against stubbed or live API
responses. Start `just serve` on a free local port when a test needs the real
server, set `PLAYWRIGHT_BASE_URL`, and stop the server afterward.
