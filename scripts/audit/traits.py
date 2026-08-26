#!/usr/bin/env python3
"""Facade-abstraction detector: trait surface vs shared body and impl count.

Sprawl smell: a facade abstraction is a trait that looks like a seam but hides
nothing - each impl restates the full surface and the trait carries little or no
shared (default-method) behavior. This flags locally-defined traits with a large
*required* surface, several impls, and few *provided* lines behind them: the
shape where an interface documents a contract but prevents no duplication.

A candidate list, not a verdict: a genuinely polymorphic trait with dynamic
dispatch looks the same to a regex. Apply the deletion test (prompts/) before
collapsing one. Advisory; stdlib-only. --json supported.
"""

from __future__ import annotations

import argparse
import re

import _report
import _scan

FN = re.compile(r"\bfn\s+(?:r#)?([A-Za-z0-9_]+)")  # `(?:r#)?` -> `fn r#match` reads as `match`
# A module declaration (`mod api;`, `pub mod api { ... }`, `pub(crate) mod api;`). The
# set of these names per crate tells the impl scan which qualified roots
# (`impl api::Trait for X`) resolve to a LOCAL module rather than an external crate.
MOD_DECL = re.compile(r"\bmod\s+(?:r#)?([a-z_][a-z0-9_]*)")
# A `use ...;` statement, whose body is searched for module aliases below.
USE_STMT = re.compile(r"\buse\s+([^;]+);")
# An `item as alias` pair inside a `use` body, covering both the plain form
# (`use crate::api as contract`) and the grouped form (`use crate::{api as contract}`).
# `item` is the last path segment renamed; when it names a local module the alias is a
# local-module root too, so `impl contract::Service for X` must not be dropped as
# external. Scoped to `use` bodies so a value/type cast (`x as u64`) is never read as
# a module alias.
USE_AS_PAIR = re.compile(r"(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s+as\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)")
# The FIRST path segment of a `use` tree - its root. Only an intra-crate root
# (`crate`/`self`/`super`) can rename a LOCAL module; an external-crate path
# (`use other::api as contract`) whose last segment merely collides with a local
# `mod` name must not fold its alias into local_mods (that would count an external
# `impl contract::Trait` as local). A leading `::` (external crate) or any other
# identifier root fails this and is skipped.
USE_ROOT = re.compile(r"\s*(?:r#)?([A-Za-z_][A-Za-z0-9_]*)")
INTRA_CRATE_ROOTS = ("crate", "self", "super")


def _match_brace(src: str, open_idx: int) -> int:
    """Index of the `}` matching the `{` at `open_idx`."""
    depth = 0
    for k in range(open_idx, len(src)):
        if src[k] == "{":
            depth += 1
        elif src[k] == "}":
            depth -= 1
            if depth == 0:
                return k
    return len(src) - 1


def _count_methods(body: str) -> tuple[int, int]:
    """(required, provided) fn count in a trait body, skipping nested bodies.

    Signature classification is delegated to `_scan.find_signature_end` so a
    const-generic argument in a required method's return type
    (`fn digest() -> Foo<{ 1 + 1 }>;`) is not mistaken for a default-method body -
    which would wrongly count the method as provided and mask a facade candidate.
    """
    req = prov = 0
    i = 0
    while True:
        m = FN.search(body, i)
        if not m:
            break
        kind, pos = _scan.find_signature_end(body, m.end())
        if kind == "decl":  # ends in `;` -> required method
            req += 1
            i = pos + 1
        elif kind == "body":  # ends in `{` -> default (provided) method
            prov += 1
            i = _match_brace(body, pos) + 1
        else:
            break
    return req, prov


def _strip_leading_generics(s: str) -> str:
    s = s.lstrip()
    if not s.startswith("<"):
        return s
    depth = 0
    for i, c in enumerate(s):
        if c == "<":
            depth += 1
        elif c == ">":
            depth -= 1
            if depth == 0:
                return s[i + 1:]
    return s


def _crate_key(rel: str) -> str:
    """Crate root for a `_scan.rel` path, so same-named traits in different crates
    do not collide (impls of an unrelated trait sharing a bare name stay separate)."""
    parts = rel.split("/")
    if "src" in parts:
        return "/".join(parts[: parts.index("src")])
    return parts[0] if parts else ""


