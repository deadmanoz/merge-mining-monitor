#!/usr/bin/env python3
"""Cluster parallel function families by name skeleton.

Collapses domain tokens (chain names, entity nouns) in every `fn` name to `*`,
then groups the results. A cluster like `run_*_backfill -> {rsk, hathor,
elastos}` is a cheap hint that those bodies may share structure worth a shared
helper - cross-check with `clones.py`. Stdlib-only.
"""

from __future__ import annotations

import argparse
import re
from collections import defaultdict

import _scan

# Domain tokens that vary across otherwise-parallel functions. Hand-tuned to this
# codebase; extend as new chains/entities appear.
DOMAIN = {
    "namecoin", "rsk", "syscoin", "fractal", "hathor", "elastos", "bitcoin",
    "core", "auxpow", "stale", "orphan", "error", "block", "blocks", "parent",
    "child", "source", "sources", "competition", "competitions", "tree", "delta",
    "navigator", "branch", "known",
}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--min", type=int, default=3, help="minimum family size to report (default: 3)")
    ap.add_argument("--include-tests", action="store_true")
    args = ap.parse_args()

    names = []
    for path in _scan.iter_rust_files(args.root, skip_tests=not args.include_tests):
        src = open(path, encoding="utf-8", errors="ignore").read()
        names += re.findall(r"\bfn\s+([a-z][a-z0-9_]+)", src)

    skeletons = defaultdict(set)
    for n in names:
        key = tuple("*" if p in DOMAIN else p for p in n.split("_"))
        skeletons[key].add(n)

    rows = sorted(((len(v), k, sorted(v)) for k, v in skeletons.items() if len(v) >= args.min), reverse=True)
    for cnt, key, members in rows:
        print("%2d  %-32s -> %s" % (cnt, "_".join(key), ", ".join(members)))
    print(f"# {len(rows)} parallel families (>= {args.min} members)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
