#!/usr/bin/env bash
# Self-checks for scripts/live-test-deployment.sh.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/live-test-deployment.sh
source "${SCRIPT_DIR}/live-test-deployment.sh"

assert_eq() {
    local got="$1"
    local expected="$2"
    local label="$3"
    if [ "${got}" != "${expected}" ]; then
        die "self-check failed for ${label}: got ${got}, expected ${expected}"
    fi
}

tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/mmm-live-test.XXXXXX")"
trap 'rm -rf "${tmpdir}"' EXIT
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

assert_contains() {
    local haystack="$1"
    local needle="$2"
    local label="$3"
    case "${haystack}" in
        *"${needle}"*) ;;
        *) die "self-check failed for ${label}: missing ${needle}" ;;
    esac
}

assert_eq "$(chain_floor namecoin)" "19200" "namecoin floor"
assert_eq "$(chain_floor rsk)" "139999" "rsk floor"
assert_eq "$(chain_floor syscoin)" "1973" "syscoin floor"
assert_eq "$(chain_backfill_cmd namecoin)" "backfill-namecoin" "namecoin backfill command"
assert_eq "$(chain_backfill_cmd rsk)" "backfill-rsk" "rsk backfill command"
assert_eq "$(chain_backfill_cmd syscoin)" "backfill-syscoin" "syscoin backfill command"
assert_eq "$(managed_services | tr '\n' ' ' | sed 's/ $//')" \
    "serve poll-namecoin poll-rsk poll-syscoin poll-fractal poll-hathor poll-elastos poll-qbit poll-terracoin sync-bitcoin-core" \
    "managed service roster"
assert_eq "$(required_env_vars | tr '\n' ' ' | sed 's/ $//')" \
    "PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE NAMECOIN_RPC_URL RSK_RPC_URL SYSCOIN_RPC_URL FRACTAL_RPC_URL QBIT_RPC_URL TERRACOIN_RPC_URL TERRACOIN_RPC_USER TERRACOIN_RPC_PASSWORD BITCOIN_RPC_URL SERVE_BIND_ADDR" \
    "required env roster"
assert_eq "$(optional_defaulted_env_vars | tr '\n' ' ' | sed 's/ $//')" \
    "HATHOR_RPC_URL HATHOR_RPC_FALLBACK_URL ELASTOS_RPC_URL" \
    "optional env roster"
usage_text="$(usage 2>&1)"
assert_contains "${usage_text}" "poll-fractal" "usage service list"
assert_contains "${usage_text}" "poll-hathor" "usage service list"
assert_contains "${usage_text}" "poll-qbit" "usage service list"
assert_eq "$(chain_poll_cmd poll-namecoin)" "poll-namecoin" "namecoin poll command"
assert_eq "$(chain_poll_cmd poll-rsk)" "poll-rsk" "rsk poll command"
assert_eq "$(chain_poll_cmd poll-syscoin)" "poll-syscoin" "syscoin poll command"
assert_eq "$(chain_poll_cmd poll-fractal)" "poll-fractal" "fractal poll command"
assert_eq "$(chain_poll_cmd poll-hathor)" "poll-hathor" "hathor poll command"
assert_eq "$(chain_poll_cmd poll-elastos)" "poll-elastos" "elastos poll command"
assert_eq "$(chain_poll_cmd poll-qbit)" "poll-qbit" "qbit poll command"
assert_eq "$(service_command serve)" "serve" "serve service command"
assert_eq "$(service_command poll-syscoin)" "poll-syscoin" "syscoin service command"
assert_eq "$(service_command poll-fractal)" "poll-fractal" "fractal service command"
assert_eq "$(service_command poll-hathor)" "poll-hathor" "hathor service command"
assert_eq "$(service_command poll-elastos)" "poll-elastos" "elastos service command"
assert_eq "$(service_command poll-qbit)" "poll-qbit" "qbit service command"
assert_eq "$(service_command sync-bitcoin-core)" "sync-bitcoin-core --follow" "backbone service command"
assert_eq "$(target_var namecoin)" "NAMECOIN_TARGET_TIP" "namecoin target var"
assert_eq "$(target_var rsk)" "RSK_TARGET_TIP" "rsk target var"
assert_eq "$(target_var syscoin)" "SYSCOIN_TARGET_TIP" "syscoin target var"

backfill_fn="$(awk '/^run_backfill_range\(/,/^}/' "${SCRIPT_DIR}/live-test-deployment.sh")"
case "${backfill_fn}" in
    *"BITCOIN_RPC_URL="*) die "self-check failed: live-test must not clear BITCOIN_RPC_URL" ;;
    *"disabled"*|*"DB-only"*) die "self-check failed: journal must not claim DB-only backfill" ;;
esac
(
    BITCOIN_RPC_URL="http://127.0.0.1:8332"
    require_bitcoin_rpc_url
) || die "self-check failed: set BITCOIN_RPC_URL must be accepted"
if (
    unset BITCOIN_RPC_URL
    require_bitcoin_rpc_url
) 2>/dev/null; then
    die "self-check failed: unset BITCOIN_RPC_URL must fail closed"
fi
if (
    BITCOIN_RPC_URL=
    require_bitcoin_rpc_url
) 2>/dev/null; then
    die "self-check failed: empty BITCOIN_RPC_URL must fail closed"
fi

printf 'live-test self-check passed\n'
