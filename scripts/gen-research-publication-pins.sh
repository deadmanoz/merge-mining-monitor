#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

usage() {
    cat <<'USAGE'
Usage: scripts/gen-research-publication-pins.sh [--check] [--allow-missing-repo] [--repo-dir DIR] [--source-commit COMMIT]

Generate or verify the historical manifest, compact error-block catalogue,
and compact body-invalid stales mirror from one Research commit. Generation
stages every output before publishing them; --check runs every check and
fails if any one fails.

Output paths are managed by this command and cannot be overridden.
USAGE
}

check=0
has_source_commit=0
for arg in "$@"; do
    case "${arg}" in
        --check) check=1 ;;
        --source-commit) has_source_commit=1 ;;
        --out|--out=*|--historical-manifest|--historical-manifest=*)
            echo "error: output paths are managed by the combined pin generator" >&2
            exit 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
    esac
done

args=("$@")
if [ "${has_source_commit}" -eq 0 ] && [ "${check}" -eq 0 ]; then
    source_commit="$(jq -er '.source_repo_commit' data/historical/historical-source-manifest.json)"
    args+=(--source-commit "${source_commit}")
fi

if [ "${check}" -eq 1 ]; then
    status=0
    ./scripts/gen-historical-source-manifest.sh "${args[@]}" || status=1
    ./scripts/gen-error-blocks-catalogue.sh "${args[@]}" || status=1
    ./scripts/gen-body-invalid-stales.sh "${args[@]}" || status=1
    exit "${status}"
fi

mkdir -p "${repo_root}/.tmp"
scratch="$(mktemp -d "${repo_root}/.tmp/publication-pins.XXXXXX")"
cleanup() {
    rm -rf "${scratch}"
}
trap cleanup EXIT

manifest="${scratch}/historical-source-manifest.json"
catalogue="${scratch}/error_blocks.csv"
body_invalid="${scratch}/body_invalid_stales.csv"
./scripts/gen-historical-source-manifest.sh "${args[@]}" --out "${manifest}"
./scripts/gen-error-blocks-catalogue.sh \
    "${args[@]}" \
    --historical-manifest "${manifest}" \
    --out "${catalogue}"
./scripts/gen-body-invalid-stales.sh \
    "${args[@]}" \
    --historical-manifest "${manifest}" \
    --out "${body_invalid}"

mv -f "${manifest}" data/historical/historical-source-manifest.json
mv -f "${manifest%.json}.sha256" data/historical/historical-source-manifest.sha256
mv -f "${catalogue}" data/consensus/error_blocks.csv
mv -f "${body_invalid}" data/consensus/body_invalid_stales.csv
printf 'published all staged Research pins\n'
