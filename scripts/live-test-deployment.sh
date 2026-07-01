#!/usr/bin/env bash
# Operator helpers for the local live test deployment.
#
# This script is deliberately thin: it wraps existing cargo subcommands,
# records journal/ledger state, and keeps long-running process logs predictable.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR="${REPO_ROOT}/.tmp/live-test-deployment"
LOG_DIR="${REPO_ROOT}/logs/live-test-deployment"
PID_DIR="${RUN_DIR}/pids"
JOURNAL="${RUN_DIR}/journal.md"
LEDGER="${RUN_DIR}/processed-ranges.csv"
TARGETS="${RUN_DIR}/target-tips.env"

usage() {
    cat <<'USAGE' >&2
usage: scripts/live-test-deployment.sh <command> [args...]

commands:
  init
  preflight
  capture-tips
  baseline
  progress
  backfill <namecoin|rsk|syscoin> <start> <end>
  backfill-next <namecoin|rsk|syscoin> <chunk-size>
  classify [args...]
  reconcile-all
  reconcile-missing
  smoke
  self-check
  start <serve|poll-namecoin|poll-rsk|poll-syscoin|poll-fractal|poll-hathor|poll-elastos|sync-bitcoin-core>
  stop <serve|poll-namecoin|poll-rsk|poll-syscoin|poll-fractal|poll-hathor|poll-elastos|sync-bitcoin-core>
  status
USAGE
}

log() {
    printf '[live-test] %s\n' "$*" >&2
}

die() {
    printf '[live-test] error: %s\n' "$*" >&2
    exit 1
}

timestamp() {
    date -u +"%Y-%m-%dT%H:%M:%SZ"
}

ensure_dirs() {
    mkdir -p "${RUN_DIR}" "${LOG_DIR}" "${PID_DIR}"
    if [ ! -f "${LEDGER}" ]; then
        printf 'chain,start,end,started_at,finished_at,exit_status,log_path\n' > "${LEDGER}"
    fi
    if [ ! -f "${JOURNAL}" ]; then
        {
            printf '# Live Test Deployment Journal\n\n'
            printf '%s\n' "- Created: $(timestamp)"
        } > "${JOURNAL}"
    fi
}

append_journal() {
    ensure_dirs
    printf '\n## %s\n\n%s\n' "$(timestamp)" "$*" >> "${JOURNAL}"
}

load_env() {
    cd "${REPO_ROOT}"
    if [ -f ".env" ]; then
        set -a
        # shellcheck disable=SC1091
        source ".env"
        set +a
    fi
    export PGHOST="${PGHOST:-localhost}"
    export PGPORT="${PGPORT:-55432}"
    export PGUSER="${PGUSER:-mmm}"
    export PGPASSWORD="${PGPASSWORD:-mmm}"
    export PGDATABASE="${PGDATABASE:-mmm}"
}

require_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required"
}

psql_query() {
    psql --no-psqlrc --tuples-only --no-align -c "$1"
}

chain_floor() {
    case "$1" in
        namecoin) printf '19200' ;;
        rsk) printf '139999' ;;
        syscoin) printf '1973' ;;
        *) die "unknown chain $1" ;;
    esac
}

chain_backfill_cmd() {
    case "$1" in
        namecoin) printf 'backfill-namecoin' ;;
        rsk) printf 'backfill-rsk' ;;
        syscoin) printf 'backfill-syscoin' ;;
        *) die "unknown chain $1" ;;
    esac
}

chain_poll_cmd() {
    case "$1" in
        poll-namecoin|poll-rsk|poll-syscoin|poll-fractal|poll-hathor|poll-elastos) printf '%s' "$1" ;;
        *) die "unknown poll service $1" ;;
    esac
}

target_var() {
    case "$1" in
        namecoin) printf 'NAMECOIN_TARGET_TIP' ;;
        rsk) printf 'RSK_TARGET_TIP' ;;
        syscoin) printf 'SYSCOIN_TARGET_TIP' ;;
        *) die "unknown chain $1" ;;
    esac
}

