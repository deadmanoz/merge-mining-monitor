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

For AST-accurate (tree-sitter) Rust clone detection, `similarity-rs` is the
graduate-to tool; this stays a zero-dependency heuristic whose niche is the
cross-crate Jaccard ranking. Advisory; verify every hit by reading the code.
"""

from __future__ import annotations

import argparse
import zlib
from collections import Counter, defaultdict

import _report
import _scan


def kgrams(tokens: list[str], k: int) -> frozenset[int]:
    if len(tokens) < k:
        return frozenset({zlib.crc32(" ".join(tokens).encode())})
    return frozenset(zlib.crc32(" ".join(tokens[i : i + k]).encode()) for i in range(len(tokens) - k + 1))


def _severity(score: float, cross_file: bool) -> str:
    # Grade on the *selected* score (Jaccard, or max(Jaccard, containment) when
    # --containment is on), so a strong containment match - a small function fully
    # inside a bigger one - is not buried at low severity by a modest Jaccard.
    if score >= 0.9:
        return "high" if cross_file else "medium"
    if score >= 0.75:
        return "medium" if cross_file else "low"
    return "low"


def collect(root: str, min_tokens: int = 45, k: int = 8, df_max: int = 40,
            min_shared: int = 4, min_jaccard: float = 0.6, containment: bool = False,
            include_tests: bool = False) -> list[_report.Finding]:
    funcs = _scan.load_functions(root, skip_tests=not include_tests)
    fps: list[frozenset[int]] = []
    kept: list[_scan.Function] = []
    tok_lens: list[int] = []  # real structural-token counts (NOT k-gram set sizes)
    for fn in funcs:
        toks = _scan.tokenize_structural(fn.body)
        if len(toks) >= min_tokens:
            kept.append(fn)
            fps.append(kgrams(toks, k))
            tok_lens.append(len(toks))

    postings: dict[int, list[int]] = defaultdict(list)
    for idx, fp in enumerate(fps):
        for g in fp:
            postings[g].append(idx)
    shared: dict[tuple[int, int], int] = Counter()
    for members in postings.values():
        if len(members) > df_max:
            continue
        for a in range(len(members)):
            ia = members[a]
            for b in range(a + 1, len(members)):
                shared[(ia, members[b])] += 1

    findings: list[_report.Finding] = []
    for (i, j), count in shared.items():
        if count < min_shared:
            continue
        fi, fj = kept[i], kept[j]
        # Skip only a function compared with itself (same file AND same line). Two
        # *different* methods that share a name in one file - e.g. `from_json_str` in
        # separate impl blocks - are legitimate clone candidates, so keying the
        # self-check on name+path (which such siblings share) wrongly dropped them.
        if fi.path == fj.path and fi.line == fj.line:
            continue
        inter = len(fps[i] & fps[j])
        if not inter:
            continue
        jac = inter / len(fps[i] | fps[j])
        contain = inter / min(len(fps[i]), len(fps[j]))
        score = max(jac, contain) if containment else jac
        if score < min_jaccard:
            continue
        cross = fi.path != fj.path
        findings.append(_report.Finding(
            tool="clones", kind="structural-clone",
            summary=f"{fi.name} <=> {fj.name} (jaccard {jac:.2f}{', cross-file' if cross else ''})",
            score=round(score, 4), severity=_severity(score, cross),
            locations=[_report.Loc(fi.path, fi.line, fi.name), _report.Loc(fj.path, fj.line, fj.name)],
            metrics={"jaccard": round(jac, 4), "containment": round(contain, 4), "cross_file": cross,
                     "min_tokens": min(tok_lens[i], tok_lens[j])},
        ))
    findings.sort(key=lambda f: (f.metrics["cross_file"], f.score, f.metrics["min_tokens"]), reverse=True)
    return findings


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates", help="directory to scan (default: crates)")
    ap.add_argument("--min-tokens", type=int, default=45, help="ignore functions shorter than this (default: 45)")
    ap.add_argument("-k", "--kgram", type=int, default=8, help="k-gram length (default: 8)")
    ap.add_argument("--df-max", type=int, default=40, help="drop k-grams shared by more than N functions (default: 40)")
    ap.add_argument("--min-shared", type=int, default=4, help="minimum shared rare k-grams to consider a pair (default: 4)")
    ap.add_argument("--min-jaccard", type=float, default=0.6, help="report threshold on the score (default: 0.6)")
    ap.add_argument("--containment", action="store_true", help="also score by containment (noisier: small helpers match big functions)")
    ap.add_argument("--include-tests", action="store_true", help="include tests/ trees (default: excluded)")
    ap.add_argument("--limit", type=int, default=80, help="max pairs to print (default: 80)")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.min_tokens, args.kgram, args.df_max, args.min_shared,
                       args.min_jaccard, args.containment, args.include_tests)
    if args.json:
        _report.print_json(findings)
        return 0

    cross = sum(1 for f in findings if f.metrics["cross_file"])
    for f in findings[: args.limit]:
        a, b = f.locations
        tag = "XFILE" if f.metrics["cross_file"] else "     "
        print(f"{f.score:.2f} (j={f.metrics['jaccard']:.2f} c={f.metrics['containment']:.2f}) {tag}  "
              f"{a.name} ({a.file}:{a.line})  <=>  {b.name} ({b.file}:{b.line})")
    print(f"# {len(findings)} pairs >= {args.min_jaccard} ({cross} cross-file, {len(findings) - cross} intra-file); "
          f"showing {min(len(findings), args.limit)}. Cross-file (XFILE) pairs are usually higher-value targets.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
