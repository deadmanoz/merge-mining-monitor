#!/usr/bin/env python3
"""Intra-crate module dependency cycles (a lightweight cargo-modules).

Cyclic module dependencies are a "modular mirage" tell: file-level modularity
with no real layering, where concepts reach back and forth across a crate. This
builds a top-level module graph per crate from `crate::<module>` references and
reports strongly-connected components (cycles).

Top-level granularity: nodes are the immediate children of each `src/` (a
`foo.rs` or a `foo/` directory). For AST-accurate, all-levels analysis use
`cargo-modules dependencies --acyclic`. Advisory; stdlib-only. --json supported.
"""

from __future__ import annotations

import argparse
import os
import re

import _report
import _scan

CRATE_REF = re.compile(r"\bcrate::([a-z_][a-z0-9_]*)")
ENTRY = {"lib", "main", "mod"}


def crate_src_dirs(root: str) -> list[tuple[str, str]]:
    out: list[tuple[str, str]] = []
    if os.path.isdir(root):
        for name in sorted(os.listdir(root)):
            src = os.path.join(root, name, "src")
            if os.path.isdir(src):
                out.append((name, src))
    if not out and os.path.isdir(os.path.join(root, "src")):
        out.append((os.path.basename(os.path.abspath(root)), os.path.join(root, "src")))
    return out


def top_modules(src: str) -> dict[str, str]:
    """Map top-level module name -> representative file (repo-relative)."""
    mods: dict[str, str] = {}
    for entry in sorted(os.listdir(src)):
        p = os.path.join(src, entry)
        if os.path.isdir(p):
            rep = os.path.join(p, "mod.rs")
            mods[entry] = _scan.rel(rep if os.path.exists(rep) else p)
        elif entry.endswith(".rs") and entry[:-3] not in ENTRY:
            mods[entry[:-3]] = _scan.rel(p)
    return mods


def _module_of(rel_parts: list[str]) -> str | None:
    first = rel_parts[0]
    if len(rel_parts) == 1:
        stem = first[:-3] if first.endswith(".rs") else first
        return None if stem in ENTRY else stem
    return first


def _sccs(nodes: list[str], adj: dict[str, set[str]]) -> list[list[str]]:
    """Tarjan strongly-connected components."""
    index: dict[str, int] = {}
    low: dict[str, int] = {}
    on_stack: set[str] = set()
    stack: list[str] = []
    out: list[list[str]] = []
    counter = [0]

    def strong(v: str):
        index[v] = low[v] = counter[0]
        counter[0] += 1
        stack.append(v)
        on_stack.add(v)
        for w in adj.get(v, ()):  # noqa: iterate neighbors
            if w not in index:
                strong(w)
                low[v] = min(low[v], low[w])
            elif w in on_stack:
                low[v] = min(low[v], index[w])
        if low[v] == index[v]:
            comp = []
            while True:
                w = stack.pop()
                on_stack.discard(w)
                comp.append(w)
                if w == v:
                    break
            out.append(comp)

    for v in nodes:
        if v not in index:
            strong(v)
    return out


def collect(root: str = "crates", include_tests: bool = False) -> list[_report.Finding]:
    findings: list[_report.Finding] = []
    for crate, src in crate_src_dirs(root):
        mods = top_modules(src)
        adj: dict[str, set[str]] = {m: set() for m in mods}
        for path in _scan.iter_rust_files(src, skip_tests=not include_tests):
            rel_parts = os.path.relpath(path, src).split(os.sep)
            owner = _module_of(rel_parts)
            if owner is None or owner not in mods:
                continue
            try:
                # Strip comments/strings first: a `crate::other` in a doc link or
                # string literal is prose, not a compiled dependency edge.
                text = _scan.strip_noise(open(path, encoding="utf-8", errors="ignore").read())
            except OSError:
                continue
            for ref in CRATE_REF.findall(text):
                if ref in mods and ref != owner:
                    adj[owner].add(ref)
        for comp in _sccs(list(mods), adj):
            if len(comp) < 2:
                continue
            order = sorted(comp)
            findings.append(_report.Finding(
                tool="modules", kind="module-cycle",
                summary=f"{crate}: {len(order)}-module cycle [{' <-> '.join(order)}]",
                score=float(len(order)),
                severity="high" if len(order) >= 4 else "medium",
                locations=[_report.Loc(mods[m], 0, m) for m in order],
                metrics={"crate": crate, "modules": order},
            ))
    findings.sort(key=lambda f: f.score, reverse=True)
    return findings


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--include-tests", action="store_true")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.include_tests)
    if args.json:
        _report.print_json(findings)
        return 0
    for f in findings:
        print(f"[{f.severity}] {f.summary}")
        for loc in f.locations:
            print(f"        {loc.name}  ({loc.file})")
    print(f"# {len(findings)} intra-crate module cycles")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
