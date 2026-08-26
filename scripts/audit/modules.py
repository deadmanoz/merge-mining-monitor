#!/usr/bin/env python3
"""Intra-crate module dependency cycles (a lightweight cargo-modules).

Cyclic module dependencies are a "modular mirage" tell: file-level modularity
with no real layering, where concepts reach back and forth across a crate. This
builds a top-level module graph per crate from `crate::<module>`/`super::`
references and reports strongly-connected components (cycles).

Top-level granularity: nodes are the modules the crate root declares - each
`mod foo;` (backed by `foo.rs`/`foo/`) AND each top-level inline `mod foo { .. }`
(defined in the root file). The graph is built by walking the `mod` declaration
tree from `lib.rs`/`main.rs`, reading only declaration-reachable files, so an
undeclared/obsolete `.rs` on disk contributes no phantom node or edge. With
`--include-tests`, each crate's companion integration-test module trees
(`<crate>/tests/<sub>/`) are analyzed as their own roots (a permissive
filesystem walk, since those have no `lib.rs`/`main.rs`). For AST-accurate,
all-levels analysis use `cargo-modules dependencies --acyclic`. Advisory;
stdlib-only. --json supported.
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
# A top-level module declaration in a crate-root file, capturing the terminator so a
# FILE module (`mod foo;`) is told apart from an INLINE one (`mod foo { ... }`). Rust
# compiles `src/foo.rs`/`src/foo/` only for the `;` form; the `{` form defines the
# module in-place, so a same-named stray `foo.rs` is dead and must not become a node.
MOD_DECL = re.compile(r"\bmod\s+(?:r#)?([a-z_][a-z0-9_]*)\s*([;{])")


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


def _declared_top_mods(src: str) -> set[str] | None:
    """Top-level module names declared by the crate root file(s) in `src`, or None.

    Rust does not compile a `src/foo.rs` merely because it exists - it must be
    declared `mod foo;` from the crate root. So an obsolete/undeclared `unused.rs`
    (or a pair of them referencing each other) is not part of the module graph and
    must not become a node/cycle. Reads `lib.rs`/`main.rs` (a crate can carry both)
    and returns the union of their `mod` declarations. Returns None when neither root
    is present (e.g. a `tests/<sub>/` root whose parent binary assembles modules
    unconventionally, often via `#[path]`): the caller then stays permissive rather
    than dropping every node."""
    roots = [f for f in ("lib.rs", "main.rs") if os.path.isfile(os.path.join(src, f))]
    if not roots:
        return None
    declared: set[str] = set()
    for rf in roots:
        try:
            # De-noise first: a `mod other` inside a string/comment is not a real
            # declaration and must not admit a phantom node.
            text = _scan.strip_noise(open(os.path.join(src, rf), encoding="utf-8", errors="ignore").read())
        except OSError:
            continue
        # Only a FILE module (`mod foo;`, group 2 == ";") declared at the CRATE ROOT
        # authorizes a `foo.rs`/`foo/` node. Two things are required:
        #  * `;` form - an inline `mod foo { ... }` has its own body, so a same-named
        #    stray `foo.rs` on disk is uncompiled and must not become that node;
        #  * brace depth 0 - a `mod ghost;` nested inside an inline `mod outer { ... }`
        #    declares `outer::ghost`, not a top-level module, so an obsolete root-level
        #    `ghost.rs` must not be bound to it (that invented a false cycle).
        # `text` is strip_noise'd, so every `{`/`}` counted here is a real code brace.
        for m in MOD_DECL.finditer(text):
            if m.group(2) != ";":
                continue
            if text.count("{", 0, m.start()) != text.count("}", 0, m.start()):
                continue  # inside an inline module block, not the crate root
            declared.add(m.group(1))
    return declared


def top_modules(src: str) -> dict[str, str]:
    """Map top-level module name -> representative file (repo-relative).

    Restricted to modules the crate root actually declares (`_declared_top_mods`), so
    an undeclared `.rs` file under `src/` is not turned into a graph node. When the
    root file is absent (a `tests/<sub>/` root), every entry is kept as before."""
    declared = _declared_top_mods(src)
    mods: dict[str, str] = {}
    for entry in sorted(os.listdir(src)):
        p = os.path.join(src, entry)
        if os.path.isdir(p):
            if entry == BIN_DIR:
                continue  # `src/bin/` holds independent binary roots, not a module
            if declared is not None and entry not in declared:
                continue  # a directory not declared `mod <entry>;` is not compiled in
            rep = os.path.join(p, "mod.rs")
            mods[entry] = _scan.rel(rep if os.path.exists(rep) else p)
        elif entry.endswith(".rs") and entry[:-3] not in ENTRY:
            if declared is not None and entry[:-3] not in declared:
                continue  # an undeclared source file is not part of the module graph
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


def _resolve_mod_file(submod_dir: str, name: str) -> str | None:
    """On-disk file backing `mod name;` for a module whose submodules live in
    `submod_dir`: Rust looks for `<submod_dir>/<name>.rs`, then
    `<submod_dir>/<name>/mod.rs`. (`#[path=...]` overrides are not resolved.)"""
    cand = os.path.join(submod_dir, name + ".rs")
    if os.path.isfile(cand):
        return cand
    cand = os.path.join(submod_dir, name, "mod.rs")
    if os.path.isfile(cand):
        return cand
    return None


def _top_mod_decls(text: str) -> list[tuple[str, str, int, int]]:
    """`(name, kind, decl_start, brace_idx)` for each `mod name;`/`mod name {` at
    brace depth 0 of `text` (this unit's OWN body, not a nested block). `kind` is
    ';' or '{'; `brace_idx` indexes the opening `{` (meaningful for '{'). `text`
    must be strip_noise'd so every counted brace is real code."""
    out: list[tuple[str, str, int, int]] = []
    for m in MOD_DECL.finditer(text):
        if text.count("{", 0, m.start()) != text.count("}", 0, m.start()):
            continue  # nested inside some block, not a declaration of this unit
        out.append((m.group(1), m.group(2), m.start(), m.end() - 1))
    return out


# A `#[cfg(test)]` attribute (or `cfg(all(test, ..))`) directly preceding a decl.
# `test` must be a bare cfg option, so `feature = "test-utils"` does not match.
_CFG_TEST_ATTR = re.compile(
    r"#\[\s*cfg\s*\([^\]]*(?<![\w-])test(?![\w-])[^\]]*\)\s*\]\s*$")


def _cfg_test_before(text: str, pos: int) -> bool:
    return bool(_CFG_TEST_ATTR.search(text[max(0, pos - 240):pos]))


def _walk_module(text: str, submod_dir: str, depth: int, owner: str,
                 fragments: dict[str, list[tuple[str, int]]], seen: set[str]) -> None:
    """Attribute `text` (a file's contents or an inline module body) to top-level
    `owner`, then descend its declaration-reachable children. Each fragment carries
    its module `depth` (crate-root-relative) so `super::` hops resolve correctly.
    Only files named by a `mod name;` are read, so an undeclared sibling on disk is
    never scanned; inline `mod name { .. }` bodies are walked in place."""
    fragments.setdefault(owner, []).append((text, depth))
    for name, kind, _start, brace in _top_mod_decls(text):
        child_dir = os.path.join(submod_dir, name)
        if kind == "{":
            body, _ = _brace_group(text, brace)
            _walk_module(body, child_dir, depth + 1, owner, fragments, seen)
        else:
            f = _resolve_mod_file(submod_dir, name)
            if not f or f in seen:
                continue
            seen.add(f)
            try:
                ft = _scan.strip_noise(open(f, encoding="utf-8", errors="ignore").read())
            except OSError:
                continue
            _walk_module(ft, child_dir, depth + 1, owner, fragments, seen)


def _declared_graph(src: str, roots: list[str], include_tests: bool
                    ) -> tuple[dict[str, str], dict[str, list[tuple[str, int]]]]:
    """Build `(nodes, fragments)` for a crate by walking its `mod` declaration tree.

    `nodes` maps each top-level module name to a representative file (the backing
    file for `mod foo;`, or the crate root for a top-level inline `mod foo {..}`).
    `fragments` maps each name to the `(text, depth)` chunks of its whole subtree.
    Test modules are skipped unless `include_tests` (a `#[cfg(test)]` inline module,
    or a `mod foo;` resolving to a `_test.rs`/`tests.rs` file)."""
    nodes: dict[str, str] = {}
    fragments: dict[str, list[tuple[str, int]]] = {}
    seen: set[str] = {os.path.join(src, r) for r in roots}
    for rf in roots:
        root_path = os.path.join(src, rf)
        try:
            root_text = _scan.strip_noise(open(root_path, encoding="utf-8", errors="ignore").read())
        except OSError:
            continue
        for name, kind, start, brace in _top_mod_decls(root_text):
            if not include_tests and _cfg_test_before(root_text, start):
                continue
            child_dir = os.path.join(src, name)
            if kind == "{":
                nodes.setdefault(name, _scan.rel(root_path))
                body, _ = _brace_group(root_text, brace)
                _walk_module(body, child_dir, 1, name, fragments, seen)
            else:
                f = _resolve_mod_file(src, name)
                if not f or (not include_tests and _scan._is_test_file(f)):
                    continue
                nodes.setdefault(name, _scan.rel(f))
                if f in seen:
                    continue
                seen.add(f)
                try:
                    ft = _scan.strip_noise(open(f, encoding="utf-8", errors="ignore").read())
                except OSError:
                    continue
                _walk_module(ft, child_dir, 1, name, fragments, seen)
    return nodes, fragments


def _edges_from_fragments(nodes: dict[str, str],
                          fragments: dict[str, list[tuple[str, int]]]) -> dict[str, set[str]]:
    adj: dict[str, set[str]] = {m: set() for m in nodes}
    for owner, frags in fragments.items():
        for text, depth in frags:
            for ref in crate_refs(text):
                if ref in nodes and ref != owner:
                    adj[owner].add(ref)
            # `super::` hops reaching exactly the crate root (== fragment depth) name
            # a top-level module; shallower hops resolve inside `owner`.
            for target in super_refs(text, depth):
                if target in nodes and target != owner:
                    adj[owner].add(target)
    return adj


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


def _fs_graph(src: str, include_tests: bool
              ) -> tuple[dict[str, str], dict[str, set[str]]]:
    """Permissive filesystem-based graph for a root WITHOUT a `lib.rs`/`main.rs`
    (a `tests/<sub>/` integration tree): every top-level file/dir is a node and
    every `.rs` beneath it is scanned, since the assembling binary root is external
    and often wires modules via `#[path]` we cannot follow."""
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
        depth = len(_module_path_parts(rel_parts))
        for target in super_refs(text, depth):
            if target in mods and target != owner:
                adj[owner].add(target)
    return mods, adj


def collect(root: str = "crates", include_tests: bool = False) -> list[_report.Finding]:
    findings: list[_report.Finding] = []
    for crate, src in crate_src_dirs(root, include_tests):
        roots = [f for f in ("lib.rs", "main.rs") if os.path.isfile(os.path.join(src, f))]
        if roots:
            # Declaration-tree walk from the crate root: nodes are the declared
            # top-level modules (file AND inline), edges come only from
            # declaration-reachable source, so obsolete undeclared files add nothing.
            nodes, fragments = _declared_graph(src, roots, include_tests)
            adj = _edges_from_fragments(nodes, fragments)
        else:
            nodes, adj = _fs_graph(src, include_tests)
        for comp in _sccs(list(nodes), adj):
            if len(comp) < 2:
                continue
            order = sorted(comp)
            findings.append(_report.Finding(
                tool="modules", kind="module-cycle",
                summary=f"{crate}: {len(order)}-module cycle [{' <-> '.join(order)}]",
                score=float(len(order)),
                severity="high" if len(order) >= 4 else "medium",
                locations=[_report.Loc(nodes[m], 0, m) for m in order],
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
