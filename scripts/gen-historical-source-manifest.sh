#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: scripts/gen-historical-source-manifest.sh [--check] [--allow-missing-repo] [--repo-dir DIR] [--out PATH]

Generate or verify data/historical/historical-source-manifest.json and its checksum from
committed merge-mining-research validated stale CSV blobs.

Options:
  --check         Compare the generated manifest with the committed file
  --allow-missing-repo
                  In --check mode, skip with a warning when the source clone
                  is unavailable. Intended for the standard test gate.
  --repo-dir DIR  merge-mining-research clone (default: $MERGE_MINING_RESEARCH_DIR)
  --out PATH      Output manifest path (default: data/historical/historical-source-manifest.json)
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "${script_dir}/lib/historical-source-chains.sh"

skip_check() {
    printf 'historical source manifest check skipped: %s\n' "$*" >&2
    exit 0
}

repo_dir="${MERGE_MINING_RESEARCH_DIR:-}"
output="data/historical/historical-source-manifest.json"
check=0
allow_missing_repo=0

while [ "$#" -gt 0 ]; do
    case "$1" in
        --check)
            check=1
            shift
            ;;
        --allow-missing-repo)
            allow_missing_repo=1
            shift
            ;;
        --repo-dir)
            [ "$#" -ge 2 ] || die "--repo-dir requires a value"
            repo_dir="$2"
            shift 2
            ;;
        --out)
            [ "$#" -ge 2 ] || die "--out requires a value"
            output="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown argument: $1"
            ;;
    esac
done

if [ -z "${repo_dir}" ]; then
    if [ "${check}" -eq 1 ] && [ "${allow_missing_repo}" -eq 1 ]; then
        skip_check "source repo not configured; set MERGE_MINING_RESEARCH_DIR or pass --repo-dir"
    fi
    die "source repo not configured; set MERGE_MINING_RESEARCH_DIR or pass --repo-dir"
fi

if ! git -C "${repo_dir}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    if [ "${check}" -eq 1 ] && [ "${allow_missing_repo}" -eq 1 ]; then
        skip_check "source repo unavailable: ${repo_dir}"
    fi
    die "not a git work tree: ${repo_dir}"
fi

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        die "neither sha256sum nor shasum is available"
    fi
}

manifest_checksum_path() {
    local manifest_path="$1"

    case "${manifest_path}" in
        *.json) printf '%s.sha256\n' "${manifest_path%.json}" ;;
        *) printf '%s.sha256\n' "${manifest_path}" ;;
    esac
}

require_column() {
    local header="$1"
    local column="$2"
    local file="$3"

    header="$(printf '%s\n' "${header}" | sed 's/^[[:space:]]*//; s/[[:space:]]*$//; s/[[:space:]]*,[[:space:]]*/,/g')"
    case ",${header}," in
        *",${column},"*) ;;
        *) die "${file} is missing required column ${column}" ;;
    esac
}

manifest_source_commit() {
    sed -n 's/^[[:space:]]*"source_repo_commit": "\([0-9a-f][0-9a-f]*\)",[[:space:]]*$/\1/p' "$1"
}

