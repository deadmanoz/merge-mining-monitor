#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: scripts/gen-historical-source-manifest.sh [--check] [--allow-missing-repo] [--repo-dir DIR] [--source-commit COMMIT] [--out PATH]

Generate or verify the monitor-owned provenance manifest for the normalized
merge-mining-research monitor-evidence publication.

Options:
  --check         Compare generated output with the committed manifest
  --allow-missing-repo
                  In --check mode, reuse committed parent-only counts only when
                  source payloads are unavailable; skip if the clone is absent
  --repo-dir DIR  merge-mining-research clone (default: $MERGE_MINING_RESEARCH_DIR)
  --source-commit COMMIT
                  Publication commit (default: the committed manifest pin)
  --out PATH      Output path (default: data/historical/historical-source-manifest.json)
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

skip_check() {
    printf 'historical source manifest check skipped: %s\n' "$*" >&2
    exit 0
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{ print $1 }'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{ print $1 }'
    else
        die "neither sha256sum nor shasum is available"
    fi
}

count_parent_only_rows() {
    python3 - "$1" <<'PY'
import csv
import sys
from pathlib import Path

path = Path(sys.argv[1])
with path.open(newline="") as handle:
    reader = csv.DictReader(handle)
    identity = ("child_height", "child_block_hash", "child_header_hex")
    required = ("classification", *identity)
    missing = [name for name in required if name not in (reader.fieldnames or [])]
    if missing:
        raise SystemExit(f"{path}: missing columns: {', '.join(missing)}")
    print(sum(
        1
        for row in reader
        if (row.get("classification") or "").strip() == "canonical"
        and not any((row.get(name) or "").strip() for name in identity)
    ))
PY
}

manifest_checksum_path() {
    case "$1" in
        *.json) printf '%s.sha256\n' "${1%.json}" ;;
        *) printf '%s.sha256\n' "$1" ;;
    esac
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is unavailable: $1"
}

repo_dir="${MERGE_MINING_RESEARCH_DIR:-}"
output="data/historical/historical-source-manifest.json"
source_commit=""
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
        --source-commit)
            [ "$#" -ge 2 ] || die "--source-commit requires a value"
            source_commit="$2"
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

require_command jq
require_command python3

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

if [ -z "${source_commit}" ]; then
    [ -f "${output}" ] || die "pass --source-commit when generating a new manifest"
    source_commit="$(jq -er '.source_repo_commit' "${output}")" \
        || die "${output} has no source_repo_commit"
fi
source_commit="$(git -C "${repo_dir}" rev-parse "${source_commit}^{commit}")" \
    || die "source commit is unavailable: ${source_commit}"

publication_manifest_path="results/monitor-evidence/monitor-evidence-manifest.json"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/historical-source-manifest.XXXXXX")"
cleanup() {
    rm -rf "${scratch}"
}
trap cleanup EXIT

research_manifest="${scratch}/research-manifest.json"
git -C "${repo_dir}" show "${source_commit}:${publication_manifest_path}" \
    >"${research_manifest}" \
    || die "${publication_manifest_path} is missing at ${source_commit}"
jq -e '.artifacts and .counts' "${research_manifest}" >/dev/null \
    || die "research publication manifest is missing artifacts or counts"

error_observation_source_chain_counts="$(jq -cer '
    .observation_chain_counts["error-block-observations"]
    | if type == "object" and length > 0 then
        .
      else
        error("missing error-observation source-chain inventory")
      end
' "${research_manifest}")" \
    || die "research publication manifest has no error-observation source-chain inventory"

