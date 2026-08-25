#!/usr/bin/env python3
"""Control-flow density per Rust fn: a cheap cyclomatic-complexity proxy.

The workspace already gates function *length* (`too_many_lines = 100`) and runs
an advisory `cognitive_complexity` pass in `just arch-lint`. This adds a third
lens: raw decision-point count, which flags functions that are branch-dense
(simplification candidates) even when they are within the line budget.

A proxy, not a real metric - it counts control-flow tokens, `&&`/`||`, `?`, and
`.await`. Advisory; stdlib-only. --json supported.
"""

from __future__ import annotations

import argparse
import re

import _report
import _scan

# Count every `?` try operator (on strip_noise'd source, so not in strings or
# comments), excluding only the `?Sized` relaxed-bound, which is not a branch.
DECISION = re.compile(r"\b(if|match|for|while|loop)\b|&&|\|\||\?(?!\s*Sized\b)|=>|\.await")


def _severity(dp: int) -> str:
    if dp >= 40:
        return "high"
    if dp >= 30:
        return "medium"
    return "low"


def collect(root: str, min_dp: int = 25, include_tests: bool = False) -> list[_report.Finding]:
    findings: list[_report.Finding] = []
    for fn in _scan.load_functions(root, skip_tests=not include_tests):
        if fn.name.startswith("test") or "#[test]" in fn.body:
            continue
        dp = len(DECISION.findall(fn.body))
        if dp < min_dp:
            continue
        lines = fn.body.count("\n") + 1
        findings.append(_report.Finding(
            tool="complexity", kind="complexity-hotspot",
            summary=f"{fn.name} has {dp} decision points across {lines} lines",
            score=float(dp), severity=_severity(dp),
            locations=[_report.Loc(fn.path, fn.line, fn.name)],
            metrics={"decision_points": dp, "lines": lines},
        ))
    findings.sort(key=lambda f: f.score, reverse=True)
    return findings


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--min", type=int, default=25, help="only show functions with >= N decision points (default: 25)")
    ap.add_argument("--limit", type=int, default=25, help="max rows (default: 25)")
    ap.add_argument("--include-tests", action="store_true")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.min, args.include_tests)
    if args.json:
        _report.print_json(findings)
        return 0
    print("  dp  lines  function (file:line)")
    for f in findings[: args.limit]:
        loc = f.locations[0]
        print(f"{int(f.score):4d}  {f.metrics['lines']:5d}  {loc.name} ({loc.file}:{loc.line})")
    print(f"# {len(findings)} functions >= {args.min} decision points")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