# These source CSV blobs are stable one-record-per-line exports. The parser
# below handles quoted commas in validation_status and other free-text fields.
validated_stale_row_count_for() {
    awk -v path="$1" '
        function trim(value) {
            gsub(/^[[:space:]]+/, "", value)
            gsub(/[[:space:]]+$/, "", value)
            return value
        }

        function csv_split(line, out,    i, ch, next_ch, field, count, in_quotes, key) {
            for (key in out) {
                delete out[key]
            }
            count = 1
            field = ""
            in_quotes = 0

            for (i = 1; i <= length(line); i++) {
                ch = substr(line, i, 1)
                if (in_quotes) {
                    if (ch == "\"") {
                        next_ch = substr(line, i + 1, 1)
                        if (next_ch == "\"") {
                            field = field "\""
                            i++
                        } else {
                            in_quotes = 0
                        }
                    } else {
                        field = field ch
                    }
                } else if (ch == "\"") {
                    in_quotes = 1
                } else if (ch == ",") {
                    out[count] = field
                    count++
                    field = ""
                } else {
                    field = field ch
                }
            }

            out[count] = field
            return count
        }

        NR == 1 {
            field_count = csv_split($0, fields)
            for (i = 1; i <= field_count; i++) {
                gsub(/\r/, "", fields[i])
                fields[i] = trim(fields[i])
                if (fields[i] == "classification") {
                    classification_col = i
                }
                if (fields[i] == "validation_status") {
                    validation_col = i
                }
            }
            next
        }
        /^[[:space:]]*$/ { next }
        {
            csv_split($0, fields)
            classification = trim(fields[classification_col])
            validation_status = trim(fields[validation_col])
            gsub(/\r/, "", classification)
            gsub(/\r/, "", validation_status)

            if (classification == "stale" && validation_status ~ /^VALID([[:space:](]|$)/) {
                count++
            } else {
                printf "%s:%d is not a validated stale row: classification=%s validation_status=%s\n", path, NR, classification, validation_status > "/dev/stderr"
                invalid = 1
            }
        }
        END {
            if (NR == 0) {
                print -1
            } else if (invalid) {
                exit 2
            } else {
                print count + 0
            }
        }
    ' "$1"
}

tmp="$(mktemp "${TMPDIR:-/tmp}/historical-source-manifest.XXXXXX")"
csv_tmpdir="$(mktemp -d "${TMPDIR:-/tmp}/historical-source-csvs.XXXXXX")"
cleanup() {
    rm -f "${tmp}"
    rm -rf "${csv_tmpdir}"
}
trap cleanup EXIT

if [ "${check}" -eq 1 ]; then
    [ -f "${output}" ] || die "missing committed manifest ${output}"
    base_commit="$(manifest_source_commit "${output}")"
    printf '%s\n' "${base_commit}" | grep -Eq '^[0-9a-f]{40}$' \
        || die "${output} has no valid source_repo_commit"
    if ! git -C "${repo_dir}" cat-file -e "${base_commit}^{commit}" 2>/dev/null; then
        if [ "${allow_missing_repo}" -eq 1 ]; then
            skip_check "source repo ${repo_dir} does not contain pinned commit ${base_commit}"
        fi
        die "source repo ${repo_dir} does not contain pinned commit ${base_commit}"
    fi
else
    base_commit="$(git -C "${repo_dir}" rev-parse HEAD)"
fi

set_csv_file() {
    local chain="$1"
    local csv_path="$2"

    file="${csv_tmpdir}/${chain}.csv"
    if [ ! -f "${file}" ]; then
        git -C "${repo_dir}" show "${base_commit}:${csv_path}" >"${file}" 2>/dev/null \
            || die "${csv_path} is missing at ${base_commit}"
    fi
}

total_rows=0
while IFS='|' read -r chain height_column; do
    csv_path="data/${chain}_validated_stales.csv"
    set_csv_file "${chain}" "${csv_path}"
    [ -f "${file}" ] || die "missing ${file}"
    header="$(head -n 1 "${file}" | tr -d '\r')"
    [ -n "${header}" ] || die "${file} has no header"
    require_column "${header}" "btc_header_hex" "${csv_path}"
    require_column "${header}" "coinbase_scriptsig_hex" "${csv_path}"
    require_column "${header}" "classification" "${csv_path}"
    require_column "${header}" "validation_status" "${csv_path}"
    require_column "${header}" "${height_column}" "${csv_path}"
    rows="$(validated_stale_row_count_for "${file}")" \
        || die "${csv_path} contains rows outside the validated stale scope"
    [ "${rows}" -ge 0 ] || die "${file} is empty"
    total_rows=$((total_rows + rows))
done <<EOF
$(historical_source_chain_entries)
EOF

{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "scope": "historical_auxpow_validated_stales_phase1",\n'
    printf '  "source_class": "validated_stales_csv",\n'
    printf '  "source_repo": "merge-mining-research",\n'
    printf '  "source_repo_commit": "%s",\n' "${base_commit}"
    printf '  "manifest_generator": "scripts/gen-historical-source-manifest.sh",\n'
    printf '  "total_declared_stale_rows": %s,\n' "${total_rows}"
    printf '  "notes": [\n'
    printf '    "Scope is the historical-only chains in scripts/lib/historical-source-chains.sh; live chains are excluded.",\n'
    printf '    "Inputs are committed validated stale CSV blobs from the source repo; full-evidence and orphan-heavy inventories are intentionally kept outside this repo.",\n'
    printf '    "Rows are stale-only provenance inputs and are re-proven by the monitor Bitcoin Core classifier during import."\n'
    printf '  ],\n'
    printf '  "sources": [\n'

    index=0
    source_count="$(historical_source_chain_entries | wc -l | tr -d '[:space:]')"
    # The interpolated values come from static chain metadata, integer counts,
    # git commit hex, and SHA-256 hex, so no JSON escaping is required here.
    while IFS='|' read -r chain height_column; do
        index=$((index + 1))
        csv_path="data/${chain}_validated_stales.csv"
        set_csv_file "${chain}" "${csv_path}"
        rows="$(validated_stale_row_count_for "${file}")" \
            || die "${csv_path} contains rows outside the validated stale scope"
        sha256="$(sha256_file "${file}")"

        printf '    {\n'
        printf '      "chain": "%s",\n' "${chain}"
        printf '      "source_code": "auxpow:%s",\n' "${chain}"
        printf '      "csv_path": "%s",\n' "${csv_path}"
        printf '      "height_column": "%s",\n' "${height_column}"
        printf '      "declared_stale_rows": %s,\n' "${rows}"
        printf '      "sha256": "%s"\n' "${sha256}"
        if [ "${index}" -eq "${source_count}" ]; then
            printf '    }\n'
        else
            printf '    },\n'
        fi
    done <<EOF
$(historical_source_chain_entries)
EOF

    printf '  ]\n'
    printf '}\n'
} >"${tmp}"

if [ "${check}" -eq 1 ]; then
    [ -f "${output}" ] || die "missing committed manifest ${output}"
    if ! cmp -s "${tmp}" "${output}"; then
        printf 'historical source manifest drifted: %s\n' "${output}" >&2
        printf 'regenerate with: scripts/gen-historical-source-manifest.sh --repo-dir %s --out %s\n' "${repo_dir}" "${output}" >&2
        exit 1
    fi
    checksum_output="$(manifest_checksum_path "${output}")"
    [ -f "${checksum_output}" ] \
        || die "missing historical source manifest checksum ${checksum_output}"
    expected_checksum="$(sed -n '1{s/[[:space:]].*$//;p;q;}' "${checksum_output}")"
    printf '%s\n' "${expected_checksum}" | grep -Eq '^[0-9a-f]{64}$' \
        || die "${checksum_output} does not contain a 64-character lowercase hex checksum"
    actual_checksum="$(sha256_file "${output}")"
    if [ "${actual_checksum}" != "${expected_checksum}" ]; then
        printf 'historical source manifest checksum drifted: %s\n' "${checksum_output}" >&2
        printf 'regenerate with: scripts/gen-historical-source-manifest.sh --repo-dir %s --out %s\n' "${repo_dir}" "${output}" >&2
        exit 1
    fi
    printf 'historical source manifest is up to date: %s\n' "${output}"
else
    mv -f "${tmp}" "${output}"
    chmod 0644 "${output}"
    checksum_output="$(manifest_checksum_path "${output}")"
    printf '%s\n' "$(sha256_file "${output}")" >"${checksum_output}"
    chmod 0644 "${checksum_output}"
    printf 'wrote %s\n' "${output}"
    printf 'wrote %s\n' "${checksum_output}"
fi