artifacts_ndjson="${scratch}/artifacts.ndjson"
: >"${artifacts_ndjson}"
while IFS= read -r chain; do
    csv_path="$(jq -er --arg chain "${chain}" '.artifacts[$chain]' "${research_manifest}")"
    pointer="${scratch}/${chain}.pointer"
    git -C "${repo_dir}" show "${source_commit}:${csv_path}" >"${pointer}" \
        || die "${csv_path} is missing at ${source_commit}"
    grep -qx 'version https://git-lfs.github.com/spec/v1' <(sed -n '1p' "${pointer}") \
        || die "${csv_path} is not a Git LFS pointer at ${source_commit}"
    oid="$(sed -n 's/^oid sha256:\([0-9a-f]\{64\}\)$/\1/p' "${pointer}")"
    size="$(sed -n 's/^size \([0-9][0-9]*\)$/\1/p' "${pointer}")"
    [ -n "${oid}" ] || die "${csv_path} has no valid LFS oid"
    [ -n "${size}" ] || die "${csv_path} has no valid LFS size"
    count="$(jq -cer --arg chain "${chain}" '
        [.counts[] | select(.chain == $chain)] as $rows
        | if ($rows | length) != 1 then
            error("missing or duplicate count row")
          else
            $rows[0]
          end
    ' "${research_manifest}")" || die "invalid count row for ${chain}"
    role="event"
    if [ "${chain}" = "stale-descendants" ]; then
        role="aggregate"
    elif [ "${chain}" = "error-block-observations" ]; then
        role="error_observation"
    fi
    source_chain_counts='{}'
    if [ "${role}" = "error_observation" ]; then
        source_chain_counts="${error_observation_source_chain_counts}"
    fi
    parent_only_rows=0
    if [ "${role}" = "event" ]; then
        materialized_path="${repo_dir}/${csv_path}"
        materialized_available=1
        if [ ! -f "${materialized_path}" ]; then
            materialized_available=0
        elif grep -qx 'version https://git-lfs.github.com/spec/v1' <(sed -n '1p' "${materialized_path}"); then
            materialized_available=0
        fi
        if [ "${check}" -eq 1 ] && [ "${allow_missing_repo}" -eq 1 ] \
            && [ "${materialized_available}" -eq 0 ]; then
            parent_only_rows="$(jq -er --arg chain "${chain}" '
                .artifacts[]
                | select(.role == "event" and .chain == $chain)
                | .parent_only_rows
            ' "${output}")" || die "${output} has no parent-only count for ${chain}"
        else
            [ -f "${materialized_path}" ] \
                || die "${csv_path} is not materialized; run git -C ${repo_dir} lfs pull --include=${csv_path}"
            grep -qx 'version https://git-lfs.github.com/spec/v1' <(sed -n '1p' "${materialized_path}") \
                && die "${csv_path} is still a Git LFS pointer; run git -C ${repo_dir} lfs pull --include=${csv_path}"
            actual_size="$(wc -c <"${materialized_path}" | tr -d '[:space:]')"
            [ "${actual_size}" = "${size}" ] \
                || die "${csv_path} materialized size does not match ${source_commit}"
            [ "$(sha256_file "${materialized_path}")" = "${oid}" ] \
                || die "${csv_path} materialized checksum does not match ${source_commit}"
            parent_only_rows="$(count_parent_only_rows "${materialized_path}")" \
                || die "cannot count parent-only rows in ${csv_path}"
        fi
    fi
    jq -cn \
        --arg chain "${chain}" \
        --arg csv_path "${csv_path}" \
        --arg role "${role}" \
        --arg sha256 "${oid}" \
        --argjson size_bytes "${size}" \
        --argjson count "${count}" \
        --argjson parent_only_rows "${parent_only_rows}" \
        --argjson source_chain_counts "${source_chain_counts}" '
        {
          chain: $chain,
          csv_path: $csv_path,
          role: $role,
          row_count: $count.monitor_rows,
          parent_only_rows: $parent_only_rows,
          size_bytes: $size_bytes,
          sha256: $sha256,
          counts: {
            canonical: $count.canonical,
            stale: $count.stale,
            stale_descendant: $count.stale_descendant,
            strict_btc_orphan: $count.strict_btc_orphan,
            weak_btc_orphan: $count.weak_btc_orphan
          }
        }
        | if $role == "error_observation" then
            . + {
              source_chain_counts: $source_chain_counts,
              counts: (.counts + { error_block: $count.error_block })
            }
          else
            .
          end
    ' >>"${artifacts_ndjson}"
done < <(jq -r '.artifacts | keys[]' "${research_manifest}")

generated="${scratch}/generated.json"
jq -S \
    --arg source_commit "${source_commit}" \
    --arg publication_manifest_path "${publication_manifest_path}" \
    --arg publication_manifest_sha256 "$(sha256_file "${research_manifest}")" \
    --slurpfile artifacts "${artifacts_ndjson}" '
    ($artifacts) as $items
    | ([ $items[] | select(.role == "error_observation") ]) as $error_artifacts
    | if ($error_artifacts | length) > 1 then
        error("multiple error-observation artifacts")
      else
        .
      end
    | {
        schema_version: 2,
        scope: "uniform_monitor_evidence_v1",
        source_repo: "merge-mining-research",
        source_repo_commit: $source_commit,
        publication_manifest_path: $publication_manifest_path,
        publication_manifest_sha256: $publication_manifest_sha256,
        manifest_generator: "scripts/gen-historical-source-manifest.sh",
        total_event_rows: (
          [$items[] | select(.role == "event") | .row_count] | add
        ),
        aggregate_rows: (
          [$items[] | select(.role == "aggregate") | .row_count] | add
        ),
        required_columns: [
          "chain",
          "source_kind",
          "source_path",
          "source_row_number",
          "artifact_scope",
          "provenance",
          "child_height",
          "child_block_hash",
          "child_header_hex",
          "child_block_time",
          "child_nbits",
          "btc_height",
          "btc_header_hash",
          "btc_prev_hash",
          "btc_time",
          "btc_bits",
          "btc_nonce",
          "btc_header_hex",
          "coinbase_scriptsig_hex",
          "coinbase_outputs",
          "full_coinbase_hex",
          "classification",
          "validation_status",
          "expected_nbits",
          "rejection_reason",
          "btc_stale_relevance",
          "relevance_reason"
        ],
        artifacts: $items
      }
    | if ($error_artifacts | length) == 1 then
        . + { error_observation_rows: $error_artifacts[0].row_count }
      else
        .
      end
' "${research_manifest}" >"${generated}"

checksum_output="$(manifest_checksum_path "${output}")"
if [ "${check}" -eq 1 ]; then
    [ -f "${output}" ] || die "missing committed manifest ${output}"
    cmp -s "${generated}" "${output}" \
        || die "historical source manifest drifted: ${output}"
    [ -f "${checksum_output}" ] \
        || die "missing historical source manifest checksum ${checksum_output}"
    expected_checksum="$(sed -n '1{s/[[:space:]].*$//;p;q;}' "${checksum_output}")"
    actual_checksum="$(sha256_file "${output}")"
    [ "${expected_checksum}" = "${actual_checksum}" ] \
        || die "historical source manifest checksum drifted: ${checksum_output}"
    printf 'historical source manifest is up to date: %s\n' "${output}"
else
    mv -f "${generated}" "${output}"
    chmod 0644 "${output}"
    printf '%s\n' "$(sha256_file "${output}")" >"${checksum_output}"
    chmod 0644 "${checksum_output}"
    printf 'wrote %s\n' "${output}"
    printf 'wrote %s\n' "${checksum_output}"
fi
