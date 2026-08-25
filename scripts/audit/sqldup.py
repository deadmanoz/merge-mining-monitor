#!/usr/bin/env python3
"""Find duplicated SQL embedded in Rust string literals.

jscpd tokenizes Rust, so a multi-line SQL string is largely one opaque token and
duplication *inside* it is under-counted. Here we pull string literals that look
like SQL, normalize whitespace and `$N` placeholders, and report exact groups
(same normalized SQL in >= 2 places) and near pairs across different files.

Production and test locations are tagged separately: test probe duplication is
common and lower-value than the same query hand-written across production crates.
Advisory; stdlib-only. Emits the shared finding schema with --json.
"""

from __future__ import annotations

import argparse
import re
from collections import defaultdict
from difflib import SequenceMatcher

import _report
import _scan

SQL_KW = re.compile(r"\b(SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|JOIN|WHERE|VALUES|ON\s+CONFLICT|RETURNING|WITH)\b", re.I)
# A PostgreSQL dollar-quote opener: `$$` or `$tag$` where the tag is a valid
# identifier (never starting with a digit, so `$1` positional binds don't match).
DOLLAR_OPEN = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)?\$")


def extract_sql(src: str):
    # Tokenize via the shared scanner: it recognizes raw/byte strings only at a
    # token boundary (so a word ending in `r` before a `"` is never a raw opener),
    # honors matched raw delimiters, and skips comments/char literals - the whole
    # class of quote-desync bugs a hand-rolled regex hits.
    for content, off in _scan.iter_string_literals(src):
        if len(content) >= 40 and len(SQL_KW.findall(content)) >= 2:
            yield src[:off].count("\n") + 1, content


def norm(s: str) -> str:
    """Normalize SQL for comparison, but ONLY outside quoted content.

    Keyword case and structural whitespace are noise (`SELECT` == `select`), so
    they are folded - but a single-quoted string literal (`'ABC'`) or a
    double-quoted identifier (`"Col"`) is case- and whitespace-significant in
    PostgreSQL (`code = 'ABC'` and `code = 'abc'` can return different rows), so its
    bytes are preserved verbatim. SQL escapes a quote by doubling it (`''`), which
    is handled here. (A quote written as a Rust `\\"` escape inside a non-raw string
    literal is a rare edge this heuristic does not unescape.)

    A PostgreSQL dollar-quoted span (`$$...$$` or `$tag$...$tag$`) is likewise
    case- and whitespace-significant (it commonly carries a function body or a
    literal), so it is copied verbatim too. Its tag cannot start with a digit, so a
    positional placeholder like `$1` is never mistaken for a dollar-quote opener.

    Placeholders are canonicalized by first-occurrence order ($3 -> $1) rather than
    all collapsed to one token, so their *equivalence pattern* survives: reusing one
    bind value (`a = $1 OR b = $1`) stays distinct from two independent ones
    (`a = $1 OR b = $2`), which are semantically different queries.
    """
    ph: dict[str, str] = {}

    def canon(m: "re.Match[str]") -> str:
        tok = m.group(0)
        if tok not in ph:
            ph[tok] = f"${len(ph) + 1}"
        return ph[tok]

    out: list[str] = []
    i, n = 0, len(s)
    while i < n:
        c = s[i]
        if c in "'\"":
            q = c
            j = i + 1
            buf = [c]
            while j < n:
                if s[j] == q:
                    if j + 1 < n and s[j + 1] == q:  # doubled-quote escape stays quoted
                        buf.append(q + q)
                        j += 2
                        continue
                    buf.append(q)
                    j += 1
                    break
                buf.append(s[j])
                j += 1
            out.append("".join(buf))  # quoted span: preserved verbatim
            i = j
            continue
        if c == "$":
            mo = DOLLAR_OPEN.match(s, i)
            if mo:
                closer = mo.group(0)  # "$$" or "$tag$"
                end = s.find(closer, mo.end())
                stop = n if end < 0 else end + len(closer)  # unterminated -> rest verbatim
                out.append(s[i:stop])  # dollar-quoted span: preserved verbatim
                i = stop
                continue
        j = i
        while j < n:
            if s[j] in "'\"":
                break
            if s[j] == "$" and DOLLAR_OPEN.match(s, j):
                break
            j += 1
        seg = re.sub(r"\$\d+", canon, s[i:j])
        out.append(re.sub(r"\s+", " ", seg).lower())
        i = j
    return "".join(out).strip()


def is_test(path: str) -> bool:
    return "/tests/" in path or path.endswith(("tests.rs", "_tests.rs", "test_fixtures.rs"))


