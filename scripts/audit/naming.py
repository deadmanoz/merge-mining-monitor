#!/usr/bin/env python3
"""Cluster parallel function families by name skeleton.

Collapses domain tokens (chain names, entity nouns) in every `fn` name to `*`,
then groups the results. A cluster like `run_*_backfill -> {rsk, hathor,
elastos}` is a cheap hint that those bodies may share structure worth a shared
helper - cross-check with `clones.py`. Advisory; stdlib-only. --json supported.
"""

from __future__ import annotations

import argparse
import re
from collections import defaultdict

import _report
import _scan

# Domain tokens that vary across otherwise-parallel functions. Hand-tuned to this
# codebase; extend as new chains/entities appear.
DOMAIN = {
    "namecoin", "rsk", "syscoin", "fractal", "hathor", "elastos", "bitcoin",
    "core", "auxpow", "stale", "orphan", "error", "block", "blocks", "parent",
    "child", "source", "sources", "competition", "competitions", "tree", "delta",
    "navigator", "branch", "known",
}


def collect(root: str, min_family: int = 3, include_tests: bool = False) -> list[_report.Finding]:
    names_at: dict[str, list[_report.Loc]] = defaultdict(list)
    for path in _scan.iter_rust_files(root, skip_tests=not include_tests):
        # De-noise first: prose like `// this fn only maps` or a `"fn foo"` string
        # would otherwise be inventoried as a real function and invent a family.
        src = _scan.strip_noise(open(path, encoding="utf-8", errors="ignore").read())
        for m in re.finditer(r"\bfn\s+([a-z][a-z0-9_]+)", src):
            names_at[m.group(1)].append(_report.Loc(_scan.rel(path), src.count("\n", 0, m.start()) + 1, m.group(1)))

    skeletons: dict[tuple, set] = defaultdict(set)
    for n in names_at:
        key = tuple("*" if p in DOMAIN else p for p in n.split("_"))
        skeletons[key].add(n)

    findings: list[_report.Finding] = []
    for key, members in skeletons.items():
        if len(members) < min_family:
            continue
        locs = [loc for n in sorted(members) for loc in names_at[n][:1]]
        findings.append(_report.Finding(
            tool="naming", kind="naming-family",
            summary=f"{'_'.join(key)} -> {', '.join(sorted(members))}",
            score=float(len(members)), severity="low",
            locations=locs, metrics={"skeleton": "_".join(key), "members": sorted(members)},
        ))
    findings.sort(key=lambda f: f.score, reverse=True)
    return findings


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--min", type=int, default=3, help="minimum family size to report (default: 3)")
    ap.add_argument("--include-tests", action="store_true")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.min, args.include_tests)
    if args.json:
        _report.print_json(findings)
        return 0
    for f in findings:
        print("%2d  %-32s -> %s" % (int(f.score), f.metrics["skeleton"], ", ".join(f.metrics["members"])))
    print(f"# {len(findings)} parallel families (>= {args.min} members)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
