#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
scratch="$(mktemp -d "${TMPDIR:-/tmp}/research-publication-pins-test.XXXXXX")"
cleanup() {
    rm -rf "${scratch}"
}
trap cleanup EXIT

fixture="${scratch}/repo"
mkdir -p "${fixture}/scripts" "${fixture}/data/historical" "${fixture}/data/consensus"
cp "${repo_root}/scripts/gen-research-publication-pins.sh" "${fixture}/scripts/"
cat >"${fixture}/data/historical/historical-source-manifest.json" <<'JSON'
{"source_repo_commit":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","generated":false}
JSON
printf 'old-checksum\n' >"${fixture}/data/historical/historical-source-manifest.sha256"
printf '# Source commit: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nold catalogue\n' \
    >"${fixture}/data/consensus/error_blocks.csv"
printf '# Source commit: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nold body-invalid mirror\n' \
    >"${fixture}/data/consensus/body_invalid_stales.csv"
cp "${fixture}/data/historical/historical-source-manifest.json" "${scratch}/manifest.before"
cp "${fixture}/data/historical/historical-source-manifest.sha256" "${scratch}/checksum.before"
cp "${fixture}/data/consensus/error_blocks.csv" "${scratch}/catalogue.before"
cp "${fixture}/data/consensus/body_invalid_stales.csv" "${scratch}/body-invalid.before"

cat >"${fixture}/scripts/gen-historical-source-manifest.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
source_commit=""
output=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --source-commit) source_commit="$2"; shift 2 ;;
        --out) output="$2"; shift 2 ;;
        *) exit 3 ;;
    esac
done
[ "${source_commit}" = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ]
[ -n "${output}" ]
printf '{"source_repo_commit":"%s","generated":true}\n' "${source_commit}" >"${output}"
printf 'new-checksum\n' >"${output%.json}.sha256"
SH

cat >"${fixture}/scripts/gen-error-blocks-catalogue.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
source_commit=""
manifest=""
output=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --source-commit) source_commit="$2"; shift 2 ;;
        --historical-manifest) manifest="$2"; shift 2 ;;
        --out) output="$2"; shift 2 ;;
        *) exit 3 ;;
    esac
done
[ "${source_commit}" = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ]
[ -f "${manifest}" ]
[ -n "${output}" ]
printf '# Source commit: %s\nnew catalogue\n' "${source_commit}" >"${output}"
[ "${FAIL_CATALOGUE:-0}" -eq 0 ]
SH
cat >"${fixture}/scripts/gen-body-invalid-stales.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
source_commit=""
manifest=""
output=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --source-commit) source_commit="$2"; shift 2 ;;
        --historical-manifest) manifest="$2"; shift 2 ;;
        --out) output="$2"; shift 2 ;;
        *) exit 3 ;;
    esac
done
[ "${source_commit}" = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa" ]
[ -f "${manifest}" ]
[ -n "${output}" ]
printf '# Source commit: %s\nnew body-invalid mirror\n' "${source_commit}" >"${output}"
[ "${FAIL_BODY_INVALID:-0}" -eq 0 ]
SH
chmod +x "${fixture}/scripts/"*.sh

help_output="$("${fixture}/scripts/gen-research-publication-pins.sh" --help)"
grep -q '^Usage: scripts/gen-research-publication-pins.sh ' <<<"${help_output}"
if grep -q -- '--out' <<<"${help_output}"; then
    echo "combined generator help advertises caller-managed outputs" >&2
    exit 1
fi
if "${fixture}/scripts/gen-research-publication-pins.sh" --out=ignored >/dev/null 2>&1; then
    echo "combined generator accepted a caller-managed output" >&2
    exit 1
fi
if FAIL_CATALOGUE=1 "${fixture}/scripts/gen-research-publication-pins.sh" >/dev/null 2>&1; then
    echo "combined generator ignored a catalogue failure" >&2
    exit 1
fi
if FAIL_BODY_INVALID=1 "${fixture}/scripts/gen-research-publication-pins.sh" >/dev/null 2>&1; then
    echo "combined generator ignored a body-invalid mirror failure" >&2
    exit 1
fi
cmp -s "${scratch}/manifest.before" "${fixture}/data/historical/historical-source-manifest.json"
cmp -s "${scratch}/checksum.before" "${fixture}/data/historical/historical-source-manifest.sha256"
cmp -s "${scratch}/catalogue.before" "${fixture}/data/consensus/error_blocks.csv"
cmp -s "${scratch}/body-invalid.before" "${fixture}/data/consensus/body_invalid_stales.csv"

"${fixture}/scripts/gen-research-publication-pins.sh" >/dev/null
jq -e '.generated == true' "${fixture}/data/historical/historical-source-manifest.json" >/dev/null
grep -qx 'new-checksum' "${fixture}/data/historical/historical-source-manifest.sha256"
grep -qx 'new catalogue' <(tail -n 1 "${fixture}/data/consensus/error_blocks.csv")
grep -qx 'new body-invalid mirror' <(tail -n 1 "${fixture}/data/consensus/body_invalid_stales.csv")
printf 'combined Research pin generator self-test passed\n'
