#!/usr/bin/env bash
# Supplementary consolidation/duplication analysis that goes beyond the
# token-literal jscpd gate (.jscpd.json). Advisory only - not a CI gate.
#
# Usage:
#   ./scripts/audit/run.sh [root]        # default root: crates
#
# Each tool takes --help and its own thresholds; see README.md.
set -euo pipefail

root="${1:-crates}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

status=0
section() { printf '\n==================== %s ====================\n' "$1"; }

# Run one detector without aborting the whole sweep on failure, but remember that
# it failed so the script's exit status reflects it (a silent `|| true` would mask
# a crashing detector and let a broken tool ship green).
run() {
  local label="$1"; shift
  section "$label"
  local rc=0
  python3 "$@" || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "!! detector failed: $(basename "$1") (exit $rc)" >&2
    status=1
  fi
}

run "STRUCTURAL (identifier-normalized) CLONES" "$here/clones.py" "$root"
run "SQL-LITERAL DUPLICATION" "$here/sqldup.py" "$root"
run "SCATTERED CONFIG + DOC DRIFT" "$here/configscan.py" "$root"
run "FACADE-ABSTRACTION CANDIDATES (trait surface)" "$here/traits.py" "$root"
run "INTRA-CRATE MODULE CYCLES" "$here/modules.py" "$root"
run "NAMING-PATTERN CLUSTERS" "$here/naming.py" "$root"
run "COMPLEXITY HOTSPOTS (decision-point proxy)" "$here/complexity.py" "$root"
run "GIT CHURN + TEMPORAL COUPLING" "$here/coupling.py" "$root"

echo
echo "For one ranked report (Markdown or --json data contract):"
echo "  python3 $here/report.py $root         # ranked Markdown"
echo "  python3 $here/report.py $root --json   # findings data contract for an LLM/tool"

if [ "$status" -ne 0 ]; then
  echo >&2
  echo "One or more detectors failed (see !! lines above)." >&2
fi
exit "$status"
