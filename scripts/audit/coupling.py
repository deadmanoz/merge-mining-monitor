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

Both halves reach the machine-readable contract: coupling pairs plus each churn
hotspot (change count >= --min-churn) are emitted as findings under --json, so the
ranked aggregate is not missing the churn half the human CLI prints.

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
import sys
from collections import Counter
from itertools import combinations

import _report
import _scan

TRACKED_SUFFIXES = (".rs", ".js")
EXCLUDE = ("generated", "Cargo.lock", "/vendor/", "node_modules/")


def _scope_prefix(root: str | None, repo: str | None) -> str | None:
    """Normalize a requested root to a repo-relative path prefix (or None = whole repo).

    Git emits paths as `crates/...`, so an absolute or `./crates` / `crates/` form must
    be collapsed to that shape or the prefix match silently excludes everything. The
    root is relativized against the *scanned* work tree (`repo`, resolved from the root
    itself), so a subtree passed as an absolute path or from a different CWD still maps
    to git's repo-relative form.
    """
    if not root or root in (".", "./"):
        return None
    base = repo or "."
    try:
        r = os.path.relpath(os.path.realpath(root), os.path.realpath(base))
    except (ValueError, OSError):
        r = os.path.normpath(root)
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


def _tracked_paths(repo: str | None) -> set[str]:
    """Paths present in the analyzed revision (HEAD) of `repo`.

    A renamed or deleted file's old path lingers in history forever; restricting
    churn and coupling candidates to paths that still exist keeps the report about
    code a reader can actually open and consolidate. Their historical commits still
    feed the co-change counts of the files that survive alongside them.
    """
    # `--full-tree` lists the whole tree with repo-root-relative paths regardless of
    # the process's working directory; without it `git ls-tree` scopes to the CWD and
    # (run from a subdir) the returned set would not match `git log`'s repo-relative
    # paths, silently filtering every file out. `cwd=repo` runs against the scanned
    # checkout, not the process CWD, so an out-of-tree scan root still resolves.
    out = subprocess.run(
        ["git", "ls-tree", "-r", "--full-tree", "--name-only", "HEAD"],
        capture_output=True, text=True, check=True, cwd=repo,
    ).stdout
    return set(out.splitlines())


def _is_shallow(repo: str | None) -> bool:
    """True in a shallow checkout, where `git log` only sees a truncated slice of
    history. Coupling and churn would then depend on the clone's fetch depth rather
    than the code, so the caller reports incompleteness instead of partial numbers."""
    try:
        out = subprocess.run(
            ["git", "rev-parse", "--is-shallow-repository"],
            capture_output=True, text=True, check=True, cwd=repo,
        ).stdout.strip()
    except Exception:
        return False
    return out == "true"


def _resolve_rename(rename_to: dict[str, str], path: str) -> str:
    """Follow a chain of `old -> new` rename links to the current (HEAD-era) name.
    A `seen` guard tolerates a pathological rename-swap cycle in history."""
    seen: set[str] = set()
    while path in rename_to and path not in seen:
        seen.add(path)
        path = rename_to[path]
    return path