load_targets() {
    if [ -f "${TARGETS}" ]; then
        # shellcheck disable=SC1090
        source "${TARGETS}"
    fi
}

target_tip_for() {
    load_targets
    local var
    var="$(target_var "$1")"
    local value="${!var:-}"
    [ -n "${value}" ] || die "missing ${var}; run just live-test-capture-tips first"
    printf '%s' "${value}"
}

completed_resume_height() {
    local chain="$1"
    local floor
    floor="$(chain_floor "${chain}")"
    completed_resume_height_from_file "${LEDGER}" "${chain}" "${floor}"
}

completed_resume_height_from_file() {
    local ledger="$1"
    local chain="$2"
    local floor="$3"
    awk -F, -v wanted="${chain}" '
        NR > 1 && $1 == wanted && $6 == 0 { print ($2 + 0) "," ($3 + 0) }
    ' "${ledger}" 2>/dev/null \
        | sort -t, -k1,1n -k2,2n \
        | awk -F, -v resume="${floor}" '
            {
                start = $1 + 0
                end = $2 + 0
                if (start <= resume && end >= resume) {
                    resume = end + 1
                }
            }
            END { print resume }
        '
}

next_chunk_range_from_values() {
    local start="$1"
    local target="$2"
    local chunk="$3"
    if [ "${start}" -gt "${target}" ]; then
        printf 'done\n'
        return 0
    fi
    local end=$((start + chunk - 1))
    if [ "${end}" -gt "${target}" ]; then
        end="${target}"
    fi
    printf '%s,%s\n' "${start}" "${end}"
}

json_rpc() {
    local url="$1"
    local user="$2"
    local password="$3"
    local method="$4"
    local params="${5:-[]}"
    local curl_args=(-sS)
    if [ -n "${user}" ] || [ -n "${password}" ]; then
        [ -n "${user}" ] && [ -n "${password}" ] || die "RPC user/password must be set together for ${method}"
        curl_args+=(--user "${user}:${password}")
    fi
    curl_args+=(
        -H 'content-type: text/plain;'
        --data-binary "{\"jsonrpc\":\"1.0\",\"id\":\"mmm-live-test\",\"method\":\"${method}\",\"params\":${params}}"
        "${url}"
    )
    curl "${curl_args[@]}" \
        | jq -r 'if .error then error(.error.message) else .result end'
}

json_rpc_cookie() {
    local url="$1"
    local cookiefile="$2"
    local method="$3"
    local params="${4:-[]}"
    [ -f "${cookiefile}" ] || die "cookie file not found: ${cookiefile}"
    local cookie
    cookie="$(cat "${cookiefile}")"
    local user="${cookie%%:*}"
    local password="${cookie#*:}"
    json_rpc "${url}" "${user}" "${password}" "${method}" "${params}"
}

namecoin_rpc() {
    : "${NAMECOIN_RPC_URL:?NAMECOIN_RPC_URL is required}"
    json_rpc "${NAMECOIN_RPC_URL}" "${NAMECOIN_RPC_USER:-}" "${NAMECOIN_RPC_PASSWORD:-}" "$@"
}

syscoin_rpc() {
    : "${SYSCOIN_RPC_URL:?SYSCOIN_RPC_URL is required}"
    if [ -n "${SYSCOIN_RPC_USER:-}" ] || [ -n "${SYSCOIN_RPC_PASSWORD:-}" ]; then
        json_rpc "${SYSCOIN_RPC_URL}" "${SYSCOIN_RPC_USER:-}" "${SYSCOIN_RPC_PASSWORD:-}" "$@"
    elif [ -n "${SYSCOIN_RPC_COOKIEFILE:-}" ]; then
        json_rpc_cookie "${SYSCOIN_RPC_URL}" "${SYSCOIN_RPC_COOKIEFILE}" "$@"
    else
        json_rpc "${SYSCOIN_RPC_URL}" "" "" "$@"
    fi
}

