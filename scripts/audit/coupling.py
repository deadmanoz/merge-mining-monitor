#!/usr/bin/env python3
"""Git behavioral analysis: change churn + temporal coupling.

Static tools see the code as it is now; git history shows how it *evolves*. Two
files that keep changing in the same commits are "temporally coupled" - often a
sign of a concept smeared across modules (a hidden shared responsibility) even
when there is no textual duplication for jscpd or `clones.py` to catch.

Reports CHURN (most-changed files) and COUPLING (co-changing pairs). The coupling
ratio is co_changes / min(co_churn_a, co_churn_b), where co_churn counts only the
commits that actually contribute co-changes (oversized "sweep" commits above
--max-commit-files are excluded from both the numerator and this denominator so a
bulk reformat cannot deflate the ratio). CHURN stays the true total change count.

History is the checked-out revision only (`git log HEAD`) and candidates are
restricted to paths that still exist at HEAD, so the report is deterministic across
clones and never names a deleted file; pass --all-refs for a cross-branch view. An
optional root scopes every metric to a subtree. Run inside the repo. Advisory;
stdlib-only. --json supported.
"""

from __future__ import annotations

import argparse
import os
import subprocess
from collections import Counter
from itertools import combinations

import _report

TRACKED_SUFFIXES = (".rs", ".js")
EXCLUDE = ("generated", "Cargo.lock", "/vendor/", "node_modules/")


def _scope_prefix(root: str | None) -> str | None:
    """Normalize a requested root to a repo-relative path prefix (or None = whole repo).

    Git emits paths as `crates/...`, so a `./crates` or `crates/` form must be
    collapsed or the prefix match silently excludes everything.
    """
    if not root or root in (".", "./"):
        return None
    r = root
    if os.path.isabs(r):
        try:
            top = subprocess.run(
                ["git", "rev-parse", "--show-toplevel"],
                capture_output=True, text=True, check=True,
            ).stdout.strip()
            r = os.path.relpath(r, top)
        except Exception:
            return None
    r = os.path.normpath(r)  # ./crates -> crates, crates/ -> crates, a/./b -> a/b
    if r == "." or r == os.curdir:
        return None  # explicit whole-repo
    # A root outside the checkout normalizes to a `..`-escaping path. Git never emits
    # `..`-prefixed paths, so returning it as the (non-matching) prefix scopes the
    # report to an honest empty result, rather than silently widening to whole-repo
    # and mislabelling unrelated findings as belonging to the requested directory.
    return r or None


def _under(path: str, prefix: str | None) -> bool:
    return prefix is None or path == prefix or path.startswith(prefix + "/")


def _tracked_paths() -> set[str]:
    """Paths present in the analyzed revision (HEAD).

    A renamed or deleted file's old path lingers in history forever; restricting
    churn and coupling candidates to paths that still exist keeps the report about
    code a reader can actually open and consolidate. Their historical commits still
    feed the co-change counts of the files that survive alongside them.
    """
    out = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", "HEAD"],
        capture_output=True, text=True, check=True,
    ).stdout
    return set(out.splitlines())


def commits(all_refs: bool = False):
    """Yield `(total_changed, code_files)` per commit.

    `total_changed` counts *every* path the commit touched (Rust, SQL, migrations,
    docs, config, ...), so sweep detection sees the true commit size; `code_files`
    is the `.rs`/`.js` subset (minus generated/vendored paths) used for churn and
    co-change pairs. Filtering to code before counting would let a bulk commit of
    two Rust files plus a hundred migrations masquerade as a focused change.

    History is the checked-out revision only (`git log HEAD`) so two clones at the
    same commit yield identical counts; `all_refs=True` restores `--all` (every
    branch/tag/remote) for an explicit cross-branch view. Traversing `--all` by
    default let abandoned branch work perturb the "deterministic" data contract.
    """
    log_args = ["git", "log"]
    log_args.append("--all" if all_refs else "HEAD")
    log_args += ["--pretty=format:%H", "--name-only"]
    out = subprocess.run(log_args, capture_output=True, text=True, check=True).stdout
    total = 0
    files: list[str] = []
    for line in out.splitlines():
        if not line.strip():
            continue
        if len(line) == 40 and all(c in "0123456789abcdef" for c in line):
            if total or files:
                yield total, files
            total, files = 0, []
        else:
            total += 1  # every changed path counts toward the sweep size
            if line.endswith(TRACKED_SUFFIXES) and not any(e in line for e in EXCLUDE):
                files.append(line)
    if total or files:
        yield total, files