def commits(all_refs: bool = False, repo: str | None = None):
    """Yield `(total_changed, code_files)` per commit, with historical paths mapped
    onto their current name.

    `total_changed` counts *every* path the commit touched (Rust, SQL, migrations,
    docs, config, ...), so sweep detection sees the true commit size; `code_files`
    is the `.rs`/`.js` subset (minus generated/vendored paths) used for churn and
    co-change pairs. Filtering to code before counting would let a bulk commit of
    two Rust files plus a hundred migrations masquerade as a focused change.

    `--name-status -M` surfaces renames; walking newest-first we record each
    `old -> new` link as it appears (newer links already seen), so every changed
    path resolves to the name it carries at HEAD. A file that survives a rename thus
    keeps its full pre-rename history under one path (`csv_source.rs` folds into
    `csv_source/mod.rs`), while a genuinely deleted path resolves to a name absent
    from HEAD and is dropped downstream - no undercount, no resurrected ghost.

    Rename ancestry is severed at a delete+recreate boundary. Walking newest-first, a
    creation (`A <name>`) marks where the LIVE generation of that name began: any OLDER
    reference to the exact name (a pre-deletion edit, the delete itself, or a rename
    INTO it) belongs to a prior, unrelated generation and is dropped. Otherwise
    `a.rs -> b.rs`, `b.rs` deleted, then a fresh unrelated `b.rs` created would fold the
    old `a.rs`'s churn into today's `b.rs`. A file that is only renamed (never deleted
    and reborn) has no such `A`, so its full pre-rename history still chains through.

    History is the checked-out revision only (`git log HEAD`) so two clones at the
    same commit yield identical counts; `all_refs=True` restores `--all` (every
    branch/tag/remote) for an explicit cross-branch view. Traversing `--all` by
    default let abandoned branch work perturb the "deterministic" data contract.
    `cwd=repo` runs git against the scanned checkout rather than the process CWD.
    """
    log_args = ["git", "log"]
    log_args.append("--all" if all_refs else "HEAD")
    # `%x00` prefixes each commit header with a NUL, an explicit delimiter that marks
    # a new commit regardless of object-ID width. Detecting the header by a 40-hex
    # SHA-1 shape instead silently discarded every line in a SHA-256 repo (64-char
    # IDs), emptying both reports. NUL cannot occur in a path or a name-status line
    # and is not a `str.splitlines()` boundary, so the hash value itself is unused.
    log_args += ["--pretty=format:%x00%H", "--name-status", "-M"]
    out = subprocess.run(log_args, capture_output=True, text=True, check=True, cwd=repo).stdout
    rename_to: dict[str, str] = {}
    # Names whose live-generation start (an `A`, newest-first) has been passed; an older
    # reference to the exact name is a prior, unrelated generation and is dropped.
    sealed: set[str] = set()
    total = 0
    files: list[str] = []
    started = False
    for line in out.splitlines():
        if line.startswith("\x00"):  # commit-header delimiter (hash width irrelevant)
            if started:
                yield total, files
            total, files, started = 0, [], True
            continue
        if not line.strip():
            continue
        parts = line.split("\t")
        total += 1  # one changed path (a rename counts once) toward the sweep size
        status = parts[0]
        if status.startswith("R") and len(parts) >= 3:
            old, new = parts[1], parts[2]
            if new in sealed:
                # This rename produced a generation later deleted and reborn under the
                # same name (an `A <new>` seen newer). Don't chain `old` into the live
                # file; seal `old` so its own older history drops too.
                sealed.add(old)
                continue
            # Bind `old` to `new`'s CURRENT terminal name, not to `new` literally: a
            # rename-back (`a -> b` ... later `b -> a`, no deletion) would otherwise
            # make rename_to[a]=b and rename_to[b]=a, a cycle whose `_resolve_rename`
            # seen-guard strands part of `a`'s history on `b` and drops it. Resolving
            # first collapses the chain so every old name points straight at the HEAD
            # name (a rename-back yields a harmless self-map the guard absorbs).
            target = _resolve_rename(rename_to, new)
            rename_to[old] = target
            # Unseal `old`: if a NEWER `A old` sealed it (a fresh, unrelated file reusing
            # the old path while `new` stays live), this rename proves the older `old`
            # history is a DIFFERENT, earlier generation that continues as `new`. Older
            # `old` edits must now follow the rename to `new`, not be dropped as the new
            # generation's prehistory. `old`'s own creation re-seals it when reached.
            sealed.discard(old)
            cur = target
        else:  # A/M/D (or C copy: attribute the destination path)
            dest = parts[-1]
            if dest in sealed:
                continue  # older activity on a name whose live generation began newer
            cur = _resolve_rename(rename_to, dest)
            if status == "A":
                sealed.add(dest)  # creation: older refs to this name are a prior gen
        if cur.endswith(TRACKED_SUFFIXES) and not any(e in cur for e in EXCLUDE):
            files.append(cur)
    if started:
        yield total, files