rsk_rpc() {
    : "${RSK_RPC_URL:?RSK_RPC_URL is required}"
    local method="$1"
    local params="${2:-[]}"
    local curl_args=(-sS)
    if [ -n "${RSK_RPC_USER:-}" ] || [ -n "${RSK_RPC_PASSWORD:-}" ]; then
        [ -n "${RSK_RPC_USER:-}" ] && [ -n "${RSK_RPC_PASSWORD:-}" ] || die "RSK_RPC_USER and RSK_RPC_PASSWORD must be set together"
        curl_args+=(--user "${RSK_RPC_USER}:${RSK_RPC_PASSWORD}")
    fi
    curl_args+=(
        -H 'content-type: application/json'
        --data-binary "{\"jsonrpc\":\"2.0\",\"id\":\"mmm-live-test\",\"method\":\"${method}\",\"params\":${params}}"
        "${RSK_RPC_URL}"
    )
    curl "${curl_args[@]}" \
        | jq -r 'if .error then error(.error.message) else .result end'
}

hex_quantity_to_decimal() {
    local hex="${1#0x}"
    case "${hex}" in
        ''|*[!0-9a-fA-F]*) die "invalid hex quantity: $1" ;;
    esac
    printf '%d\n' "$((16#${hex}))"
}

cmd_init() {
    ensure_dirs
    append_journal "Initialized live test deployment workspace."
    log "run dir: ${RUN_DIR}"
    log "log dir: ${LOG_DIR}"
}

managed_services() {
    printf '%s\n' \
        serve \
        poll-namecoin \
        poll-rsk \
        poll-syscoin \
        poll-fractal \
        poll-hathor \
        poll-elastos \
        sync-bitcoin-core
}

required_env_vars() {
    printf '%s\n' \
        PGHOST \
        PGPORT \
        PGUSER \
        PGPASSWORD \
        PGDATABASE \
        NAMECOIN_RPC_URL \
        RSK_RPC_URL \
        SYSCOIN_RPC_URL \
        FRACTAL_RPC_URL \
        BITCOIN_RPC_URL \
        SERVE_BIND_ADDR
}

optional_defaulted_env_vars() {
    printf '%s\n' HATHOR_RPC_URL HATHOR_RPC_FALLBACK_URL ELASTOS_RPC_URL
}

cmd_preflight() {
    ensure_dirs
    load_env
    require_cmd psql
    require_cmd curl
    require_cmd jq

    printf 'repo=%s\n' "${REPO_ROOT}"
    printf 'journal=%s\n' "${JOURNAL}"
    printf 'ledger=%s\n' "${LEDGER}"
    printf 'env_file=%s\n' "$([ -f "${REPO_ROOT}/.env" ] && printf present || printf missing)"
    printf 'git_branch=%s\n' "$(git -C "${REPO_ROOT}" branch --show-current)"
    printf 'git_status=%s\n' "$(git -C "${REPO_ROOT}" status --short | wc -l | tr -d ' ') changed paths"

    printf '\nrequired env:\n'
    for var in $(required_env_vars); do
        if [ -n "${!var:-}" ]; then
            printf '  %s=present\n' "${var}"
        else
            printf '  %s=missing\n' "${var}"
        fi
    done

    printf '\noptional/defaulted env:\n'
    for var in $(optional_defaulted_env_vars); do
        if [ -n "${!var:-}" ]; then
            printf '  %s=present\n' "${var}"
        elif [ "${var}" = "ELASTOS_RPC_URL" ]; then
            printf '  %s=defaulted (localhost; set .env for the LAN node)\n' "${var}"
        else
            printf '  %s=defaulted\n' "${var}"
        fi
    done

    printf '\nschema_migrations:\n'
    psql_query "SELECT version FROM schema_migrations ORDER BY version;" || true

    printf '\nsources:\n'
    psql_query "SELECT code FROM source ORDER BY code;" || true
}

