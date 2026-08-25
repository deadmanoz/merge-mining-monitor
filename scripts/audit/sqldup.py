#!/usr/bin/env python3
"""Find duplicated SQL embedded in Rust string literals.

jscpd tokenizes Rust, so a multi-line SQL string is largely one opaque token and
duplication *inside* it is under-counted. Here we pull string literals that look
like SQL, normalize whitespace and `$N` placeholders, and report:

  * EXACT groups  - same normalized SQL in >= 2 places (often a probe query
                    hand-inlined everywhere; a shared helper is usually worth it).
  * NEAR pairs    - high-similarity but not identical, across different files
                    (the cross-crate ones are the interesting structural hits).

Production and test locations are tagged separately: test probe duplication is
common and lower-value than the same query hand-written across production crates.
Stdlib-only.
"""

from __future__ import annotations

import argparse
import re
from collections import defaultdict
from difflib import SequenceMatcher

import _scan

SQL_KW = re.compile(r"\b(SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|JOIN|WHERE|VALUES|ON\s+CONFLICT|RETURNING|WITH)\b", re.I)


def extract_sql(src: str):
    out = []
    # Raw strings first; then blank their spans (newlines preserved so line
    # numbers stay correct) so the normal-string pass cannot re-match the same
    # literal and report it as a phantom same-line "duplicate".
    chars = list(src)
    for m in re.finditer(r'r#*"(.*?)"#*', src, flags=re.S):
        out.append((m.group(1), src[: m.start()].count("\n") + 1))
        for k in range(m.start(), m.end()):
            if chars[k] != "\n":
                chars[k] = " "
    masked = "".join(chars)
    for m in re.finditer(r'"((?:\\.|[^"\\])*)"', masked, flags=re.S):
        out.append((m.group(1), masked[: m.start()].count("\n") + 1))
    for s, line in out:
        if len(s) >= 40 and len(SQL_KW.findall(s)) >= 2:
            yield line, s


def norm(s: str) -> str:
    s = re.sub(r"\$\d+", "$N", s)
    return re.sub(r"\s+", " ", s).strip().lower()


def is_test(path: str) -> bool:
    return "/tests/" in path or path.endswith(("tests.rs", "_tests.rs", "test_fixtures.rs"))


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--near", type=float, default=0.85, help="near-duplicate similarity floor (default: 0.85)")
    ap.add_argument("--limit", type=int, default=25, help="max near-duplicate pairs (default: 25)")
    args = ap.parse_args()

    items = []  # (path, line, normalized)
    for path in _scan.iter_rust_files(args.root, skip_tests=False):
        try:
            src = open(path, encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        for line, s in extract_sql(src):
            items.append((_scan.rel(path), line, norm(s)))

    exact = defaultdict(list)
    for path, line, n in items:
        exact[n].append((path, line))

    print("=== EXACT (whitespace/param-normalized) SQL duplicates ===")
    groups = sorted(((n, locs) for n, locs in exact.items() if len(locs) >= 2), key=lambda x: -len(x[1]))
    for n, locs in groups:
        prod = [l for l in locs if not is_test(l[0])]
        tag = "PROD" if prod else "test"
        note = f" [{len(prod)} prod]" if prod and len(prod) != len(locs) else ""
        print(f"x{len(locs):<3d} {tag}{note}  {n[:130]}")
        for p, line in (prod or locs)[:6]:
            print(f"        {p}:{line}")
    print(f"# {len(groups)} exact-duplicate SQL groups\n")

    print(f"=== NEAR-duplicate SQL (>= {args.near}, different files) ===")
    uniq = list({n: (p, line, n) for p, line, n in items}.values())
    uniq.sort(key=lambda x: len(x[2]))
    pairs = []
    sm = SequenceMatcher(None, autojunk=False)
    for i in range(len(uniq)):
        pi, li, ni = uniq[i]
        sm.set_seq2(ni)  # cache b2j once; vary seq1 in the inner loop
        for j in range(i + 1, len(uniq)):
            pj, lj, nj = uniq[j]
            if len(nj) > len(ni) * 1.3:
                break
            if pi == pj:
                continue
            sm.set_seq1(nj)
            # Cheap upper-bound prefilters before the O(len^2) ratio().
            if sm.real_quick_ratio() < args.near or sm.quick_ratio() < args.near:
                continue
            r = sm.ratio()
            if args.near <= r < 0.999:
                both_prod = not is_test(pi) and not is_test(pj)
                pairs.append((both_prod, r, pi, li, pj, lj))
    pairs.sort(key=lambda x: (x[0], x[1]), reverse=True)  # production pairs first, then by ratio
    for both_prod, r, pi, li, pj, lj in pairs[: args.limit]:
        tag = "PROD" if both_prod else "test"
        print(f"{r:.2f} {tag}  {pi}:{li}  <=>  {pj}:{lj}")
    print(f"# {len(pairs)} near-duplicate cross-file pairs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
