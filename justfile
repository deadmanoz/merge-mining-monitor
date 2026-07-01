# merge-mining-monitor justfile
#
# Project automation targets, see README.md for phase status. Quality gates
# assume a working Rust toolchain (stable, ≥1.88) on PATH.

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
# See scripts/migrate-safe.sh.
db-migrate-dev:
    ./scripts/migrate-safe.sh dev

# Deploy mode uses the same automatic restore behavior; remote deployment
# scripts call this explicit target.
db-migrate-deploy:
    ./scripts/migrate-safe.sh deploy

# Convenience local default. Production deployment scripts call db-migrate-deploy.
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
    ./scripts/test-historical-source-manifest.sh
    ./scripts/gen-historical-source-manifest.sh --check --allow-missing-repo
    ./scripts/live-test-deployment.sh self-check

test-integration: db-up
    cargo test -p merge-mining-monitor --features db-integration --test db_integration
    cargo test -p merge-mining-monitor --features db-integration --test api_db_integration

test-e2e:
    npm run test:e2e

lint:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets --all-features -- -D warnings
    ./scripts/arch-lint.sh

# Mechanical architecture gates: file-size budgets + jscpd duplication
# (scripts/arch-lint.sh, .jscpd.json). The structural clippy lints
# (too_many_lines, self_named_module_files) are denied workspace-wide via
# [workspace.lints.clippy], so every clippy run enforces them; the script
# gates also run inside `lint`. This target stays independently invokable
# and adds the advisory cognitive_complexity pass (nursery lint, metric
# churns across toolchains, so advisory -W only, never a hard gate). Red is
# fixed by refactoring, never by raising a threshold or allowlisting.
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
    ./scripts/gen-historical-source-manifest.sh --check --allow-missing-repo

# Regenerate data/pools/current.json from a clean upstream
# bitcoin-data/mining-pools clone. Pin --generated-at for byte-for-byte
# reproduction. Pass POOLS_DIR explicitly, or set MMM_POOLS_DIR locally.
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

# Regenerate (or --check) the registry-derived artifacts from src/source_registry:
# the source-seed SQL (migrations/0002_seed_sources.sql) and the frontend metadata
# (www/js/source-registry.generated.js). `--check` is the drift gate (also run by
# `cargo test`).
gen-source-artifacts *args="":
    cargo run --quiet --features artifact-generation --bin gen_source_artifacts -- {{args}}

# Regenerate (or --check) data/historical/historical-source-manifest.json from the local
# merge-mining-research validated stale CSVs for the historical-only chains.
gen-historical-source-manifest *args="":
    ./scripts/gen-historical-source-manifest.sh {{args}}

serve:
    cargo run -- serve

# Parameterized producer recipes: one pair serves every chain; the registry
# dispatch in the binary resolves poll-<chain>/backfill-<chain> from the
# ChainSpec table. The per-chain names below are one-line delegations kept as
# the documented operator interface; adding a chain needs no new recipe.
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

backfill-namecoin start end: (backfill "namecoin" start end)

backfill-rsk start end: (backfill "rsk" start end)

backfill-syscoin start end: (backfill "syscoin" start end)

backfill-fractal start end: (backfill "fractal" start end)

backfill-hathor start end: (backfill "hathor" start end)

backfill-elastos start end: (backfill "elastos" start end)

backfill-hathor-cache csv *args:
    cargo run -- backfill-hathor-cache {{csv}} {{args}}

import-dataset chain *args:
    cargo run -- import-dataset {{chain}} {{args}}

reclassify-unknown-parents *args:
    cargo run -- reclassify-unknown-parents {{args}}

# Offline historical pool re-resolution. Self-seeds the expanded pool snapshot
# and child identity registries, then re-attributes merge_mining_event pool IDs
# from stored coinbase columns plus persisted child-chain identity fields.
# Default is fill-NULL-only; pass --overwrite to re-attribute already-set
# registry-backed pool IDs. Never writes NULL over known attribution.
#
# Phase gates (run only what changed; the RSK pass scans the whole RSK corpus, so
# skip it unless the RSK miner registry changed):
#   --only <rsk|main|hathor|elastos>   run exactly one phase
#   --skip-<rsk|main|hathor|elastos>   drop one phase (mutually exclusive with --only)
#   --source auxpow:<chain>            bound the main scan to one source (full source code)
# Runbook (which pass to run): docs/attribution.md.
reclassify-pools *args:
    cargo run -- reclassify-pools {{args}}

# Populate the canonical Bitcoin header backbone from Bitcoin Core. The tree API
# reads this data only and returns a 409 when the requested window is unsynced.
sync-bitcoin-core *args:
    cargo run -- sync-bitcoin-core {{args}}

reconcile-read-model *args:
    cargo run -- reconcile-read-model {{args}}

# Rebuild the source_health per-source rollup counters from base tables (the
# authoritative recompute behind /api/v1/sources). Run after fresh database
# setup and after bulk backfills; sets source_health_ready so /sources serves.
rebuild-source-health:
    cargo run -- reconcile-read-model --rebuild-source-health

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