cmd_capture_tips() {
    ensure_dirs
    load_env
    require_cmd curl
    require_cmd jq

    local namecoin_tip rsk_tip_hex rsk_tip syscoin_tip captured_at
    captured_at="$(timestamp)"
    namecoin_tip="$(namecoin_rpc getblockcount)"
    rsk_tip_hex="$(rsk_rpc eth_blockNumber '[]')"
    rsk_tip="$(hex_quantity_to_decimal "${rsk_tip_hex}")"
    syscoin_tip="$(syscoin_rpc getblockcount)"

    {
        printf 'CAPTURED_AT=%q\n' "${captured_at}"
        printf 'NAMECOIN_TARGET_TIP=%q\n' "${namecoin_tip}"
        printf 'RSK_TARGET_TIP=%q\n' "${rsk_tip}"
        printf 'SYSCOIN_TARGET_TIP=%q\n' "${syscoin_tip}"
    } > "${TARGETS}"

    append_journal "Captured target tips: Namecoin ${namecoin_tip}, RSK ${rsk_tip}, Syscoin ${syscoin_tip}."
    cat "${TARGETS}"
}

cmd_baseline() {
    ensure_dirs
    load_env
    require_cmd psql

    local out_dir="${RUN_DIR}/baseline-$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "${out_dir}"

    psql_query "SELECT s.code, e.btc_parent_kind, count(*) FROM merge_mining_event e JOIN source s ON s.id = e.source_id WHERE e.revoked_at IS NULL GROUP BY s.code, e.btc_parent_kind ORDER BY s.code, e.btc_parent_kind;" \
        > "${out_dir}/event-kind-counts.txt"
    psql_query "SELECT s.code, max(e.child_height) FROM merge_mining_event e JOIN source s ON s.id = e.source_id WHERE e.revoked_at IS NULL GROUP BY s.code ORDER BY s.code;" \
        > "${out_dir}/max-child-height.txt"
    psql_query "SELECT kind, count(*) FROM block GROUP BY kind ORDER BY kind;" \
        > "${out_dir}/block-kind-counts.txt"
    psql_query "SELECT 'derived_stale_competition', count(*) FROM block stale JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash WHERE stale.kind = 'stale' AND canonical.kind = 'canonical'
UNION ALL SELECT 'attestation_proof', count(*) FROM attestation_proof
UNION ALL SELECT 'rsk_merge_mining_evidence', count(*) FROM rsk_merge_mining_evidence;" \
        > "${out_dir}/read-model-counts.txt"
    cp -f "${LEDGER}" "${out_dir}/processed-ranges.csv"

    local bind base
    bind="${SERVE_BIND_ADDR:-127.0.0.1:8080}"
    base="http://${bind}"
    if curl -fsS "${base}/" -o "${out_dir}/index.html" >/dev/null 2>&1; then
        curl -fsS "${base}/api/v1/sources" -o "${out_dir}/sources.json"
        curl -fsS "${base}/api/v1/tree" -o "${out_dir}/tree.json"
        curl -fsS "${base}/api/v1/navigator/stale?limit=1" -o "${out_dir}/navigator-stale.json"
        curl -fsS "${base}/api/v1/navigator/orphan?limit=1" -o "${out_dir}/navigator-orphan.json"
    else
        printf 'serve was not reachable at %s; API baseline not captured\n' "${base}" > "${out_dir}/api-baseline-skipped.txt"
    fi

    append_journal "Captured baseline DB state in ${out_dir}."
    printf '%s\n' "${out_dir}"
}

run_backfill_range() {
    local chain="$1"
    local start="$2"
    local end="$3"
    local cmd
    cmd="$(chain_backfill_cmd "${chain}")"
    local started finished logfile status
    started="$(timestamp)"
    logfile="${LOG_DIR}/${cmd}-${start}-${end}-$(date -u +%Y%m%dT%H%M%SZ).log"
    append_journal "Starting ${cmd} ${start} ${end} with BITCOIN_RPC_URL disabled. Log: ${logfile}"

    set +e
    BITCOIN_RPC_URL= cargo run -- "${cmd}" "${start}" "${end}" 2>&1 | tee -a "${logfile}"
    status="${PIPESTATUS[0]}"
    set -e

    finished="$(timestamp)"
    if [ "${status}" -eq 0 ]; then
        printf '%s,%s,%s,%s,%s,%s,%s\n' "${chain}" "${start}" "${end}" "${started}" "${finished}" "${status}" "${logfile}" >> "${LEDGER}"
        append_journal "Completed ${cmd} ${start} ${end}."
    else
        append_journal "FAILED ${cmd} ${start} ${end} with exit ${status}. Log: ${logfile}"
        return "${status}"
    fi
}

