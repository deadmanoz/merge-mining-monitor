#!/usr/bin/env python3
"""Intra-crate module dependency cycles (a lightweight cargo-modules).

Cyclic module dependencies are a "modular mirage" tell: file-level modularity
with no real layering, where concepts reach back and forth across a crate. This
builds a top-level module graph per crate from `crate::<module>` references and
reports strongly-connected components (cycles).

Top-level granularity: nodes are the immediate children of each `src/` (a
`foo.rs` or a `foo/` directory). With `--include-tests`, each crate's companion
integration-test module trees (`<crate>/tests/<sub>/`) are analyzed as their own
roots too. For AST-accurate, all-levels analysis use `cargo-modules dependencies
--acyclic`. Advisory; stdlib-only. --json supported.
"""

from __future__ import annotations

import argparse
import os
import re

import _report
import _scan

CRATE_PREFIX = re.compile(r"\bcrate::")
# A module identifier, accepting an optional `r#` raw-identifier prefix so a
# keyword-named module referenced as `crate::r#type` resolves to the logical name
# `type` (the on-disk node is `type.rs`, not `r#type.rs`). group(1) is that name;
# without the prefix the bare regex captured only `r` and the edge/cycle was lost.
_IDENT = re.compile(r"(?:r#)?([a-z_][a-z0-9_]*)")
# A run of one or more `super::` hops, e.g. `super::` or `super::super::`. The
# captured run's length says how many parents to ascend; what follows (a single
# identifier or a `{...}` group) names the referenced module(s).
SUPER_PREFIX = re.compile(r"\b((?:super::)+)")
ENTRY = {"lib", "main", "mod"}
# Cargo compiles each file under `src/bin/` as an independent binary crate root,
# so the directory is not a library module and its files must not be pooled into
# one synthetic `bin` node (which would invent cross-binary edges/cycles).
BIN_DIR = "bin"


def _brace_group(text: str, open_idx: int) -> tuple[str, int]:
    """Content between the `{` at `open_idx` and its matching `}`, plus the index
    just past the close (or end of string if unbalanced)."""
    depth = 0
    for k in range(open_idx, len(text)):
        if text[k] == "{":
            depth += 1
        elif text[k] == "}":
            depth -= 1
            if depth == 0:
                return text[open_idx + 1 : k], k + 1
    return text[open_idx + 1 :], len(text)


def _split_top_commas(s: str) -> list[str]:
    """Split on commas at brace depth 0 (so a nested group `a::{b, c}` stays one
    item)."""
    items: list[str] = []
    depth = start = 0
    for i, c in enumerate(s):
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
        elif c == "," and depth == 0:
            items.append(s[start:i])
            start = i + 1
    items.append(s[start:])
    return items


def crate_refs(text: str) -> set[str]:
    """Top-level modules referenced via `crate::`, covering both the single-path
    form (`crate::foo::bar` -> `foo`) and grouped imports
    (`use crate::{cli_args, lock_block_hash};` -> `cli_args`, `lock_block_hash`;
    nested `crate::{a::{b}, c}` -> `a`, `c`). Missing the grouped form silently
    dropped real edges and could hide a whole cycle."""
    out: set[str] = set()
    for m in CRATE_PREFIX.finditer(text):
        i = m.end()
        while i < len(text) and text[i].isspace():
            i += 1
        if i < len(text) and text[i] == "{":
            group, _ = _brace_group(text, i)
            for item in _split_top_commas(group):
                im = _IDENT.match(item.strip())
                if im:
                    out.add(im.group(1))
        else:
            im = _IDENT.match(text, i)
            if im:
                out.add(im.group(1))
    return out


def super_refs(text: str, depth: int) -> set[str]:
    """Top-level modules referenced through a run of `super::` hops that ascends
    exactly to the crate root (hop count == the referrer's module depth).

    Covers the single-path form (`super::other`) and grouped imports
    (`use super::{a, b};`, `super::super::{x}`, nested `super::{a::{b}, c}`),
    mirroring `crate_refs`. Without the grouped form a sibling cycle written
    `use super::{other};` is invisible and the crate looks acyclic."""
    out: set[str] = set()
    for m in SUPER_PREFIX.finditer(text):
        if m.group(1).count("super") != depth:
            continue  # ascends to a sub-module, not a crate-level module
        i = m.end()
        while i < len(text) and text[i].isspace():
            i += 1
        if i < len(text) and text[i] == "{":
            group, _ = _brace_group(text, i)
            for item in _split_top_commas(group):
                im = _IDENT.match(item.strip())
                if im:
                    out.add(im.group(1))
        else:
            im = _IDENT.match(text, i)
            if im:
                out.add(im.group(1))
    return out


