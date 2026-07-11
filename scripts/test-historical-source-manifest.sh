#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
generator="${repo_root}/scripts/gen-historical-source-manifest.sh"
source "${repo_root}/scripts/lib/historical-source-chains.sh"

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

sha256_stream() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 | awk '{ print $1 }'
    else
        die "neither sha256sum nor shasum is available"
    fi
}

scratch="$(mktemp -d "${TMPDIR:-/tmp}/historical-source-manifest-test.XXXXXX")"
cleanup() {
    rm -rf "${scratch}"
}
trap cleanup EXIT

committed_manifest="${repo_root}/data/historical/historical-source-manifest.json"
committed_manifest_checksum="${repo_root}/data/historical/historical-source-manifest.sha256"
expected_source_count="$(historical_source_chain_entries | wc -l | tr -d '[:space:]')"

grep -Eq '^[[:space:]]*"source_repo_commit": "[0-9a-f]{40}",[[:space:]]*$' \
    "${committed_manifest}" || die "committed manifest source_repo_commit is not 40 lowercase hex"

actual_source_count="$(grep -c '"source_code": "auxpow:' "${committed_manifest}")"
[ "${actual_source_count}" -eq "${expected_source_count}" ] \
    || die "committed manifest has ${actual_source_count} sources; expected ${expected_source_count}"

actual_sha_count="$(grep -Ec '^[[:space:]]*"sha256": "[0-9a-f]{64}"[[:space:]]*$' \
    "${committed_manifest}")"
[ "${actual_sha_count}" -eq "${expected_source_count}" ] \
    || die "committed manifest has ${actual_sha_count} valid sha256 fields; expected ${expected_source_count}"

[ -f "${committed_manifest_checksum}" ] \
    || die "committed manifest checksum is missing: ${committed_manifest_checksum}"
expected_manifest_sha="$(sed -n '1{s/[[:space:]].*$//;p;q;}' "${committed_manifest_checksum}")"
printf '%s\n' "${expected_manifest_sha}" | grep -Eq '^[0-9a-f]{64}$' \
    || die "committed manifest checksum is not 64 lowercase hex"
actual_manifest_sha="$(sha256_stream <"${committed_manifest}")"
[ "${actual_manifest_sha}" = "${expected_manifest_sha}" ] \
    || die "committed manifest checksum does not match ${committed_manifest_checksum}"

tampered_committed_manifest="${scratch}/tampered-committed-manifest.json"
awk '
    /"sha256":/ && !changed {
        replacement = "0000000000000000000000000000000000000000000000000000000000000000"
        if (index($0, replacement) > 0) {
            replacement = "1111111111111111111111111111111111111111111111111111111111111111"
        }
        sub(/"sha256": "[0-9a-f][0-9a-f]*"/, "\"sha256\": \"" replacement "\"")
        changed = 1
    }
    { print }
    END {
        if (!changed) {
            exit 2
        }
    }
' "${committed_manifest}" >"${tampered_committed_manifest}" \
    || die "could not build a well-formed tampered manifest fixture"
tampered_manifest_sha="$(sha256_stream <"${tampered_committed_manifest}")"
[ "${tampered_manifest_sha}" != "${expected_manifest_sha}" ] \
    || die "well-formed committed manifest value edit did not change the checksum"

declared_total="$(sed -n 's/^[[:space:]]*"total_declared_stale_rows": \([0-9][0-9]*\),[[:space:]]*$/\1/p' \
    "${committed_manifest}")"
[ -n "${declared_total}" ] || die "committed manifest is missing total_declared_stale_rows"
row_sum="$(sed -n 's/^[[:space:]]*"declared_stale_rows": \([0-9][0-9]*\),[[:space:]]*$/\1/p' \
    "${committed_manifest}" | awk '{ sum += $1 } END { print sum + 0 }')"
[ "${declared_total}" -eq "${row_sum}" ] \
    || die "committed manifest total_declared_stale_rows ${declared_total} does not equal per-source sum ${row_sum}"