cmd_backfill() {
    [ "$#" -eq 3 ] || die "backfill requires <chain> <start> <end>"
    ensure_dirs
    load_env
    run_backfill_range "$1" "$2" "$3"
}

cmd_backfill_next() {
    [ "$#" -eq 2 ] || die "backfill-next requires <chain> <chunk-size>"
    ensure_dirs
    load_env
    local chain="$1"
    local chunk="$2"
    [[ "${chunk}" =~ ^[0-9]+$ ]] || die "chunk-size must be a positive integer"
    [ "${chunk}" -gt 0 ] || die "chunk-size must be positive"

    local target start range end
    target="$(target_tip_for "${chain}")"
    start="$(completed_resume_height "${chain}")"
    range="$(next_chunk_range_from_values "${start}" "${target}" "${chunk}")"
    if [ "${range}" = "done" ]; then
        log "${chain} already covered through target tip ${target}"
        return 0
    fi
    end="${range#*,}"
    run_backfill_range "${chain}" "${start}" "${end}"
}

cmd_progress() {
    ensure_dirs
    load_env
    load_targets
    require_cmd psql

    printf 'targets_file=%s\n' "${TARGETS}"
    [ -f "${TARGETS}" ] && cat "${TARGETS}" || printf 'target tips not captured\n'
    printf '\nprocessed ranges:\n'
    cat "${LEDGER}"
    printf '\nnext ledger resume heights:\n'
    for chain in namecoin rsk syscoin; do
        printf '  %s=%s\n' "${chain}" "$(completed_resume_height "${chain}")"
    done
    printf '\nDB max child height by source (sanity only):\n'
    psql_query "SELECT s.code, max(e.child_height) FROM merge_mining_event e JOIN source s ON s.id = e.source_id WHERE e.revoked_at IS NULL GROUP BY s.code ORDER BY s.code;" || true
}

run_logged_command() {
    local label="$1"
    local logfile="$2"
    shift 2
    append_journal "Starting ${label}. Log: ${logfile}"
    set +e
    "$@" 2>&1 | tee -a "${logfile}"
    local status="${PIPESTATUS[0]}"
    set -e
    if [ "${status}" -eq 0 ]; then
        append_journal "Completed ${label}."
    else
        append_journal "FAILED ${label} with exit ${status}. Log: ${logfile}"
        return "${status}"
    fi
}

cmd_classify() {
    ensure_dirs
    load_env
    local logfile="${LOG_DIR}/reclassify-$(date -u +%Y%m%dT%H%M%SZ).log"
    run_logged_command "reclassify-unknown-parents" "${logfile}" \
        cargo run -- reclassify-unknown-parents "$@"
}

cmd_reconcile_all() {
    ensure_dirs
    load_env
    local logfile="${LOG_DIR}/reconcile-all-$(date -u +%Y%m%dT%H%M%SZ).log"
    run_logged_command "reconcile-read-model --all" "${logfile}" \
        cargo run -- reconcile-read-model --all --batch-size 1000 --max-iterations 100000
}

cmd_reconcile_missing() {
    ensure_dirs
    load_env
    local logfile="${LOG_DIR}/reconcile-missing-$(date -u +%Y%m%dT%H%M%SZ).log"
    run_logged_command "reconcile-read-model --missing-only" "${logfile}" \
        cargo run -- reconcile-read-model --missing-only --batch-size 1000 --max-iterations 100000
}

