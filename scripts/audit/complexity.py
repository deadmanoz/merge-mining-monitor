#!/usr/bin/env python3
"""Control-flow density per Rust fn: a cheap cyclomatic-complexity proxy.

The workspace already gates function *length* (`too_many_lines = 100`) and runs
an advisory `cognitive_complexity` pass in `just arch-lint`. This adds a third
lens: raw decision-point count, which flags functions that are branch-dense
(simplification candidates) even when they are within the line budget.

A proxy, not a real metric - it counts control-flow tokens, `&&`/`||`, `?`, and
`.await`. Stdlib-only.
"""

from __future__ import annotations

import argparse
import re

import _scan

DECISION = re.compile(r"\b(if|match|for|while|loop)\b|&&|\|\||\?\s*[.;)]|=>|\.await")


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--min", type=int, default=25, help="only show functions with >= N decision points (default: 25)")
    ap.add_argument("--limit", type=int, default=25, help="max rows (default: 25)")
    ap.add_argument("--include-tests", action="store_true")
    args = ap.parse_args()

    rows = []
    for fn in _scan.load_functions(args.root, skip_tests=not args.include_tests):
        # Skip inline unit tests that survive file-level filtering.
        if fn.name.startswith("test") or "#[test]" in fn.body:
            continue
        dp = len(DECISION.findall(fn.body))
        if dp >= args.min:
            rows.append((dp, fn.body.count("\n") + 1, fn.name, fn.path, fn.line))
    rows.sort(reverse=True)
    print("  dp  lines  function (file:line)")
    for dp, lines, name, path, line in rows[: args.limit]:
        print(f"{dp:4d}  {lines:5d}  {name} ({path}:{line})")
    print(f"# {len(rows)} functions >= {args.min} decision points")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