def _has_rust(dirpath: str) -> bool:
    for _dp, _dn, fns in os.walk(dirpath):
        if any(f.endswith(".rs") for f in fns):
            return True
    return False


def crate_src_dirs(root: str, include_tests: bool = False) -> list[tuple[str, str]]:
    """Analysis `(name, dir)` roots under `root`.

    Each crate contributes its `src/`. With `include_tests`, each crate also
    contributes every companion module tree under `<crate>/tests/<sub>/`: a
    conventional integration-test binary rooted at `tests/<sub>.rs` assembles its
    submodules from `tests/<sub>/` (often via `#[path]`), and cross-references there
    resolve through the binary root, so a cycle among those modules is real. Without
    this the flag only stopped pruning a `src/tests` dir and never reached the 48+
    files under crate-level `tests/`. Flat single-file test binaries (`tests/foo.rs`
    with no `tests/foo/`) have no internal module graph and contribute nothing.

    Handles both a crates container passed directly (`crates/<crate>/src`) and a
    repository root (`.`) whose crates live under a nested `crates/` dir - a whole
    -repo report would otherwise analyze zero crates while every other detector
    scans the tree.
    """
    out: list[tuple[str, str]] = []
    seen: set[str] = set()

    def add_members(container: str) -> None:
        if not os.path.isdir(container):
            return
        for name in sorted(os.listdir(container)):
            crate_dir = os.path.join(container, name)
            src = os.path.join(crate_dir, "src")
            if os.path.isdir(src) and src not in seen:
                seen.add(src)
                out.append((name, src))
                if include_tests:
                    tests = os.path.join(crate_dir, "tests")
                    if os.path.isdir(tests):
                        for sub in sorted(os.listdir(tests)):
                            d = os.path.join(tests, sub)
                            if os.path.isdir(d) and d not in seen and _has_rust(d):
                                seen.add(d)
                                out.append((f"{name}/tests/{sub}", d))

    if os.path.isdir(root):
        add_members(root)                          # <root>/<crate>/src
        add_members(os.path.join(root, "crates"))  # <root>/crates/<crate>/src (workspace layout)
    if not out and os.path.isdir(os.path.join(root, "src")):
        out.append((os.path.basename(os.path.abspath(root)), os.path.join(root, "src")))
    return out


def top_modules(src: str) -> dict[str, str]:
    """Map top-level module name -> representative file (repo-relative)."""
    mods: dict[str, str] = {}
    for entry in sorted(os.listdir(src)):
        p = os.path.join(src, entry)
        if os.path.isdir(p):
            if entry == BIN_DIR:
                continue  # `src/bin/` holds independent binary roots, not a module
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


def _module_path_parts(rel_parts: list[str]) -> list[str]:
    """Full module path (crate-root-relative) for a source file, as segments.

    `foo/mod.rs` -> `[foo]`; `foo/bar.rs` -> `[foo, bar]`; `foo.rs` -> `[foo]`;
    `lib.rs`/`main.rs` -> `[]` (the crate root). The length is the module's depth,
    which tells `super::` resolution how many parents a hop-run ascends.
    """
    parts = list(rel_parts)
    last = parts[-1]
    if last.endswith(".rs"):
        stem = last[:-3]
        parts = parts[:-1] if stem in ENTRY else parts[:-1] + [stem]
    return parts


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
        for w in sorted(adj.get(v, ())):  # sorted: component emission independent of set/hash order
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
    for crate, src in crate_src_dirs(root, include_tests):
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
            for ref in crate_refs(text):
                if ref in mods and ref != owner:
                    adj[owner].add(ref)
            # Relative `super::` hops (single or grouped). From a module at depth d, k
            # `super`s ascend to mp[:d-k]; the reference is a crate-level (top-level)
            # module only when the hops reach exactly the crate root (k == d) -
            # otherwise it still resolves inside `owner`. Without this, a sibling cycle
            # expressed as `super::other` (paired with the other side's `crate::owner`)
            # is invisible and the whole crate looks acyclic.
            depth = len(_module_path_parts(rel_parts))
            for target in super_refs(text, depth):
                if target in mods and target != owner:
                    adj[owner].add(target)
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
    # Deterministic order: larger cycles first, then by summary (the sorted module
    # list) so tied findings never reorder across runs/hash seeds - the JSON contract
    # is byte-stable.
    findings.sort(key=lambda f: (-f.score, f.summary))
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