cmd_smoke() {
    ensure_dirs
    load_env
    require_cmd curl
    local bind="${SERVE_BIND_ADDR:-127.0.0.1:8080}"
    local base="http://${bind}"
    local out_dir="${RUN_DIR}/smoke-$(date -u +%Y%m%dT%H%M%SZ)"
    mkdir -p "${out_dir}"
    curl -fsS "${base}/" -o "${out_dir}/index.html"
    curl -fsS "${base}/api/v1/sources" -o "${out_dir}/sources.json"
    curl -fsS "${base}/api/v1/tree" -o "${out_dir}/tree.json"
    curl -fsS "${base}/api/v1/navigator/stale?limit=1" -o "${out_dir}/navigator-stale.json"
    curl -fsS "${base}/api/v1/navigator/orphan?limit=1" -o "${out_dir}/navigator-orphan.json"
    append_journal "Captured smoke responses in ${out_dir}."
    printf '%s\n' "${out_dir}"
}

assert_eq() {
    local got="$1"
    local expected="$2"
    local label="$3"
    if [ "${got}" != "${expected}" ]; then
        die "self-check failed for ${label}: got ${got}, expected ${expected}"
    fi
}

assert_contains() {
    local haystack="$1"
    local needle="$2"
    local label="$3"
    case "${haystack}" in
        *"${needle}"*) ;;
        *) die "self-check failed for ${label}: missing ${needle}" ;;
    esac
}

