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


def extract_sql(src: str):
    out = []
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

    uniq = list({n: (p, line, n) for p, line, n in items}.values())
    uniq.sort(key=lambda x: len(x[2]))
    sm = SequenceMatcher(None, autojunk=False)
    for i in range(len(uniq)):
        pi, li, ni = uniq[i]
        sm.set_seq2(ni)
        for j in range(i + 1, len(uniq)):
            pj, lj, nj = uniq[j]
            if len(nj) > len(ni) * 1.3:
                break
            if pi == pj:
                continue
            sm.set_seq1(nj)
            if sm.real_quick_ratio() < near or sm.quick_ratio() < near:
                continue
            r = sm.ratio()
            if near <= r < 0.999:
                both_prod = not is_test(pi) and not is_test(pj)
                findings.append(_report.Finding(
                    tool="sqldup", kind="sql-near-dup",
                    summary=f"{r:.2f} similar SQL across files ({'prod' if both_prod else 'test'})",
                    score=round(r, 4), severity="medium" if both_prod else "low",
                    locations=[_report.Loc(pi, li), _report.Loc(pj, lj)],
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
