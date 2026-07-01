#!/usr/bin/env bash
# migrate-safe.sh - apply Postgres migrations with an automatic backup so a
# failed migration can be rolled back without manual archaeology.
#
# Required by the global database-safety.md rule. ALL migration runs must go
# through this script — never invoke a raw migration tool against a real DB.
#
# Modes:
#   dev          backup -> migrate -> on failure, auto-restore from backup
#   deploy       backup -> migrate -> on failure, auto-restore from backup
#   backup-only  pg_dump only, no migration
#
# Connection comes from PGHOST/PGPORT/PGUSER/PGPASSWORD/PGDATABASE in the
# environment (set by the justfile or your shell).
#
# Migrations are plain .sql files under migrations/, applied in lexical order.
# Applied migrations are tracked in a schema_migrations(version TEXT PRIMARY KEY,
# applied_at TIMESTAMPTZ NOT NULL DEFAULT now()) table, created on first run.

set -euo pipefail

MODE="${1:-dev}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MIGRATIONS_DIR="${REPO_ROOT}/migrations"
BACKUPS_DIR="${REPO_ROOT}/backups"
TIMESTAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BACKUP_FILE="${BACKUPS_DIR}/${PGDATABASE:-mmm}-${TIMESTAMP}.sql.gz"

: "${PGHOST:?must be set}"
: "${PGPORT:?must be set}"
: "${PGUSER:?must be set}"
: "${PGDATABASE:?must be set}"
# PGPASSWORD is read by psql/pg_dump; tolerate it being unset for trust auth.

mkdir -p "${BACKUPS_DIR}"

log() { printf '[migrate-safe] %s\n' "$*" >&2; }

restore_command() {
    printf "gunzip -c '%s' | sed '/^SET transaction_timeout/d' | psql --set ON_ERROR_STOP=on" "${BACKUP_FILE}"
}

backup() {
    log "pg_dump -> ${BACKUP_FILE}"
    pg_dump --no-owner --no-privileges --clean --if-exists \
        | gzip -9 > "${BACKUP_FILE}"
    log "backup size: $(du -h "${BACKUP_FILE}" | awk '{print $1}')"
    log "restore command: $(restore_command)"
}

restore() {
    log "restoring from ${BACKUP_FILE}"
    gunzip -c "${BACKUP_FILE}" \
        | sed '/^SET transaction_timeout/d' \
        | psql --set ON_ERROR_STOP=on
}

ensure_migrations_table() {
    psql --set ON_ERROR_STOP=on --quiet <<'SQL'
CREATE TABLE IF NOT EXISTS schema_migrations (
    version    TEXT        PRIMARY KEY,
    applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
SQL
}

pending_migrations() {
    local applied
    applied="$(psql --no-psqlrc --tuples-only --no-align \
        -c 'SELECT version FROM schema_migrations ORDER BY version' \
        | sed '/^$/d')"
    local file version
    for file in "${MIGRATIONS_DIR}"/*.sql; do
        [ -e "$file" ] || continue
        version="$(basename "$file" .sql)"
        if ! grep -Fxq "$version" <<<"$applied"; then
            printf '%s\n' "$file"
        fi
    done
}

apply_migrations() {
    local file version
    while IFS= read -r file; do
        [ -z "$file" ] && continue
        version="$(basename "$file" .sql)"
        log "applying ${version}"
        psql --set ON_ERROR_STOP=on --single-transaction \
            -v migration_version="${version}" \
            -f "$file" \
            -c "INSERT INTO schema_migrations(version) VALUES ('${version}')"
    done < <(pending_migrations)
}

run_migrations() {
    ensure_migrations_table \
        && apply_migrations
}

case "${MODE}" in
    backup-only)
        backup
        ;;
    dev|deploy)
        backup
        if ! run_migrations; then
            log "migration failed, auto-restoring (${MODE} mode)"
            restore
            log "restored. migration aborted."
            exit 1
        fi
        log "migrations applied successfully."
        ;;
    *)
        echo "usage: $0 {dev|deploy|backup-only}" >&2
        exit 2
        ;;
esac