expected_chain_heights="${scratch}/expected-chain-heights.txt"
manifest_chain_heights="${scratch}/manifest-chain-heights.txt"
historical_source_chain_entries | sort >"${expected_chain_heights}"
awk '
    /"chain":/ {
        line = $0
        sub(/^[[:space:]]*"chain": "/, "", line)
        sub(/".*$/, "", line)
        chain = line
        next
    }
    /"height_column":/ {
        line = $0
        sub(/^[[:space:]]*"height_column": "/, "", line)
        sub(/".*$/, "", line)
        if (chain == "") {
            exit 2
        }
        print chain "|" line
        chain = ""
    }
' "${committed_manifest}" | sort >"${manifest_chain_heights}" \
    || die "committed manifest chain/height_column fields are malformed"
if ! diff -u "${expected_chain_heights}" "${manifest_chain_heights}" >/dev/null; then
    diff -u "${expected_chain_heights}" "${manifest_chain_heights}" >&2 || true
    die "committed manifest chain/height_column mapping does not match shared historical chain table"
fi

registry_codes_all="${scratch}/registry-historical-auxpow-codes-all.txt"
registry_codes="${scratch}/registry-historical-auxpow-codes.txt"
explicit_recovery_codes="${scratch}/explicit-recovery-source-codes.txt"
manifest_codes="${scratch}/manifest-source-codes.txt"
awk '
    /export const SOURCE_LIFECYCLE = {/ { in_lifecycle = 1; next }
    in_lifecycle && /^};/ { in_lifecycle = 0 }
    in_lifecycle && /"auxpow:[^"]*": "historical"/ {
        line = $0
        sub(/^[^"]*"/, "", line)
        sub(/".*$/, "", line)
        print line
    }
' "${repo_root}/www/js/source-registry.generated.js" | sort >"${registry_codes_all}"
explicit_recovery_source_codes | sort >"${explicit_recovery_codes}"
grep -Fvx -f "${explicit_recovery_codes}" "${registry_codes_all}" >"${registry_codes}"
sed -n 's/.*"source_code": "\(auxpow:[^"]*\)".*/\1/p' \
    "${committed_manifest}" | sort >"${manifest_codes}"
if ! diff -u "${registry_codes}" "${manifest_codes}" >/dev/null; then
    diff -u "${registry_codes}" "${manifest_codes}" >&2 || true
    die "historical manifest source set does not match generated SOURCE_LIFECYCLE"
fi

source_repo="${scratch}/merge-mining-research"
mkdir -p "${source_repo}/data"
git -C "${source_repo}" init -q

while IFS='|' read -r chain height_column; do
    csv="${source_repo}/data/${chain}_validated_stales.csv"
    printf 'btc_header_hex,coinbase_scriptsig_hex,%s,classification,validation_status\n' "${height_column}" >"${csv}"
    case "${chain}" in
        argentum)
            printf 'aa,"bb,pre-classification comma",101,stale,"VALID"\n' >>"${csv}"
            ;;
        bitcoin-vault)
            printf 'cc,dd,202,stale,"VALID (coinbase, header)"\n' >>"${csv}"
            ;;
    esac
done <<EOF
$(historical_source_chain_entries)
EOF

git -C "${source_repo}" add data
git -C "${source_repo}" \
    -c commit.gpgsign=false \
    -c user.name="Merge Mining Monitor Tests" \
    -c user.email="merge-mining-monitor-tests@example.invalid" \
    commit -qm "add historical source fixture"

source_commit="$(git -C "${source_repo}" rev-parse HEAD)"
manifest="${scratch}/historical-source-manifest.json"
"${generator}" --repo-dir "${source_repo}" --out "${manifest}" >/dev/null

grep -q "\"source_repo_commit\": \"${source_commit}\"" "${manifest}" \
    || die "manifest did not record the fixture commit"
grep -q '"total_declared_stale_rows": 2,' "${manifest}" \
    || die "manifest did not count the two stale fixture rows"
[ "$(grep -c '"declared_stale_rows": 1,' "${manifest}")" -eq 2 ] \
    || die "manifest did not assign one row to each non-empty fixture"
expected_zero_row_sources=$((expected_source_count - 2))
[ "$(grep -c '"declared_stale_rows": 0,' "${manifest}")" -eq "${expected_zero_row_sources}" ] \
    || die "manifest did not preserve header-only fixtures as zero-row sources"

argentum_sha="$(git -C "${source_repo}" show "${source_commit}:data/argentum_validated_stales.csv" | sha256_stream)"
grep -q "\"sha256\": \"${argentum_sha}\"" "${manifest}" \
    || die "manifest did not record the committed blob checksum"

"${generator}" --check --repo-dir "${source_repo}" --out "${manifest}" >/dev/null

checksum_path="${manifest%.json}.sha256"
checksum_drift_log="${scratch}/checksum-drift-check.log"
printf '0000000000000000000000000000000000000000000000000000000000000000\n' \
    >"${checksum_path}"
if "${generator}" --check --repo-dir "${source_repo}" --out "${manifest}" \
    >"${checksum_drift_log}" 2>&1; then
    die "strict check unexpectedly passed with a stale manifest checksum"
fi
grep -q "historical source manifest checksum drifted: ${checksum_path}" \
    "${checksum_drift_log}" || die "strict check did not report manifest checksum drift"
"${generator}" --repo-dir "${source_repo}" --out "${manifest}" >/dev/null

drift_manifest="${scratch}/drifted-manifest.json"
sed 's/"total_declared_stale_rows": 2,/"total_declared_stale_rows": 3,/' \
    "${manifest}" >"${drift_manifest}"
drift_log="${scratch}/drift-check.log"
if "${generator}" --check --repo-dir "${source_repo}" --out "${drift_manifest}" \
    >"${drift_log}" 2>&1; then
    die "strict check unexpectedly passed with a drifted manifest"
fi
grep -q "historical source manifest drifted: ${drift_manifest}" "${drift_log}" \
    || die "strict check did not report manifest drift"

missing_repo_log="${scratch}/missing-repo-skip.log"
"${generator}" --check --allow-missing-repo --repo-dir "${scratch}/missing-source-repo" \
    --out "${manifest}" >"${missing_repo_log}" 2>&1
grep -q "historical source manifest check skipped: source repo unavailable" \
    "${missing_repo_log}" || die "allow-missing check did not report the missing source repo"

missing_commit_manifest="${scratch}/missing-commit-manifest.json"
sed 's/"source_repo_commit": "[0-9a-f]*"/"source_repo_commit": "0000000000000000000000000000000000000000"/' \
    "${manifest}" >"${missing_commit_manifest}"
if "${generator}" --check --repo-dir "${source_repo}" --out "${missing_commit_manifest}" >/dev/null 2>&1; then
    die "strict check unexpectedly passed with a missing pinned commit"
fi
skip_log="${scratch}/missing-commit-skip.log"
"${generator}" --check --allow-missing-repo --repo-dir "${source_repo}" --out "${missing_commit_manifest}" \
    >"${skip_log}" 2>&1
grep -q "historical source manifest check skipped: source repo ${source_repo} does not contain pinned commit" \
    "${skip_log}" || die "allow-missing check did not report the missing pinned commit"

{
    printf 'ee,ff,303,orphan,"VALID"\n'
    printf 'gg,hh,404,stale,INVALID\n'
    printf 'ii,jj,505,stale,VALIDATED\n'
} >>"${source_repo}/data/argentum_validated_stales.csv"
git -C "${source_repo}" add data/argentum_validated_stales.csv
git -C "${source_repo}" \
    -c commit.gpgsign=false \
    -c user.name="Merge Mining Monitor Tests" \
    -c user.email="merge-mining-monitor-tests@example.invalid" \
    commit -qm "add invalid historical source row"
invalid_log="${scratch}/invalid-row.log"
if "${generator}" --repo-dir "${source_repo}" --out "${scratch}/invalid-row-manifest.json" \
    >"${invalid_log}" 2>&1; then
    die "generation unexpectedly passed with an invalid row"
fi
grep -q "contains rows outside the validated stale scope" "${invalid_log}" \
    || die "invalid row did not produce the strict scope error"
grep -q "classification=orphan validation_status=VALID" "${invalid_log}" \
    || die "non-stale row with VALID status was not rejected"
grep -q "classification=stale validation_status=INVALID" "${invalid_log}" \
    || die "stale row with INVALID status was not rejected"
grep -q "classification=stale validation_status=VALIDATED" "${invalid_log}" \
    || die "stale row with VALIDATED status was not rejected"

printf 'historical source manifest self-test passed\n'
