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

section "NAMING-PATTERN CLUSTERS"
python3 "$here/naming.py" "$root" || true

section "COMPLEXITY HOTSPOTS (decision-point proxy)"
python3 "$here/complexity.py" "$root" || true

section "GIT CHURN + TEMPORAL COUPLING"
python3 "$here/coupling.py" || true