def collect(root: str, min_required: int = 3, min_impls: int = 2, include_tests: bool = False) -> list[_report.Finding]:
    # Key traits and impls by (crate, bare name). Bare names are not globally
    # unique; without the crate qualifier a later trait would overwrite an earlier
    # one and impls of unrelated same-named traits would pool together.
    traits: dict[tuple[str, str], dict] = {}
    ambiguous: set[tuple[str, str]] = set()  # same (crate, name) defined more than once
    impls: dict[tuple[str, str], list[_report.Loc]] = {}
    # Local module names per crate, so a trait impl qualified through a local module
    # path (`impl api::Service for X` with `mod api`) is recognized as local rather
    # than dropped as external. Impl attribution is deferred until every file's `mod`
    # declarations across the crate have been gathered.
    local_mods: dict[str, set[str]] = {}
    # `(crate, item, alias)` for every `use <path>::item as alias` in the crate. After
    # all `mod` declarations are known, an alias whose `item` is a local module is
    # folded into `local_mods`, so an impl rooted at the alias (`use crate::api as
    # contract; impl contract::Service for X`) resolves as local, not external.
    pending_aliases: list[tuple[str, str, str]] = []
    pending_impls: list[tuple[str, str, str | None, _report.Loc]] = []

    for path in _scan.iter_rust_files(root, skip_tests=not include_tests):
        try:
            src = _scan.strip_noise(open(path, encoding="utf-8", errors="ignore").read())
        except OSError:
            continue
        rel = _scan.rel(path)
        crate = _crate_key(rel)
        for mm in MOD_DECL.finditer(src):
            local_mods.setdefault(crate, set()).add(mm.group(1))
        for um in USE_STMT.finditer(src):
            body = um.group(1)
            # Gate on the use-tree root: only crate/self/super paths rename a local
            # module, so an external `use other::api as contract` never fabricates a
            # local-module alias even when `api` collides with a real `mod api`.
            rm = USE_ROOT.match(body)
            if not rm or rm.group(1) not in INTRA_CRATE_ROOTS:
                continue
            for am in USE_AS_PAIR.finditer(body):
                pending_aliases.append((crate, am.group(1), am.group(2)))
        # `(?:r#)?` consumes a raw-identifier prefix so a keyword-named `trait r#type`
        # is keyed as `type` - the same logical name the impl scan derives from
        # `impl r#type for ...` (its `idents[-1]` is `type`). Without it the trait was
        # keyed `r`, the definition and impls never joined, and a real facade was hidden.
        for m in re.finditer(r"\btrait\s+(?:r#)?([A-Za-z0-9_]+)", src):
            bo = src.find("{", m.end())
            if bo < 0:
                continue
            req, prov = _count_methods(src[bo : _match_brace(src, bo) + 1])
            key = (crate, m.group(1))
            if key in traits:
                ambiguous.add(key)
            traits[key] = {
                "required": req, "provided": prov,
                "loc": _report.Loc(rel, src[: m.start()].count("\n") + 1, m.group(1)),
            }
        for m in re.finditer(r"\bimpl\b", src):
            bo = src.find("{", m.end())
            if bo < 0:
                continue
            header = src[m.end() : bo]
            fm = re.search(r"\bfor\b(?!\s*<)", header)  # skip HRTB `for<'a>`
            if not fm:
                continue  # inherent impl, not a trait impl
            head = _strip_leading_generics(header[: fm.start()]).split("<")[0]
            idents = re.findall(r"[A-Za-z_][A-Za-z0-9_]*", head)
            if not idents:
                continue
            # Record the qualified-path root (if any) for later locality resolution.
            # Bare names (`impl Display for X`) and crate/self/super-rooted paths are
            # always local (root None). A leading `r` is the raw-identifier prefix of a
            # raw module (`r#foo::Bar`), so the real root is the next segment.
            ext_root: str | None = None
            if "::" in head:
                root_seg = idents[1] if idents[0] == "r" and len(idents) > 1 else idents[0]
                if root_seg not in ("crate", "self", "super"):
                    ext_root = root_seg  # local vs external decided once mods are known
            pending_impls.append(
                (crate, idents[-1], ext_root, _report.Loc(rel, src[: m.start()].count("\n") + 1))
            )

    # Fold module aliases into the crate's local-module set: `use crate::api as
    # contract` makes `contract` a local root whenever `api` is a local `mod`. Done
    # after the file scan so an alias resolves regardless of `mod`/`use` ordering.
    for crate, item, alias in pending_aliases:
        if item in local_mods.get(crate, ()):
            local_mods.setdefault(crate, set()).add(alias)

    # Resolve deferred impls now that every crate's local modules (and their aliases)
    # are known. A qualified path rooted at a local module or its alias (`impl
    # api::Service` / `impl contract::Service`, with `mod api`) is a local impl and
    # counts; only a root naming no local module (std/core/another crate) is external
    # and dropped, so an external `std::fmt::Display` no longer inflates a same-named
    # local trait while a local `api::Service` still does.
    for crate, name, ext_root, loc in pending_impls:
        if ext_root is not None and ext_root not in local_mods.get(crate, ()):
            continue
        impls.setdefault((crate, name), []).append(loc)

    findings: list[_report.Finding] = []
    for key, t in traits.items():
        if key in ambiguous:
            continue  # cannot attribute impls unambiguously; skip rather than guess
        name = key[1]
        impl_locs = impls.get(key, [])
        if t["required"] < min_required or len(impl_locs) < min_impls:
            continue
        req, prov, nimpl = t["required"], t["provided"], len(impl_locs)
        if req >= 5 and nimpl >= 3 and prov == 0:
            sev = "high"
        elif prov == 0:
            sev = "medium"
        else:
            sev = "low"
        findings.append(_report.Finding(
            tool="traits", kind="facade-candidate",
            summary=f"trait {name}: {req} required + {prov} default methods across {nimpl} impls "
                    f"(surface restated ~{req * nimpl}x, {prov} shared)",
            score=float(req * nimpl), severity=sev,
            locations=[t["loc"], *impl_locs],
            metrics={"required": req, "provided": prov, "impls": nimpl},
        ))
    findings.sort(key=lambda f: (f.score, -f.metrics["provided"]), reverse=True)
    return findings


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--min-required", type=int, default=3, help="minimum required methods to consider (default: 3)")
    ap.add_argument("--min-impls", type=int, default=2, help="minimum impls to consider (default: 2)")
    ap.add_argument("--include-tests", action="store_true")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.min_required, args.min_impls, args.include_tests)
    if args.json:
        _report.print_json(findings)
        return 0
    print("  surface  req/def  impls  trait (file:line)")
    for f in findings:
        loc = f.locations[0]
        print(f"{int(f.score):8d}  {f.metrics['required']:2d}/{f.metrics['provided']:<3d}  "
              f"{f.metrics['impls']:5d}  {loc.name} ({loc.file}:{loc.line})")
    print(f"# {len(findings)} facade candidates (>= {args.min_required} required methods, >= {args.min_impls} impls)")
    print("# High = large surface, many impls, zero shared default behavior. Verify with the deletion test.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