def _pick_cross(locs_i, locs_j):
    """A representative pair from *different* files (preferring prod/prod), so the
    two reported locations actually demonstrate the cross-file duplication."""
    li_s, lj_s = sorted(locs_i), sorted(locs_j)
    fallback = None
    for a in li_s:
        for b in lj_s:
            if a[0] != b[0]:
                if not is_test(a[0]) and not is_test(b[0]):
                    return a, b
                if fallback is None:
                    fallback = (a, b)
    return fallback if fallback else (li_s[0], lj_s[0])


def collect(root: str, near: float = 0.85) -> list[_report.Finding]:
    items = []  # (path, line, normalized)
    for path in _scan.iter_rust_files(root, skip_tests=False):
        try:
            src = open(path, encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        for line, s in extract_sql(src):
            items.append((_scan.rel(path), line, norm(s)))

    findings: list[_report.Finding] = []

    exact = defaultdict(list)
    for path, line, n in items:
        exact[n].append((path, line))
    for n, locs in sorted(exact.items(), key=lambda x: -len(x[1])):
        if len(locs) < 2:
            continue
        prod = [l for l in locs if not is_test(l[0])]
        prod_files = {p for p, _ in prod}
        sev = "high" if len(prod_files) >= 2 else ("medium" if prod_files else "low")
        findings.append(_report.Finding(
            tool="sqldup", kind="sql-exact-dup",
            summary=f"{len(locs)}x identical SQL ({len(prod_files)} prod file(s)): {n[:90]}",
            score=float(len(locs)), severity=sev,
            locations=[_report.Loc(p, line) for p, line in locs],
            metrics={"count": len(locs), "prod_files": sorted(prod_files), "sql": n[:400]},
        ))

    # Keep every location per normalized query (an exact query in N files keeps
    # all N), so exact duplication can never hide a valid cross-file near-dup and
    # the prod/test classification sees all sites, not an arbitrary last one.
    groups: dict[str, list[tuple[str, int]]] = defaultdict(list)
    for path, line, n in items:
        groups[n].append((path, line))
    norms = sorted(groups, key=len)
    # Longest admissible length ratio for a pair that can still reach `near`: with
    # the shorter string fully matched, ratio = 2*La/(La+Lb), so Lb/La <=
    # (2-near)/near. Deriving it from the threshold (not a fixed 1.3) stops the
    # length prune from discarding pairs that would clear a lower `near`.
    max_len_ratio = (2.0 - near) / near
    sm = SequenceMatcher(None, autojunk=False)
    for i in range(len(norms)):
        ni = norms[i]
        files_i = {p for p, _ in groups[ni]}
        sm.set_seq2(ni)
        for j in range(i + 1, len(norms)):
            nj = norms[j]
            if len(nj) > len(ni) * max_len_ratio:
                break
            files_j = {p for p, _ in groups[nj]}
            if len(files_i | files_j) == 1:
                continue  # both queries confined to the same single file
            sm.set_seq1(nj)
            if sm.real_quick_ratio() < near or sm.quick_ratio() < near:
                continue
            r = sm.ratio()
            if near <= r < 0.999:
                li, lj = _pick_cross(groups[ni], groups[nj])
                both_prod = not is_test(li[0]) and not is_test(lj[0])
                findings.append(_report.Finding(
                    tool="sqldup", kind="sql-near-dup",
                    summary=f"{r:.2f} similar SQL across files ({'prod' if both_prod else 'test'})",
                    score=round(r, 4), severity="medium" if both_prod else "low",
                    locations=[_report.Loc(li[0], li[1]), _report.Loc(lj[0], lj[1])],
                    metrics={"ratio": round(r, 4), "both_prod": both_prod},
                ))
    findings.sort(key=lambda f: f.sort_key(), reverse=True)
    return findings


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--near", type=float, default=0.85, help="near-duplicate similarity floor (default: 0.85)")
    ap.add_argument("--limit", type=int, default=25, help="max near-duplicate pairs (default: 25)")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.near)
    if args.json:
        _report.print_json(findings)
        return 0

    exact = [f for f in findings if f.kind == "sql-exact-dup"]
    near = [f for f in findings if f.kind == "sql-near-dup"]
    print("=== EXACT (whitespace/param-normalized) SQL duplicates ===")
    for f in exact:
        tag = "PROD" if f.metrics["prod_files"] else "test"
        print(f"x{f.metrics['count']:<3d} {tag}  {f.metrics['sql'][:110]}")
        for loc in (f.locations if not f.metrics["prod_files"] else [l for l in f.locations if not is_test(l.file)])[:6]:
            print(f"        {loc.file}:{loc.line}")
    print(f"# {len(exact)} exact-duplicate SQL groups\n")
    print(f"=== NEAR-duplicate SQL (>= {args.near}, different files) ===")
    for f in near[: args.limit]:
        a, b = f.locations
        tag = "PROD" if f.metrics["both_prod"] else "test"
        print(f"{f.score:.2f} {tag}  {a.file}:{a.line}  <=>  {b.file}:{b.line}")
    print(f"# {len(near)} near-duplicate cross-file pairs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
