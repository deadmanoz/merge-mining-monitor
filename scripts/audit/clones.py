#!/usr/bin/env python3
"""Structural (identifier-insensitive) clone detector for Rust.

jscpd (`.jscpd.json`) matches tokens verbatim, so `fetch_error_blocks` and
`fetch_stale_blocks` look unrelated. Here every identifier/literal is normalized
to a placeholder (see `_scan.tokenize_structural`), so renamed "Type-3" clones
surface as high-similarity function pairs.

Method: k-gram fingerprinting with a document-frequency cap.
  * Each function -> set of k-gram hashes over its normalized token stream.
  * Candidate pairs come from a *rare* k-gram inverted index (k-grams shared by
    more than --df-max functions are treated as boilerplate and skipped), which
    keeps this near-linear instead of the naive O(n^2) all-pairs compare.
  * Each candidate is scored by Jaccard similarity over full fingerprints, plus
    containment (|A n B| / min(|A|,|B|)) so a small function cloned inside a
    bigger one still shows up.

Advisory only; verify every hit by reading the code. Stdlib-only.
"""

from __future__ import annotations

import argparse
import zlib
from collections import Counter, defaultdict

import _scan


def kgrams(tokens: list[str], k: int) -> frozenset[int]:
    if len(tokens) < k:
        joined = " ".join(tokens)
        return frozenset({zlib.crc32(joined.encode())})
    out = set()
    for i in range(len(tokens) - k + 1):
        out.add(zlib.crc32(" ".join(tokens[i : i + k]).encode()))
    return frozenset(out)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates", help="directory to scan (default: crates)")
    ap.add_argument("--min-tokens", type=int, default=45, help="ignore functions shorter than this (default: 45)")
    ap.add_argument("-k", "--kgram", type=int, default=8, help="k-gram length (default: 8)")
    ap.add_argument("--df-max", type=int, default=40, help="drop k-grams shared by more than N functions from candidate generation (default: 40)")
    ap.add_argument("--min-shared", type=int, default=4, help="minimum shared rare k-grams to consider a pair (default: 4)")
    ap.add_argument("--min-jaccard", type=float, default=0.6, help="report threshold on the score (default: 0.6)")
    ap.add_argument(
        "--containment",
        action="store_true",
        help="also score by containment (|A n B| / min|A|,|B|); off by default because "
        "a small helper is 'contained' in many large functions and floods the output with noise",
    )
    ap.add_argument("--include-tests", action="store_true", help="include tests/ trees (default: excluded)")
    ap.add_argument("--limit", type=int, default=80, help="max pairs to print (default: 80)")
    args = ap.parse_args()

    funcs = _scan.load_functions(args.root, skip_tests=not args.include_tests)
    fps: list[frozenset[int]] = []
    kept: list[_scan.Function] = []
    for fn in funcs:
        toks = _scan.tokenize_structural(fn.body)
        if len(toks) >= args.min_tokens:
            kept.append(fn)
            fps.append(kgrams(toks, args.kgram))
    print(f"# {len(kept)} functions >= {args.min_tokens} tokens (of {len(funcs)} scanned)")

    # Rare k-gram inverted index for candidate generation.
    postings: dict[int, list[int]] = defaultdict(list)
    for idx, fp in enumerate(fps):
        for g in fp:
            postings[g].append(idx)
    shared: dict[tuple[int, int], int] = Counter()
    for g, members in postings.items():
        if len(members) > args.df_max:
            continue  # ubiquitous structural boilerplate; not a clone signal
        for a in range(len(members)):
            ia = members[a]
            for b in range(a + 1, len(members)):
                shared[(ia, members[b])] += 1

    results = []
    for (i, j), count in shared.items():
        if count < args.min_shared:
            continue
        fi, fj = kept[i], kept[j]
        if fi.name == fj.name and fi.path == fj.path:
            continue
        inter = len(fps[i] & fps[j])
        if not inter:
            continue
        union = len(fps[i] | fps[j])
        jac = inter / union
        contain = inter / min(len(fps[i]), len(fps[j]))
        score = max(jac, contain) if args.containment else jac
        if score >= args.min_jaccard:
            results.append((score, jac, contain, min(len(fps[i]), len(fps[j])), fi, fj))

    results.sort(key=lambda r: (-r[0], -r[3]))
    cross = sum(1 for r in results if r[4].path != r[5].path)
    for score, jac, contain, size, fi, fj in results[: args.limit]:
        tag = "XFILE" if fi.path != fj.path else "     "
        print(
            f"{score:.2f} (j={jac:.2f} c={contain:.2f}) {tag}  "
            f"{fi.name} ({fi.path}:{fi.line})  <=>  {fj.name} ({fj.path}:{fj.line})"
        )
    shown = min(len(results), args.limit)
    print(
        f"# {len(results)} pairs >= {args.min_jaccard} "
        f"({cross} cross-file, {len(results) - cross} intra-file); showing {shown}. "
        f"Cross-file pairs (XFILE) are usually the higher-value consolidation targets."
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
