# merge-mining-monitor justfile
#
# Project automation targets. Quality gates assume a working Rust toolchain
# (stable, ≥1.88) on PATH.

set shell := ["bash", "-uc"]
set dotenv-load := true

# Postgres connection (override via env if needed)
export PGHOST     := env_var_or_default("PGHOST", "localhost")
export PGPORT     := env_var_or_default("PGPORT", "55432")
export PGUSER     := env_var_or_default("PGUSER", "mmm")
export PGPASSWORD := env_var_or_default("PGPASSWORD", "mmm")
export PGDATABASE := env_var_or_default("PGDATABASE", "mmm")

default:
    @just --list

# ─── Database lifecycle (docker compose v2) ──────────────────────────────────

db-up:
    docker compose up -d db
    @echo "Waiting for Postgres to accept connections…"
    @until docker compose exec -T db pg_isready -U "$PGUSER" -d "$PGDATABASE" >/dev/null 2>&1; do sleep 1; done
    @echo "Postgres is up."

db-down:
    docker compose down

db-logs:
    docker compose logs -f db

db-psql:
    docker compose exec -it db psql -U "$PGUSER" -d "$PGDATABASE"

# Apply all pending migrations with an automatic backup and failure restore.
db-migrate-dev:
    ./scripts/migrate-safe.sh dev

db-migrate-deploy:
    ./scripts/migrate-safe.sh deploy

db-migrate: db-migrate-dev

db-backup:
    ./scripts/migrate-safe.sh backup-only

# Destructive local reset for disposable databases; backs up before removing the Postgres Docker volume.
db-reset:
    @if [ "${CONFIRM_DB_RESET:-}" != "1" ]; then \
        echo "error: db-reset removes the Postgres Docker volume." >&2; \
        echo "Run CONFIRM_DB_RESET=1 just db-reset only for disposable local databases." >&2; \
        exit 2; \
    fi
    just db-backup
    docker compose down -v
    @echo "Postgres Docker volume removed. Run 'just db-up && just db-migrate-dev' to recreate it."

# ─── Quality gates ───────────────────────────────────────────────────────────

build:
    cargo build

test:
    cargo test --workspace
    just check-data-artifacts
    ./scripts/live-test-deployment.sh self-check

test-integration: db-up
    cargo test -p merge-mining-monitor --features db-integration --test db_integration -- --test-threads=1
    cargo test -p merge-mining-monitor --features db-integration --test api_db_integration -- --test-threads=1

test-e2e: build
    npm run test:e2e

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    ./scripts/arch-lint.sh

# File-size + jscpd duplication gates plus advisory cognitive_complexity pass.
arch-lint:
    ./scripts/arch-lint.sh
    cargo clippy --workspace --all-targets --all-features -- -W clippy::cognitive_complexity

format:
    cargo fmt --all

# Local deterministic checks for committed release-domain data under data/.
check-data-artifacts:
    find data -type f -name '*.json' -print0 | xargs -0 jq empty
    just gen-source-artifacts --check
    ./scripts/test-historical-source-manifest.sh
    ./scripts/test-gen-research-publication-pins.sh
    ./scripts/gen-research-publication-pins.sh --check --allow-missing-repo

# Regenerate data/pools/current.json from bitcoin-data/mining-pools (POOLS_DIR or $MMM_POOLS_DIR).
gen-pool-snapshot *args="":
    @set -- {{args}}; \
    pools_dir="${MMM_POOLS_DIR:-}"; \
    if [ "$#" -gt 0 ] && [ "${1#-}" = "$1" ]; then \
        pools_dir="$1"; \
        shift; \
    fi; \
    if [ -z "$pools_dir" ]; then \
        echo "error: set MMM_POOLS_DIR or pass the upstream pools dir as the first argument" >&2; \
        exit 2; \
    fi; \
    cargo run --features artifact-generation --bin gen_pool_snapshot -- "$pools_dir" "$@"

# Regenerate (or --check) migrations/0002_seed_sources.sql, www/js/source-registry.generated.js, www/js/findings.generated.js, and fixtures/api/version.json.
gen-source-artifacts *args="":
    cargo run --quiet --features artifact-generation --bin gen_source_artifacts -- {{args}}

# Refresh the Research manifest and invalid-blocks catalogue from one commit.
gen-research-publication-pins *args="":
    ./scripts/gen-research-publication-pins.sh {{args}}

