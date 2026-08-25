#!/usr/bin/env python3
"""Git behavioral analysis: change churn + temporal coupling.

Static tools see the code as it is now; git history shows how it *evolves*. Two
files that keep changing in the same commits are "temporally coupled" - often a
sign of a concept smeared across modules (a hidden shared responsibility) even
when there is no textual duplication for jscpd or `clones.py` to catch.

Reports CHURN (most-changed files) and COUPLING (co-changing pairs, with a
coupling ratio co_changes / min(churn_a, churn_b)). Reads `git log`; run inside
the repo. Advisory; stdlib-only. --json supported.
"""

from __future__ import annotations

import argparse
import subprocess
from collections import Counter
from itertools import combinations

import _report

TRACKED_SUFFIXES = (".rs", ".js")
EXCLUDE = ("generated", "Cargo.lock", "/vendor/", "node_modules/")


def commits():
    out = subprocess.run(
        ["git", "log", "--all", "--pretty=format:%H", "--name-only"],
        capture_output=True, text=True, check=True,
    ).stdout
    files: list[str] = []
    for line in out.splitlines():
        if not line.strip():
            continue
        if len(line) == 40 and all(c in "0123456789abcdef" for c in line):
            if files:
                yield files
            files = []
        elif line.endswith(TRACKED_SUFFIXES) and not any(e in line for e in EXCLUDE):
            files.append(line)
    if files:
        yield files


def collect(min_co: int = 5, min_ratio: float = 0.6, max_commit_files: int = 30) -> tuple[list[_report.Finding], Counter]:
    churn: Counter[str] = Counter()
    co: Counter[tuple[str, str]] = Counter()
    for files in commits():
        uniq = sorted(set(files))
        churn.update(uniq)
        if len(uniq) > max_commit_files:
            continue
        for a, b in combinations(uniq, 2):
            co[(a, b)] += 1

    findings: list[_report.Finding] = []
    for (a, b), n in co.items():
        if n < min_co:
            continue
        ratio = n / min(churn[a], churn[b])
        if ratio < min_ratio:
            continue
        cross_dir = a.rsplit("/", 1)[0] != b.rsplit("/", 1)[0]
        findings.append(_report.Finding(
            tool="coupling", kind="temporal-coupling",
            summary=f"{a} <=> {b} co-changed {n}x (ratio {ratio:.2f}{', cross-dir' if cross_dir else ''})",
            score=round(ratio, 4), severity="medium" if cross_dir and n >= min_co else "low",
            locations=[_report.Loc(a), _report.Loc(b)],
            metrics={"co_changes": n, "ratio": round(ratio, 4), "cross_dir": cross_dir},
        ))
    findings.sort(key=lambda f: (f.metrics["co_changes"], f.score), reverse=True)
    return findings, churn


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--min-co", type=int, default=5, help="minimum co-changes for a coupled pair (default: 5)")
    ap.add_argument("--min-ratio", type=float, default=0.6, help="minimum coupling ratio (default: 0.6)")
    ap.add_argument("--max-commit-files", type=int, default=30, help="ignore commits touching more than N tracked files (default: 30)")
    ap.add_argument("--limit", type=int, default=25)
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings, churn = collect(args.min_co, args.min_ratio, args.max_commit_files)
    if args.json:
        _report.print_json(findings)
        return 0

    print("=== CHURN (most-changed tracked files) ===")
    for path, n in churn.most_common(args.limit):
        print(f"{n:4d}  {path}")
    print(f"\n=== TEMPORAL COUPLING (co-changes >= {args.min_co}, ratio >= {args.min_ratio}) ===")
    for f in findings[: args.limit]:
        a, b = f.locations
        cross = "XDIR " if f.metrics["cross_dir"] else "     "
        print(f"{f.score:.2f}  n={f.metrics['co_changes']:<3d} {cross} {a.file}  <=>  {b.file}")
    print(f"# {len(findings)} coupled pairs (XDIR = different directories, the more interesting ones)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