cmd_self_check() {
    local tmpdir ledger
    tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/mmm-live-test.XXXXXX")"
    ledger="${tmpdir}/processed-ranges.csv"

    printf 'chain,start,end,started_at,finished_at,exit_status,log_path\n' > "${ledger}"
    assert_eq "$(completed_resume_height_from_file "${ledger}" namecoin 19200)" "19200" "empty ledger resume"

    {
        printf 'chain,start,end,started_at,finished_at,exit_status,log_path\n'
        printf 'namecoin,19210,19219,t,t,0,log\n'
        printf 'namecoin,19200,19209,t,t,0,log\n'
        printf 'namecoin,19225,19229,t,t,0,log\n'
        printf 'namecoin,19220,19224,t,t,0,log\n'
    } > "${ledger}"
    assert_eq "$(completed_resume_height_from_file "${ledger}" namecoin 19200)" "19230" "out-of-order contiguous resume"

    {
        printf 'chain,start,end,started_at,finished_at,exit_status,log_path\n'
        printf 'namecoin,19200,19209,t,t,0,log\n'
        printf 'namecoin,19200,19209,t,t,0,log\n'
        printf 'namecoin,19205,19214,t,t,0,log\n'
        printf 'namecoin,19215,19219,t,t,0,log\n'
    } > "${ledger}"
    assert_eq "$(completed_resume_height_from_file "${ledger}" namecoin 19200)" "19220" "duplicate and overlapping resume"

    {
        printf 'chain,start,end,started_at,finished_at,exit_status,log_path\n'
        printf 'rsk,139999,140009,t,t,0,log\n'
        printf 'rsk,140020,140030,t,t,0,log\n'
    } > "${ledger}"
    assert_eq "$(completed_resume_height_from_file "${ledger}" rsk 139999)" "140010" "gap stops resume"

    {
        printf 'chain,start,end,started_at,finished_at,exit_status,log_path\n'
        printf 'syscoin,1973,1982,t,t,1,log\n'
    } > "${ledger}"
    assert_eq "$(completed_resume_height_from_file "${ledger}" syscoin 1973)" "1973" "failed chunk ignored"

    assert_eq "$(next_chunk_range_from_values 10 19 5)" "10,14" "ordinary chunk"
    assert_eq "$(next_chunk_range_from_values 18 19 5)" "18,19" "chunk caps at target"
    assert_eq "$(next_chunk_range_from_values 19 19 5)" "19,19" "single-height target chunk"
    assert_eq "$(next_chunk_range_from_values 20 19 5)" "done" "chunk done past target"
    assert_eq "$(hex_quantity_to_decimal 0x10)" "16" "hex quantity conversion"
    assert_eq "$(chain_floor namecoin)" "19200" "namecoin floor"
    assert_eq "$(chain_floor rsk)" "139999" "rsk floor"
    assert_eq "$(chain_floor syscoin)" "1973" "syscoin floor"
    assert_eq "$(chain_backfill_cmd namecoin)" "backfill-namecoin" "namecoin backfill command"
    assert_eq "$(chain_backfill_cmd rsk)" "backfill-rsk" "rsk backfill command"
    assert_eq "$(chain_backfill_cmd syscoin)" "backfill-syscoin" "syscoin backfill command"
    assert_eq "$(managed_services | tr '\n' ' ' | sed 's/ $//')" \
        "serve poll-namecoin poll-rsk poll-syscoin poll-fractal poll-hathor poll-elastos sync-bitcoin-core" \
        "managed service roster"
    assert_eq "$(required_env_vars | tr '\n' ' ' | sed 's/ $//')" \
        "PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE NAMECOIN_RPC_URL RSK_RPC_URL SYSCOIN_RPC_URL FRACTAL_RPC_URL BITCOIN_RPC_URL SERVE_BIND_ADDR" \
        "required env roster"
    assert_eq "$(optional_defaulted_env_vars | tr '\n' ' ' | sed 's/ $//')" \
        "HATHOR_RPC_URL HATHOR_RPC_FALLBACK_URL ELASTOS_RPC_URL" \
        "optional env roster"
    local usage_text
    usage_text="$(usage 2>&1)"
    assert_contains "${usage_text}" "poll-fractal" "usage service list"
    assert_contains "${usage_text}" "poll-hathor" "usage service list"
    assert_eq "$(chain_poll_cmd poll-namecoin)" "poll-namecoin" "namecoin poll command"
    assert_eq "$(chain_poll_cmd poll-rsk)" "poll-rsk" "rsk poll command"
    assert_eq "$(chain_poll_cmd poll-syscoin)" "poll-syscoin" "syscoin poll command"
    assert_eq "$(chain_poll_cmd poll-fractal)" "poll-fractal" "fractal poll command"
    assert_eq "$(chain_poll_cmd poll-hathor)" "poll-hathor" "hathor poll command"
    assert_eq "$(chain_poll_cmd poll-elastos)" "poll-elastos" "elastos poll command"
    assert_eq "$(service_command serve)" "serve" "serve service command"
    assert_eq "$(service_command poll-syscoin)" "poll-syscoin" "syscoin service command"
    assert_eq "$(service_command poll-fractal)" "poll-fractal" "fractal service command"
    assert_eq "$(service_command poll-hathor)" "poll-hathor" "hathor service command"
    assert_eq "$(service_command poll-elastos)" "poll-elastos" "elastos service command"
    assert_eq "$(service_command sync-bitcoin-core)" "sync-bitcoin-core --follow" "backbone service command"
    assert_eq "$(target_var namecoin)" "NAMECOIN_TARGET_TIP" "namecoin target var"
    assert_eq "$(target_var rsk)" "RSK_TARGET_TIP" "rsk target var"
    assert_eq "$(target_var syscoin)" "SYSCOIN_TARGET_TIP" "syscoin target var"

    rm -rf "${tmpdir}"
    printf 'live-test self-check passed\n'
}

service_command() {
    case "$1" in
        serve) printf 'serve' ;;
        sync-bitcoin-core) printf 'sync-bitcoin-core --follow' ;;
        poll-namecoin|poll-rsk|poll-syscoin|poll-fractal|poll-hathor|poll-elastos) chain_poll_cmd "$1" ;;
        *) die "unknown service $1" ;;
    esac
}

