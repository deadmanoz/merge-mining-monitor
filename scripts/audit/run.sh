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

section() { printf '\n==================== %s ====================\n' "$1"; }

section "STRUCTURAL (identifier-normalized) CLONES"
python3 "$here/clones.py" "$root" || true

section "SQL-LITERAL DUPLICATION"
python3 "$here/sqldup.py" "$root" || true

section "SCATTERED CONFIG + DOC DRIFT"
python3 "$here/configscan.py" "$root" || true

section "FACADE-ABSTRACTION CANDIDATES (trait surface)"
python3 "$here/traits.py" "$root" || true

section "INTRA-CRATE MODULE CYCLES"
python3 "$here/modules.py" "$root" || true

section "NAMING-PATTERN CLUSTERS"
python3 "$here/naming.py" "$root" || true

section "COMPLEXITY HOTSPOTS (decision-point proxy)"
python3 "$here/complexity.py" "$root" || true

section "GIT CHURN + TEMPORAL COUPLING"
python3 "$here/coupling.py" || true

echo
echo "For one ranked report (Markdown or --json data contract):"
echo "  python3 $here/report.py $root         # ranked Markdown"
echo "  python3 $here/report.py $root --json   # findings data contract for an LLM/tool"
