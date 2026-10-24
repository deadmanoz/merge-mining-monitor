#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
manifest="${repo_root}/data/historical/historical-source-manifest.json"
checksum="${repo_root}/data/historical/historical-source-manifest.sha256"

# shellcheck source=scripts/lib/research-pins.sh
source "${repo_root}/scripts/lib/research-pins.sh"

require_command jq
[ -f "${manifest}" ] || die "missing ${manifest}"
[ -f "${checksum}" ] || die "missing ${checksum}"

jq -e '
    .schema_version == 2
    and .source_repo == "merge-mining-research"
    and (.source_repo_commit | test("^[0-9a-f]{40}$"))
    and (.publication_manifest_sha256 | test("^[0-9a-f]{64}$"))
    and .total_event_rows == ([.artifacts[] | select(.role == "event") | .row_count] | add)
    and .aggregate_rows == ([.artifacts[] | select(.role == "aggregate") | .row_count] | add)
    and ([.artifacts[] | select(.role == "event")] | length) == 29
    and ([.artifacts[] | select(.role == "aggregate" and .chain == "stale-descendants")] | length) == 1
    and ([.artifacts[] | select(.role == "invalid_observation" and .chain == "error-block-observations")] | length) == 1
    and .invalid_observation_rows == ([.artifacts[] | select(.role == "invalid_observation") | .row_count] | add)
    and (.artifacts | length) == 31
    and ([.artifacts[].chain] | unique | length) == (.artifacts | length)
    and all(.artifacts[];
        (.sha256 | test("^[0-9a-f]{64}$"))
        and (.size_bytes >= 0)
        and (.parent_only_rows >= 0)
        and (if .role == "invalid_observation" then
            (.row_count == .counts.invalid_block)
            and ([.source_chain_counts[]] | add) == .row_count
          else
            .row_count == (
                .counts.canonical
                + .counts.stale
                + .counts.stale_descendant
                + .counts.strict_btc_orphan
                + .counts.weak_btc_orphan
            )
          end)
        and (if .role == "event" then
            .parent_only_rows <= .counts.canonical
          else
            .parent_only_rows == 0
          end)
    )
' "${manifest}" >/dev/null || die "committed publication manifest is invalid"

expected_checksum="$(sed -n '1{s/[[:space:]].*$//;p;q;}' "${checksum}")"
[ "${expected_checksum}" = "$(sha256_file "${manifest}")" ] \
    || die "manifest checksum does not match ${checksum}"

printf 'historical source manifest self-test passed\n'