cmd_start() {
    [ "$#" -eq 1 ] || die "start requires <service>"
    ensure_dirs
    load_env
    local service="$1"
    local pidfile logfile
    # service_command may return multiple tokens (e.g. "sync-bitcoin-core --follow").
    # Capture it first so an unknown-service `die` propagates (set -e) instead of
    # being masked by the command substitution inside `read` (which would succeed
    # on an empty here-string and continue into build/launch); then split into an
    # argv array so flags pass through instead of being one quoted arg.
    local launch_str
    launch_str="$(service_command "${service}")"
    local -a launch
    read -r -a launch <<< "${launch_str}"
    pidfile="${PID_DIR}/${service}.pid"
    logfile="${LOG_DIR}/${service}-$(date -u +%Y%m%dT%H%M%SZ).log"
    if [ -f "${pidfile}" ] && kill -0 "$(cat "${pidfile}")" >/dev/null 2>&1; then
        die "${service} already running with pid $(cat "${pidfile}")"
    fi
    cargo build >/dev/null
    append_journal "Starting ${service}. Log: ${logfile}"
    local binary="${REPO_ROOT}/target/debug/merge-mining-monitor"
    if command -v setsid >/dev/null 2>&1; then
        nohup setsid "${binary}" "${launch[@]}" > "${logfile}" 2>&1 < /dev/null &
    elif command -v perl >/dev/null 2>&1; then
        nohup perl -MPOSIX=setsid -e 'setsid() or die "setsid: $!"; exec @ARGV or die "exec: $!"' \
            "${binary}" "${launch[@]}" > "${logfile}" 2>&1 < /dev/null &
    else
        die "setsid or perl is required to detach ${service}"
    fi
    local pid="$!"
    printf '%s\n' "${pid}" > "${pidfile}"
    sleep 1
    if ! kill -0 "${pid}" >/dev/null 2>&1; then
        rm -f "${pidfile}"
        append_journal "Failed to start ${service}. Log: ${logfile}"
        tail -n 40 "${logfile}" >&2 || true
        die "${service} exited immediately; see ${logfile}"
    fi
    printf '%s pid=%s log=%s\n' "${service}" "${pid}" "${logfile}"
}

cmd_stop() {
    [ "$#" -eq 1 ] || die "stop requires <service>"
    ensure_dirs
    local service="$1"
    local pidfile="${PID_DIR}/${service}.pid"
    [ -f "${pidfile}" ] || die "no pid file for ${service}"
    local pid
    pid="$(cat "${pidfile}")"
    if kill -0 "${pid}" >/dev/null 2>&1; then
        kill "${pid}"
        local waited=0
        while kill -0 "${pid}" >/dev/null 2>&1; do
            if [ "${waited}" -ge 75 ]; then
                kill -KILL "${pid}" >/dev/null 2>&1 || true
                break
            fi
            sleep 1
            waited=$((waited + 1))
        done
        if kill -0 "${pid}" >/dev/null 2>&1; then
            die "${service} pid ${pid} did not exit"
        fi
        append_journal "Stopped ${service} pid ${pid}."
    fi
    rm -f "${pidfile}"
}

cmd_status() {
    ensure_dirs
    for service in $(managed_services); do
        local pidfile="${PID_DIR}/${service}.pid"
        if [ -f "${pidfile}" ] && kill -0 "$(cat "${pidfile}")" >/dev/null 2>&1; then
            printf '%s=running pid=%s\n' "${service}" "$(cat "${pidfile}")"
        else
            printf '%s=stopped\n' "${service}"
        fi
    done
}

main() {
    [ "$#" -ge 1 ] || { usage; exit 2; }
    local command="$1"
    shift
    cd "${REPO_ROOT}"
    case "${command}" in
        init) cmd_init "$@" ;;
        preflight) cmd_preflight "$@" ;;
        capture-tips) cmd_capture_tips "$@" ;;
        baseline) cmd_baseline "$@" ;;
        progress) cmd_progress "$@" ;;
        backfill) cmd_backfill "$@" ;;
        backfill-next) cmd_backfill_next "$@" ;;
        classify) cmd_classify "$@" ;;
        reconcile-all) cmd_reconcile_all "$@" ;;
        reconcile-missing) cmd_reconcile_missing "$@" ;;
        smoke) cmd_smoke "$@" ;;
        self-check) cmd_self_check "$@" ;;
        start) cmd_start "$@" ;;
        stop) cmd_stop "$@" ;;
        status) cmd_status "$@" ;;
        -h|--help|help) usage ;;
        *) usage; exit 2 ;;
    esac
}

main "$@"