def collect(min_co: int = 5, min_ratio: float = 0.6, max_commit_files: int = 30,
            root: str | None = None, all_refs: bool = False,
            min_churn: int = 10) -> tuple[list[_report.Finding], Counter]:
    # Resolve the repo from the scanned root, not the process CWD, so every git command
    # runs against the checkout that owns `root` (an absolute or out-of-tree scan root
    # otherwise queried the wrong repo, or none). Falls back to the CWD repo when no
    # root is given.
    repo = (_scan.git_root_of(root) if root else None) or _scan._git_root()
    prefix = _scope_prefix(root, repo)
    if _is_shallow(repo):
        # A shallow clone carries only a truncated slice of history, so `git log`
        # would silently under-count churn and coupling (a depth-1 clone reports zero
        # pairs where a full clone reports many). Emit one explicit incompleteness
        # marker and no pairs rather than a valid-looking but fetch-depth-dependent
        # data contract.
        marker = _report.Finding(
            tool="coupling", kind="coupling-incomplete",
            summary="shallow repository - git history is truncated; churn/coupling omitted "
                    "(run in a full clone or `git fetch --unshallow`)",
            severity="info", locations=[], metrics={"shallow": True},
        )
        return [marker], Counter()
    tracked = _tracked_paths(repo)
    churn: Counter[str] = Counter()      # scoped files across all commits (churn report)
    co_churn: Counter[str] = Counter()   # scoped files in non-sweep commits (ratio denominator)
    co: Counter[tuple[str, str]] = Counter()
    for total_changed, files in commits(all_refs, repo):
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
    # Churn hotspots belong in the machine-readable contract too: the detector
    # advertises "churn + temporal coupling", but only the coupling half was ever
    # serialized (the Counter was returned for the human table and then discarded by
    # `--json`/`report.py`). Emit each high-churn file as an `info` finding (a lead,
    # not a defect) so the JSON array and the ranked aggregate carry both halves. The
    # Counter is still returned unchanged for the CLI's full CHURN table.
    for path, n in churn.items():
        if n < min_churn:
            continue
        findings.append(_report.Finding(
            tool="coupling", kind="git-churn",
            summary=f"{path} changed {n}x (churn hotspot)",
            score=float(n), severity="info",
            locations=[_report.Loc(path)],
            metrics={"changes": n},
        ))
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
    # Coupling pairs first (by co-change strength), then churn hotspots (by count), so
    # a `--json` consumer sees the behavioral smells ahead of the raw hotspots.
    findings.sort(
        key=lambda f: (f.kind == "temporal-coupling", f.metrics.get("co_changes", 0), f.score),
        reverse=True,
    )
    return findings, churn


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default=None, help="optional repo-relative subtree to scope every metric to (default: whole repo)")
    ap.add_argument("--min-co", type=int, default=5, help="minimum co-changes for a coupled pair (default: 5)")
    ap.add_argument("--min-ratio", type=float, default=0.6, help="minimum coupling ratio (default: 0.6)")
    ap.add_argument("--max-commit-files", type=int, default=30, help="ignore commits touching more than N changed files of any type (default: 30)")
    ap.add_argument("--min-churn", type=int, default=10, help="minimum change count for a file to appear as a churn finding in --json (default: 10)")
    ap.add_argument("--all-refs", action="store_true", help="traverse every branch/tag/remote (git log --all) instead of just HEAD (non-deterministic across clones)")
    ap.add_argument("--limit", type=int, default=25)
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings, churn = collect(args.min_co, args.min_ratio, args.max_commit_files, root=args.root, all_refs=args.all_refs, min_churn=args.min_churn)
    if args.json:
        _report.print_json(findings)
        return 0

    incomplete = [f for f in findings if f.kind == "coupling-incomplete"]
    if incomplete:
        print(f"# coupling skipped: {incomplete[0].summary}", file=sys.stderr)
        return 0

    pairs = [f for f in findings if f.kind == "temporal-coupling"]
    print("=== CHURN (most-changed tracked files) ===")
    for path, n in churn.most_common(args.limit):
        print(f"{n:4d}  {path}")
    print(f"\n=== TEMPORAL COUPLING (co-changes >= {args.min_co}, ratio >= {args.min_ratio}) ===")
    for f in pairs[: args.limit]:
        a, b = f.locations
        cross = "XDIR " if f.metrics["cross_dir"] else "     "
        print(f"{f.score:.2f}  n={f.metrics['co_changes']:<3d} {cross} {a.file}  <=>  {b.file}")
    print(f"# {len(pairs)} coupled pairs (XDIR = different directories, the more interesting ones)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