gen-historical-source-manifest *args="":
    ./scripts/gen-historical-source-manifest.sh {{args}}

gen-invalid-blocks-catalogue *args="":
    ./scripts/gen-invalid-blocks-catalogue.sh {{args}}

serve:
    cargo run -- serve

# Parameterized producer recipes and per-chain operator aliases.
poll chain:
    cargo run -- poll-{{chain}}

backfill chain start end:
    cargo run -- backfill-{{chain}} {{start}} {{end}}

poll-namecoin: (poll "namecoin")
poll-rsk: (poll "rsk")
poll-syscoin: (poll "syscoin")
poll-fractal: (poll "fractal")
poll-hathor: (poll "hathor")
poll-elastos: (poll "elastos")
poll-qbit: (poll "qbit")
poll-terracoin: (poll "terracoin")

backfill-namecoin start end: (backfill "namecoin" start end)
backfill-rsk start end: (backfill "rsk" start end)
backfill-syscoin start end: (backfill "syscoin" start end)
backfill-fractal start end: (backfill "fractal" start end)
backfill-hathor start end: (backfill "hathor" start end)
backfill-elastos start end: (backfill "elastos" start end)
backfill-qbit start end: (backfill "qbit" start end)
backfill-terracoin start end: (backfill "terracoin" start end)

backfill-hathor-cache csv *args:
    cargo run -- backfill-hathor-cache {{csv}} {{args}}

import-dataset chain *args:
    cargo run -- import-dataset {{chain}} {{args}}

import-all *args:
    cargo run -- import-all {{args}}

import-known-stales *args:
    cargo run -- import-known-stales {{args}}

reclassify-known-stales *args:
    cargo run -- reclassify-known-stales {{args}}

reclassify-unknown-parents *args:
    cargo run -- reclassify-unknown-parents {{args}}

reclassify-parent hash:
    cargo run -- reclassify-parent {{hash}}

# Offline historical pool re-resolution (--overwrite, --only/--skip-<phase>, --source; see docs/attribution.md).
reclassify-pools *args:
    cargo run -- reclassify-pools {{args}}

# Populate the canonical Bitcoin header backbone from Bitcoin Core.
sync-bitcoin-core *args:
    cargo run -- sync-bitcoin-core {{args}}

reconcile-read-model *args:
    cargo run -- reconcile-read-model {{args}}

# Rebuild source_health rollup counters from base tables and set source_health_ready.
rebuild-source-health:
    cargo run -- reconcile-read-model --rebuild-source-health

# Rebuild the CORE/exSat protocol commitment tables from stored coinbase outputs and record the decoder version.
rebuild-protocol-commitments *args:
    cargo run -- reconcile-read-model --rebuild-protocol-commitments {{args}}

revoke-merge-mining-event event_id reason:
    cargo run -- revoke-merge-mining-event {{event_id}} "{{reason}}"

restore-merge-mining-event event_id:
    cargo run -- restore-merge-mining-event {{event_id}}

# ─── Live test deployment helpers ────────────────────────────────────────────

live-test-init:
    ./scripts/live-test-deployment.sh init
live-test-preflight:
    ./scripts/live-test-deployment.sh preflight
live-test-capture-tips:
    ./scripts/live-test-deployment.sh capture-tips
live-test-baseline:
    ./scripts/live-test-deployment.sh baseline
live-test-progress:
    ./scripts/live-test-deployment.sh progress
live-test-backfill chain start end:
    ./scripts/live-test-deployment.sh backfill {{chain}} {{start}} {{end}}
live-test-backfill-next chain chunk:
    ./scripts/live-test-deployment.sh backfill-next {{chain}} {{chunk}}
live-test-classify *args:
    ./scripts/live-test-deployment.sh classify {{args}}
live-test-reconcile-all:
    ./scripts/live-test-deployment.sh reconcile-all
live-test-reconcile-missing:
    ./scripts/live-test-deployment.sh reconcile-missing
live-test-smoke:
    ./scripts/live-test-deployment.sh smoke
live-test-self-check:
    ./scripts/live-test-deployment.sh self-check
live-test-start service:
    ./scripts/live-test-deployment.sh start {{service}}
live-test-stop service:
    ./scripts/live-test-deployment.sh stop {{service}}
live-test-status:
    ./scripts/live-test-deployment.sh status

# ─── Cleanup ─────────────────────────────────────────────────────────────────

clean:
    docker compose down
    cargo clean
    rm -rf .tmp logs test-results playwright-report
