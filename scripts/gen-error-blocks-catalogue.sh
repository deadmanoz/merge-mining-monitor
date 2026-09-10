#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'USAGE'
Usage: scripts/gen-error-blocks-catalogue.sh [--check] [--allow-missing-repo] [--repo-dir DIR] [--source-commit COMMIT] [--historical-manifest PATH] [--out PATH]

Generate or verify the compact pinned mirror of merge-mining-research
data/error-blocks/error_blocks.csv.

Options:
  --check         Compare generated output with the committed catalogue
  --allow-missing-repo
                  In --check mode, skip when the source clone is unavailable
  --repo-dir DIR  merge-mining-research clone (default: $MERGE_MINING_RESEARCH_DIR)
  --source-commit COMMIT
                  Research commit (default: the committed catalogue pin).
                  Must match source_repo_commit in
                  data/historical/historical-source-manifest.json.
  --historical-manifest PATH
                  Manifest whose Research pin must match (default:
                  data/historical/historical-source-manifest.json).
  --out PATH      Output path (default: data/consensus/error_blocks.csv)
USAGE
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

skip_check() {
    printf 'error-blocks catalogue check skipped: %s\n' "$*" >&2
    exit 0
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || die "required command is unavailable: $1"
}

committed_source_commit() {
    local path="$1"
    [ -f "${path}" ] || die "pass --source-commit when generating a new catalogue"
    sed -n 's/^# Source commit: \([0-9a-f]\{40\}\)$/\1/p' "${path}" | head -n 1
}

historical_manifest_commit() {
    local path="$1"
    [ -f "${path}" ] || die "missing historical source manifest ${path}"
    python3 - "${path}" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
try:
    payload = json.loads(path.read_text())
except json.JSONDecodeError as exc:
    raise SystemExit(f"{path}: invalid JSON: {exc}") from exc
commit = str(payload.get("source_repo_commit") or "").strip()
if not commit:
    raise SystemExit(f"{path}: missing source_repo_commit")
print(commit)
PY
}

require_matching_publication_pin() {
    local selected="$1"
    local manifest_commit="$2"
    [ "${selected}" = "${manifest_commit}" ] || die \
        "error-blocks catalogue source commit ${selected} does not match ${historical_manifest} source_repo_commit ${manifest_commit}; pin both artifacts to the same research SHA"
}

repo_dir="${MERGE_MINING_RESEARCH_DIR:-}"
output="data/consensus/error_blocks.csv"
historical_manifest="data/historical/historical-source-manifest.json"
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
        --historical-manifest)
            [ "$#" -ge 2 ] || die "--historical-manifest requires a value"
            historical_manifest="$2"
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

require_command python3

manifest_commit="$(historical_manifest_commit "${historical_manifest}")"

if [ -z "${repo_dir}" ]; then
    if [ "${check}" -eq 1 ] && [ "${allow_missing_repo}" -eq 1 ]; then
        if [ -z "${source_commit}" ]; then
            source_commit="$(committed_source_commit "${output}")"
            [ -n "${source_commit}" ] || die "${output} has no Source commit pin"
        fi
        require_matching_publication_pin "${source_commit}" "${manifest_commit}"
        skip_check "source repo not configured; set MERGE_MINING_RESEARCH_DIR or pass --repo-dir"
    fi
    die "source repo not configured; set MERGE_MINING_RESEARCH_DIR or pass --repo-dir"
fi
if ! git -C "${repo_dir}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    if [ "${check}" -eq 1 ] && [ "${allow_missing_repo}" -eq 1 ]; then
        if [ -z "${source_commit}" ]; then
            source_commit="$(committed_source_commit "${output}")"
            [ -n "${source_commit}" ] || die "${output} has no Source commit pin"
        fi
        require_matching_publication_pin "${source_commit}" "${manifest_commit}"
        skip_check "source repo unavailable: ${repo_dir}"
    fi
    die "not a git work tree: ${repo_dir}"
fi

if [ -z "${source_commit}" ]; then
    source_commit="$(committed_source_commit "${output}")"
    [ -n "${source_commit}" ] || die "${output} has no Source commit pin"
fi
source_commit="$(git -C "${repo_dir}" rev-parse "${source_commit}^{commit}")" \
    || die "source commit is unavailable: ${source_commit}"
manifest_commit="$(git -C "${repo_dir}" rev-parse "${manifest_commit}^{commit}")" \
    || die "historical manifest pin is unavailable: ${manifest_commit}"
require_matching_publication_pin "${source_commit}" "${manifest_commit}"

catalogue_path="data/error-blocks/error_blocks.csv"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/error-blocks-catalogue.XXXXXX")"
cleanup() {
    rm -rf "${scratch}"
}
trap cleanup EXIT

research_catalogue="${scratch}/error_blocks.csv"
git -C "${repo_dir}" show "${source_commit}:${catalogue_path}" \
    >"${research_catalogue}" \
    || die "${catalogue_path} is missing at ${source_commit}"

generated="${scratch}/generated.csv"
{
    printf '%s\n' \
        "# Mirror of deadmanoz/merge-mining-research data/error-blocks/error_blocks.csv" \
        "# Source commit: ${source_commit}" \
        "# Fields intentionally retained here: canonical BTC height, display-order hash," \
        "# and the primary mechanically re-derived consensus violation." \
        "height,hash,rejection_reason"
    python3 - "${research_catalogue}" <<'PY'
import csv
import sys
from pathlib import Path

path = Path(sys.argv[1])
with path.open(newline="") as handle:
    reader = csv.DictReader(handle)
    required = ("height", "hash", "rejection_reason")
    missing = [name for name in required if name not in (reader.fieldnames or [])]
    if missing:
        raise SystemExit(f"missing columns: {', '.join(missing)}")
    rows = 0
    for row_number, row in enumerate(reader, start=2):
        height = (row.get("height") or "").strip()
        digest = (row.get("hash") or "").strip().lower()
        reason = (row.get("rejection_reason") or "").strip()
        if not height or not digest or not reason:
            raise SystemExit(f"{path}:{row_number}: incomplete compact catalogue fields")
        if any("," in value for value in (height, digest, reason)):
            raise SystemExit(f"{path}:{row_number}: compact fields must not contain commas")
        print(f"{height},{digest},{reason}")
        rows += 1
    if rows == 0:
        raise SystemExit(f"{path}: catalogue is empty")
PY
} >"${generated}"

if [ "${check}" -eq 1 ]; then
    [ -f "${output}" ] || die "missing committed catalogue ${output}"
    cmp -s "${generated}" "${output}" \
        || die "error-blocks catalogue drifted: ${output}"
    printf 'error-blocks catalogue is up to date: %s\n' "${output}"
else
    mv -f "${generated}" "${output}"
    chmod 0644 "${output}"
    printf 'wrote %s\n' "${output}"
fi