def collect(min_co: int = 5, min_ratio: float = 0.6, max_commit_files: int = 30,
            root: str | None = None, all_refs: bool = False) -> tuple[list[_report.Finding], Counter]:
    prefix = _scope_prefix(root)
    tracked = _tracked_paths()
    churn: Counter[str] = Counter()      # scoped files across all commits (churn report)
    co_churn: Counter[str] = Counter()   # scoped files in non-sweep commits (ratio denominator)
    co: Counter[tuple[str, str]] = Counter()
    for total_changed, files in commits(all_refs):
        # Sweep detection uses the commit's *total* changed-file count (all types),
        # so a mass commit is recognized as a sweep even when only a few of its
        # files are code; the subtree/suffix filter applies to churn and pairs only.
        # Candidates are also restricted to paths that still exist at HEAD, so a
        # deleted/renamed-away file cannot surface as a phantom churn or coupling row.
        scoped = sorted(f for f in set(files) if f in tracked and _under(f, prefix))
        churn.update(scoped)
        if total_changed > max_commit_files:
            continue
        co_churn.update(scoped)
        for a, b in combinations(scoped, 2):
            co[(a, b)] += 1

    findings: list[_report.Finding] = []
    for (a, b), n in co.items():
        if n < min_co:
            continue
        ratio = n / min(co_churn[a], co_churn[b])
        if ratio < min_ratio:
            continue
        cross_dir = a.rsplit("/", 1)[0] != b.rsplit("/", 1)[0]
        findings.append(_report.Finding(
            tool="coupling", kind="temporal-coupling",
            summary=f"{a} <=> {b} co-changed {n}x (ratio {ratio:.2f}{', cross-dir' if cross_dir else ''})",
            score=round(ratio, 4), severity="medium" if cross_dir and n >= min_co else "low",
            locations=[_report.Loc(a), _report.Loc(b)],
            metrics={"co_changes": n, "ratio": round(ratio, 4), "cross_dir": cross_dir},
        ))
    findings.sort(key=lambda f: (f.metrics["co_changes"], f.score), reverse=True)
    return findings, churn


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default=None, help="optional repo-relative subtree to scope every metric to (default: whole repo)")
    ap.add_argument("--min-co", type=int, default=5, help="minimum co-changes for a coupled pair (default: 5)")
    ap.add_argument("--min-ratio", type=float, default=0.6, help="minimum coupling ratio (default: 0.6)")
    ap.add_argument("--max-commit-files", type=int, default=30, help="ignore commits touching more than N changed files of any type (default: 30)")
    ap.add_argument("--all-refs", action="store_true", help="traverse every branch/tag/remote (git log --all) instead of just HEAD (non-deterministic across clones)")
    ap.add_argument("--limit", type=int, default=25)
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings, churn = collect(args.min_co, args.min_ratio, args.max_commit_files, root=args.root, all_refs=args.all_refs)
    if args.json:
        _report.print_json(findings)
        return 0

    print("=== CHURN (most-changed tracked files) ===")
    for path, n in churn.most_common(args.limit):
        print(f"{n:4d}  {path}")
    print(f"\n=== TEMPORAL COUPLING (co-changes >= {args.min_co}, ratio >= {args.min_ratio}) ===")
    for f in findings[: args.limit]:
        a, b = f.locations
        cross = "XDIR " if f.metrics["cross_dir"] else "     "
        print(f"{f.score:.2f}  n={f.metrics['co_changes']:<3d} {cross} {a.file}  <=>  {b.file}")
    print(f"# {len(findings)} coupled pairs (XDIR = different directories, the more interesting ones)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
