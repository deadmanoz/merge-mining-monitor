#!/usr/bin/env python3
"""Git behavioral analysis: change churn + temporal coupling.

Static tools see the code as it is now; git history shows how it *evolves*. Two
files that keep changing in the same commits are "temporally coupled" - often a
sign of a concept smeared across modules (a hidden shared responsibility) even
when there is no textual duplication for jscpd or `clones.py` to catch.

Reports:
  * CHURN    - files changed most often (where duplication costs the most).
  * COUPLING - file pairs that co-change, with a coupling ratio
               co_changes / min(churn_a, churn_b). A high ratio means "when A
               changes, B almost always changes too."

Reads `git log`; run inside the repo. Stdlib-only.
"""

from __future__ import annotations

import argparse
import subprocess
from collections import Counter
from itertools import combinations

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


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--min-co", type=int, default=5, help="minimum co-changes for a coupled pair (default: 5)")
    ap.add_argument("--min-ratio", type=float, default=0.6, help="minimum coupling ratio (default: 0.6)")
    ap.add_argument("--max-commit-files", type=int, default=30, help="ignore commits touching more than N tracked files, so sweeping mechanical commits don't create phantom coupling (default: 30)")
    ap.add_argument("--limit", type=int, default=25)
    args = ap.parse_args()

    churn: Counter[str] = Counter()
    co: Counter[tuple[str, str]] = Counter()
    for files in commits():
        uniq = sorted(set(files))
        churn.update(uniq)
        if len(uniq) > args.max_commit_files:
            continue
        for a, b in combinations(uniq, 2):
            co[(a, b)] += 1

    print("=== CHURN (most-changed tracked files) ===")
    for path, n in churn.most_common(args.limit):
        print(f"{n:4d}  {path}")

    print(f"\n=== TEMPORAL COUPLING (co-changes >= {args.min_co}, ratio >= {args.min_ratio}) ===")
    scored = []
    for (a, b), n in co.items():
        if n < args.min_co:
            continue
        ratio = n / min(churn[a], churn[b])
        if ratio >= args.min_ratio:
            scored.append((ratio, n, a, b))
    # Rank by absolute co-change count first: a high ratio from only a handful of
    # co-changes is usually the artifact of files introduced in the same burst,
    # whereas many co-changes with a strong ratio is real hidden coupling.
    scored.sort(key=lambda x: (x[1], x[0]), reverse=True)
    for ratio, n, a, b in scored[: args.limit]:
        cross = "XDIR " if a.rsplit("/", 1)[0] != b.rsplit("/", 1)[0] else "     "
        print(f"{ratio:.2f}  n={n:<3d} {cross} {a}  <=>  {b}")
    print(f"# {len(scored)} coupled pairs (XDIR = different directories, the more interesting ones)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
