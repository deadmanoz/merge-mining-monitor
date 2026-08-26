#!/usr/bin/env python3
"""Scattered-config detector: env-var reads, config structs, and doc drift.

The "scattered config" sprawl smell (config reconstructed in many places instead
of expressed once). For a Rust workspace this surfaces as `env::var`/lookup reads
spread across crates and per-area `*Config` structs. This tool inventories that
surface and cross-checks the keys against the documented ground truth
(`docs/configuration.md` and `.env.example`), reporting three-way drift.

Model note (this repo): per-chain keys are built as `format!("{PREFIX}_SUFFIX")`,
so the *suffix* is the literal in code and the prefix comes from the chain spec.
The tool compares shared full keys directly and per-chain keys by suffix, which
avoids the false drift a naive prefix x suffix expansion would produce.

Advisory; stdlib-only. Emits the shared finding schema with --json.
"""

from __future__ import annotations

import argparse
import os
import re

import _report
import _scan

# Cargo/build-time env vars are not application configuration; exclude them so
# they do not masquerade as scattered runtime config (they cluster in fixtures).
# The toolchain names are enumerated (plus the `RUSTC_`/`RUSTUP_` prefixes) rather
# than filtering every `RUST*` name: a blanket `RUST` prefix also swallowed runtime
# variables an application legitimately reads - `RUST_LOG`, `RUST_BACKTRACE` - and
# project keys like `RUSTY_SERVICE_TOKEN`, dropping their read sites from the whole
# inventory so they evaded undocumented/multi-read/unread findings.
BUILD_ENV = {
    "OUT_DIR", "TARGET", "HOST", "PROFILE", "NUM_JOBS",
    "RUSTC", "RUSTDOC", "RUSTFLAGS", "RUSTDOCFLAGS",
}
BUILD_ENV_PREFIXES = ("CARGO_", "RUSTC_", "RUSTUP_", "LD_", "DYLD_")


def _is_build_env(key: str) -> bool:
    return key in BUILD_ENV or key.startswith(BUILD_ENV_PREFIXES)


# A full env key: either the usual UNDERSCORE_SEPARATED shape, or a single
# all-caps run of >= 4 chars (e.g. `PGHOST`, `PGPASSWORD`) so underscore-free
# keys are recognized. The >= 4 floor and prefix filter in `doc_tokens` keep short
# words (RPC/SQL/URL) and bare chain prefixes out.
FULLKEY = re.compile(r"^[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+$|^[A-Z][A-Z0-9]{3,}$")
SUFFIX = re.compile(r"^_[A-Z0-9]+(?:_[A-Z0-9]+)*$")
# A per-chain key *constructed* as `format!("{prefix}_RPC_USER")`: the ENTIRE
# literal is one placeholder immediately followed by the `_SUFFIX` run, so the
# `^`/`$` anchors require the whole (stripped) literal to be exactly `{...}` +
# `_SUFFIX` and nothing else. Anchoring is what separates a real key construction
# from a message that merely embeds a rendered name - e.g. the error literal
# `"... exceeds {}_MAX_BACKFILL_RANGE {max}; set {}_ALLOW_LARGE_BACKFILL=1 ..."`,
# whose interior placeholders an unanchored search would harvest as false suffix
# evidence, keeping a family "built in code" and masking a `config-unread` result.
CONSTRUCT_SUFFIX = re.compile(r"^\{[^}]*\}((?:_[A-Z0-9]+)+)$")
# Only actual read *calls*: the name must be followed by `(`. This excludes the
# `std::env` namespace itself (`std::env::Args`), the `fn env_lookup` definition,
# and places that pass `env_lookup` as a callback value - all of which inflate the
# surface count without reading the environment.
#
# A BARE `env::var(...)` is only a process-env read when the file imports the stdlib
# module (`use std::env;`); a `crate::env::var(...)` is a call into a LOCAL module
# named `env` and must NOT count. So the read is split three ways: the fully-qualified
# `std::env::var` (always), the repo's own `env_lookup` primitive (always), and the
# bare `env::var` form (only when `has_std_env`, and never the `std::env::var` tail -
# `(?<![:\w])` rejects any `X::env::var`). The caller gates the bare form per file.
ENV_READ_STD = re.compile(r"\bstd::env::var(?:_os)?\s*\(")
ENV_READ_BARE = re.compile(r"(?<![:\w])env::var(?:_os)?\s*\(")
ENV_READ_LOOKUP = re.compile(r"(?<!fn )\benv_lookup\s*\(")
# Does the file bring the stdlib `env` MODULE into scope (so a bare `env::var(...)` is
# a std read, not a local-module call)? Matches `use std::env;` and a grouped
# `use std::{env, fs};`. `use std::env::var;` imports the function `var`, not the
# module, and is handled by env_read_aliases instead.
USE_STD_ENV = re.compile(r"\buse\s+std::env\s*;|\buse\s+std::\{[^}]*\benv\b[^}]*\}")
# A local function call `name(` in a fn body - used to propagate env-reading status
# transitively (a() -> b() -> env::var). `(?<![.\w:])` excludes method calls (`.foo()`),
# associated/qualified calls (`Type::foo()`, `mod::foo()`), and mid-identifier hits, so
# only free-function calls (a local helper) are followed.
CALLEE = re.compile(r"(?<![.\w:])([a-z_][a-z0-9_]*)\s*\(")
# All-caps names bound locally - by `let`/`let mut` or as a fn/closure parameter. An
# `env::var(THIS)` whose argument is such a binding reads a runtime value, so it must
# NOT be resolved against a same-named GLOBAL const (a different module's key).
LET_BIND = re.compile(r"\blet\s+(?:mut\s+)?([A-Z][A-Z0-9_]{2,})\b")
PARAM_BIND = re.compile(r"[(,|]\s*(?:mut\s+)?([A-Z][A-Z0-9_]{2,})\s*:")
# The env key passed directly to a read call. Keying off the call site (not any
# ALL_CAPS literal) avoids matching opcode/status constants like OP_RETURN. The
# `(?:r#*)?` accepts a raw-string key (`env::var(r"SERVICE_TOKEN")`,
# `env::var(r#"..."#)`): without it a valid raw-string read recorded no key, so it
# looked undocumented (or made a documented key look unread). The trailing `#`s of a
# hash-delimited raw literal are left unconsumed - harmless, since the ALL_CAPS key
# is already captured. No length floor on the key (`[A-Z][A-Z0-9_]*`, >= 1 char): the
# read-call anchor is already discriminating, so a legitimately short key
# (`env::var("CI")`, `var("TZ")`) is recorded rather than silently dropped and later
# mis-reported as unread. The broader helper/first-arg shapes below keep their floor.
KEYCALL = re.compile(
    r'(?:std::env::var(?:_os)?|env_lookup|\blookup|getenv)\s*\(\s*&?\s*(?:r#*)?"([A-Z][A-Z0-9_]*)"'
)
# The bare `env::var("KEY")` form, kept separate so the caller can gate it on
# `has_std_env` (a `crate::env::var("KEY")` is a local-module call, not a std read).
# `(?<![:\w])` rejects any qualified `X::env::var` and the `std::env::var` tail (already
# covered above).
KEYCALL_BARE_ENV = re.compile(
    r'(?<![:\w])env::var(?:_os)?\s*\(\s*&?\s*(?:r#*)?"([A-Z][A-Z0-9_]*)"'
)
# The same read calls, but with a bare identifier argument (a key passed through a
# constant). Paired with CONST_KEY, this recovers keys KEYCALL's literal form misses.
KEYCALL_IDENT = re.compile(
    r'(?:std::env::var(?:_os)?|env_lookup|\blookup|getenv)\s*\(\s*&?\s*([A-Z][A-Z0-9_]{2,})\s*\)'
)
KEYCALL_IDENT_BARE_ENV = re.compile(
    r'(?<![:\w])env::var(?:_os)?\s*\(\s*&?\s*([A-Z][A-Z0-9_]{2,})\s*\)'
)
# A key handed to a helper that performs the read on the caller's behalf, made
# recognizable because the read fn is passed alongside it, e.g.
# `exact_one_from_lookup("HATHOR_BACKFILL_SKIP_HOLDS", env_lookup)`. Without this the
# key is invisible - the literal is not a direct argument to a recognized read call -
# so it looks undocumented-in-reverse (present in docs, "unused" in code). No length
# floor on the key (`[A-Z][A-Z0-9_]*`): the trailing read fn already discriminates, so
# a short key (`("CI", env_lookup)`) is recorded like a direct read, not dropped.
KEYCALL_HELPER = re.compile(
    r'(?:r#*)?"([A-Z][A-Z0-9_]*)"\s*,\s*(?:&\s*)?(?:env_lookup|env::var(?:_os)?)\b'
)
# A key handed as the *first* argument to any local function, e.g.
# `parse_env_or("BITCOIN_RPC_TIMEOUT_SECS", 30)`. On its own this shape is too broad
# (it also matches `insert("SOME_CONST", ..)`), so a hit is kept only when the callee
# is an imported read primitive/alias or a discovered env-reading helper (a fn whose
# body reads the environment). That callee identity - not a key-length floor - is the
# discriminator, so no floor is imposed here (`[A-Z][A-Z0-9_]*`, >= 1 char): an
# imported `use std::env::var; var("CI")` or a confirmed helper `read_env("TZ")` with
# a one/two-char key is recorded like a qualified direct read, while a non-helper
# `insert("ID", ..)` still collects a candidate that resolution then discards.
HELPER_CALL = re.compile(r'\b([a-z_][a-z0-9_]*)\s*\(\s*&?\s*(?:r#*)?"([A-Z][A-Z0-9_]*)"')
# Direct read primitives already inventoried by KEYCALL/KEYCALL_IDENT; skip them in
# the HELPER_CALL pass so a `env::var("KEY")` site is not counted twice (its callee
# name captures as `var`).
READ_PRIMITIVES = {"var", "var_os", "env_lookup", "lookup", "getenv"}
CONST_KEY = re.compile(
    r'\b(?:const|static)\s+([A-Z][A-Z0-9_]*)\s*:\s*&\s*(?:\'static\s+)?str\s*=\s*(?:r#*)?"([A-Z][A-Z0-9_]{2,})"'
)
# A `use` that imports a std::env read primitive under some local name:
# `use std::env::var;`, `use std::env::var_os as getenv;`, or a group
# `use std::env::{var, var_os as v};`. The captured body (a single item or a `{...}`
# group) is parsed by env_read_aliases into the bound local names.
ENV_USE = re.compile(r"\buse\s+std::env::(\{[^}]*\}|[A-Za-z0-9_]+(?:\s+as\s+[A-Za-z0-9_]+)?)\s*;")
CONFIG_STRUCT = re.compile(r"\bstruct\s+([A-Za-z0-9_]*Config)\b")
DEFAULT_PREFIXES = ["NAMECOIN", "RSK", "SYSCOIN", "FRACTAL", "HATHOR", "ELASTOS"]


def parse_prefixes(docs: str) -> list[str]:
    m = re.search(r"Prefixes?:\s*(.+)", docs)
    if m:
        found = re.findall(r"`([A-Z][A-Z0-9]+)`", m.group(1))
        if found:
            return found
    return DEFAULT_PREFIXES


def read_text(path: str) -> str:
    try:
        return open(path, encoding="utf-8", errors="ignore").read()
    except OSError:
        return ""


def env_keys(text: str) -> set[str]:
    """Env keys declared in a .env-style file (including commented `# KEY=...`)."""
    return set(re.findall(r"^\s*#?\s*([A-Z][A-Z0-9_]+)=", text, flags=re.M))


def env_read_aliases(text: str) -> set[str]:
    """Local identifiers bound to `std::env::var`/`var_os` through a `use` import.

    `use std::env::var;` binds `var`; `use std::env::var_os as getenv;` binds
    `getenv`; `use std::env::{var, var_os as v};` binds `var` and `v`. A bare call to
    such a name (`var("KEY")`) is a direct environment read the qualified
    `env::var(...)` patterns cannot see, so recognizing it (scoped to the importing
    file) keeps a documented key from looking unread."""
    names: set[str] = set()
    for m in ENV_USE.finditer(text):
        body = m.group(1)
        items = body[1:-1].split(",") if body.startswith("{") else [body]
        for it in items:
            parts = it.split()  # ["var"] or ["var", "as", "alias"]
            if parts and parts[0] in ("var", "var_os"):
                names.add(parts[2] if len(parts) >= 3 and parts[1] == "as" else parts[0])
    return names


# A `$VAR` / `${VAR}` shell reference to an UPPER_SNAKE variable.
SHELL_VAR = re.compile(r"\$\{?([A-Z][A-Z0-9_]{2,})\}?")


def _shell_runtime_refs(text: str) -> set[str]:
    """`$VAR`/`${VAR}` references a shell would actually *expand*.

    Two constructs carry a `$KEY` that never runs, so counting them as tooling
    evidence would wrongly suppress a `config-unread` finding for an obsolete key: a
    `#` comment (only when it starts a word - not the `#` inside `${VAR#pat}`) and a
    single-quoted span (POSIX single quotes suppress expansion entirely). Double-quoted
    text still expands, so it is kept. A byte-level scanner, since a regex cannot tell
    a live `$KEY` from one buried in a comment or single quotes."""
    kept: list[str] = []
    i, n = 0, len(text)
    quote: str | None = None  # None | "'" (no expansion) | '"' (expansion)
    while i < n:
        c = text[i]
        if quote == "'":
            if c == "'":
                quote = None  # drop the single-quoted contents entirely
            i += 1
            continue
        if quote == '"':
            kept.append(c)
            if c == '"':
                quote = None
            i += 1
            continue
        if c == "'":
            quote = "'"
            i += 1
            continue
        if c == '"':
            quote = '"'
            kept.append(c)
            i += 1
            continue
        if c == "#" and (i == 0 or text[i - 1] in " \t\n"):
            nl = text.find("\n", i)  # comment to end of line
            i = n if nl < 0 else nl
            continue
        kept.append(c)
        i += 1
    return set(SHELL_VAR.findall("".join(kept)))


def tooling_referenced_keys(root: str | None = None) -> set[str]:
    """UPPER_SNAKE env names referenced by non-Rust operational files - the `justfile`
    and `scripts/**` shell scripts - as `$VAR`/`${VAR}`.

    A key consumed only by tooling (e.g. `${MMM_POOLS_DIR:-}` in a `just` recipe) is
    genuinely *read*, just not by the Rust binary, so the stale-config check must not
    report it. `root` is the scanned repository's work-tree root; the caller resolves
    it from the scan path (`git_root_of`), not the process CWD, so a scan launched
    outside the checkout still finds the `justfile`/`scripts/**` sitting next to the
    code instead of discarding every reference and mis-flagging a tooling-only key as
    `config-unread`. Falls back to the CWD repo only when no root is supplied."""
    if root is None:
        root = _scan._git_root()
    if not root:
        return set()
    files = [os.path.join(root, "justfile")]
    scripts = os.path.join(root, "scripts")
    for dp, dns, fns in os.walk(scripts):
        dns[:] = [d for d in dns if d != "audit"]  # our own tooling, not app config
        files += [os.path.join(dp, f) for f in fns if f.endswith((".sh", ".bash"))]
    refs: set[str] = set()
    for p in files:
        try:
            refs.update(_shell_runtime_refs(open(p, encoding="utf-8", errors="ignore").read()))
        except OSError:
            continue
    return refs


def doc_tokens(docs: str, prefixes: list[str]) -> tuple[set[str], set[str], set[str]]:
    """Full keys, all per-chain suffixes, and template-only suffixes documented in
    configuration.md.

    A `<PREFIX>_RPC_URL` template contributes the suffix `_RPC_URL` (to both suffix
    sets); a bare backticked `PGHOST` contributes a full key; a concrete per-chain
    key like `RSK_BACKFILL_...` contributes a full key *and* its suffix to the
    combined set only. The template-only set is returned separately so the reverse
    (example) drift check can flag a documented `<PREFIX>_X` family with no example
    instance without double-reporting a concrete full key that the full-key check
    already covers. `` `X` / `Y` `` splits are handled per backticked token.
    """
    full: set[str] = set()
    suffixes: set[str] = set()
    template_suffixes: set[str] = set()
    for tok in re.findall(r"`([^`]+)`", docs):
        tok = tok.strip()
        if tok.startswith("<PREFIX>_") or tok.startswith("<PREFIX>"):
            suf = tok[len("<PREFIX>"):]
            if SUFFIX.match(suf):
                suffixes.add(suf)
                template_suffixes.add(suf)
        elif tok in prefixes:
            # A bare chain prefix (`ELASTOS`) is a prefix, not a full key; the
            # broadened FULLKEY would otherwise mis-file it into `doc_full`.
            continue
        elif FULLKEY.match(tok):
            # A documented full key that starts with a chain prefix is really a
            # per-chain example; record its suffix too.
            full.add(tok)
            for p in prefixes:
                if tok.startswith(p + "_"):
                    suffixes.add(tok[len(p):])
    return full, suffixes, template_suffixes


def is_chain_scoped(key: str, prefixes: list[str]) -> bool:
    return any(key.startswith(p + "_") for p in prefixes)


def _inventory(root: str, include_tests: bool) -> tuple[dict[str, int], list[_report.Loc], dict[str, list[_report.Loc]], set[str]]:
    """Scan a tree into the raw config inventory: per-file env-read counts, *Config
    struct locations, per-key read locations, and per-chain suffixes built in code.

    Factored out of `collect` so the reverse-drift (config-unread) check can run it a
    second time over the whole workspace, independent of a partial detector root -
    otherwise a key read in a sibling crate looks unread when a single crate is
    scanned."""
    read_files: dict[str, int] = {}
    struct_locs: list[_report.Loc] = []
    key_locs: dict[str, list[_report.Loc]] = {}
    code_suffixes: set[str] = set()
    # `const KEY_ENV: &str = "SOME_KEY"` declarations, resolved after the scan so a
    # `var_os(KEY_ENV)` call site counts against the real key, not a phantom.
    # Declarations are tracked per file (a same-file declaration is authoritative)
    # plus a repo-wide map with an ambiguity set, so a conventional constant name
    # reused for different keys across modules is not collapsed to one global key.
    const_by_file: dict[str, dict[str, str]] = {}
    const_global: dict[str, str] = {}
    const_ambig: set[str] = set()
    ident_calls: list[tuple[str, str, _report.Loc]] = []  # (ident, file, loc)
    # Discovered env-reading helper fns, tracked per file plus a repo-wide view with
    # a counter-set of same-named fns that do NOT read env. A `NAME("KEY", ..)` call
    # is credited only via a same-file env-reading definition or an unambiguous
    # repo-wide one, so a common name (`from_env`, `parse_env_or`) reused for an
    # unrelated fn in another crate does not turn that crate's literals into keys.
    helper_env_by_file: dict[str, set[str]] = {}
    helper_env_names: set[str] = set()
    helper_nonenv_names: set[str] = set()
    helper_calls: list[tuple[str, str, _report.Loc]] = []  # (fn_name, key, loc); loc.file scopes it
    # All-caps names bound locally per file (`let`/param). A `var(THIS)` whose argument
    # is such a binding must not be resolved to a same-named GLOBAL const.
    binds_by_file: dict[str, set[str]] = {}

    for path in _scan.iter_rust_files(root, skip_tests=not include_tests):
        src = read_text(path)
        if not src:
            continue
        rel = _scan.rel(path)
        # Structural/count matches run on de-noised source so a `//! uses env::var`
        # doc line or a struct name inside a string is not miscounted. Newline count
        # is preserved, so match offsets still map to the correct original line.
        stripped = _scan.strip_noise(src)
        # Key-call discovery needs the literal key *contents* (which strip_noise
        # collapses), but must still ignore a commented-out `// env::var("PHANTOM")`,
        # so it runs on comment-stripped-but-string-preserving source.
        decommented = _scan.strip_comments(src)
        # Local names this file binds to std::env::var/var_os via `use`; a bare
        # `var("KEY")` is then a direct read the qualified patterns cannot see.
        file_env_aliases = env_read_aliases(stripped)
        # Does this file import the stdlib `env` module, so a bare `env::var(...)` is a
        # process-env read rather than a call into a local `mod env`?
        has_std_env = bool(USE_STD_ENV.search(stripped))
        # All-caps `let`/param bindings; an `env::var(THIS)` with such an argument is a
        # runtime value, not the same-named global const.
        binds_by_file[rel] = set(LET_BIND.findall(stripped)) | set(PARAM_BIND.findall(stripped))
        reads = len(ENV_READ_STD.findall(stripped)) + len(ENV_READ_LOOKUP.findall(stripped))
        if has_std_env:
            reads += len(ENV_READ_BARE.findall(stripped))
        alias_reads = 0  # bare imported-primitive reads (found in the HELPER_CALL pass)
        for m in CONFIG_STRUCT.finditer(stripped):
            struct_locs.append(_report.Loc(rel, stripped.count("\n", 0, m.start()) + 1, m.group(1)))
        keycall_iters = [KEYCALL.finditer(decommented)]
        if has_std_env:  # bare `env::var("KEY")` is a std read only with `use std::env;`
            keycall_iters.append(KEYCALL_BARE_ENV.finditer(decommented))
        for it in keycall_iters:
            for m in it:
                key = m.group(1)
                if not _is_build_env(key):
                    key_locs.setdefault(key, []).append(_report.Loc(rel, decommented.count("\n", 0, m.start()) + 1))
        for m in KEYCALL_HELPER.finditer(decommented):
            key = m.group(1)
            if not _is_build_env(key):
                key_locs.setdefault(key, []).append(_report.Loc(rel, decommented.count("\n", 0, m.start()) + 1))
        for m in CONST_KEY.finditer(decommented):
            ident, key = m.group(1), m.group(2)
            const_by_file.setdefault(rel, {})[ident] = key
            if ident in const_global and const_global[ident] != key:
                const_ambig.add(ident)  # same name, different key in another module
            const_global[ident] = key
        ident_iters = [KEYCALL_IDENT.finditer(decommented)]
        if has_std_env:
            ident_iters.append(KEYCALL_IDENT_BARE_ENV.finditer(decommented))
        for it in ident_iters:
            for m in it:
                ident_calls.append((m.group(1), rel, _report.Loc(rel, decommented.count("\n", 0, m.start()) + 1)))
        # A fn whose (de-noised) body performs an env read is an env helper; its name
        # qualifies the literal keys its callers pass. Bodies use `stripped` so a read
        # in a comment/string does not count; call sites use `decommented` to keep the
        # literal key text. Track env vs non-env defs per file and repo-wide so a
        # reused name can be disambiguated at resolution time.
        # A wrapper that reads through an *imported* primitive
        # (`use std::env::var; fn read(n) { var(n) }`) is still an env-reading helper,
        # but the qualified regex cannot see it. Match a call to one of this file's env
        # aliases in the body too; otherwise a `read("KEY")` caller is misclassified as
        # non-reading and its key is dropped (a false config-unread). Scoped to the
        # file's own `use`, so it never fires elsewhere.
        alias_call = (
            re.compile(r"\b(?:" + "|".join(re.escape(a) for a in file_env_aliases) + r")\s*\(")
            if file_env_aliases else None
        )

        def _reads_env(body: str) -> bool:
            if ENV_READ_STD.search(body) or ENV_READ_LOOKUP.search(body):
                return True
            if has_std_env and ENV_READ_BARE.search(body):
                return True
            return bool(alias_call and alias_call.search(body))

        # A helper need not read env *directly*: `a() { b() }` with `b() { env::var(..) }`
        # is an env reader too. Record each fn's direct-read flag and the local functions
        # it calls, then propagate env-reading status to a fixpoint WITHIN the file. The
        # closure is deliberately file-scoped (not a repo-wide name graph): bare fn names
        # collide across crates, and the resolution below already prefers a same-file def.
        file_fns: dict[str, list] = {}  # name -> [direct_env: bool, callees: set[str]]
        for fn in _scan.find_functions(stripped, path):
            callees = {cm.group(1) for cm in CALLEE.finditer(fn.body)}
            rec = file_fns.setdefault(fn.name, [False, set()])
            rec[0] = rec[0] or _reads_env(fn.body)
            rec[1] |= callees
        env_in_file = {name for name, (direct, _c) in file_fns.items() if direct}
        changed = True
        while changed:
            changed = False
            for name, (_direct, callees) in file_fns.items():
                if name not in env_in_file and (callees & env_in_file):
                    env_in_file.add(name)
                    changed = True
        for name in file_fns:
            if name in env_in_file:
                helper_env_by_file.setdefault(rel, set()).add(name)
                helper_env_names.add(name)
            else:
                helper_nonenv_names.add(name)
        for m in HELPER_CALL.finditer(decommented):
            fn_name, key = m.group(1), m.group(2)
            if _is_build_env(key):
                continue  # build-time vars are not application config
            loc = _report.Loc(rel, decommented.count("\n", 0, m.start()) + 1)
            # An imported `std::env::var`/`var_os` (or its alias) reads directly, so
            # credit the key here - before the READ_PRIMITIVES skip, which would drop a
            # bare `var(...)`. The identity comes from this file's `use`, so the
            # recognition stays scoped and never fires on an unrelated `var(...)`.
            if fn_name in file_env_aliases:
                key_locs.setdefault(key, []).append(loc)
                alias_reads += 1
                continue
            if fn_name in READ_PRIMITIVES:
                continue  # qualified direct reads handled by KEYCALL
            helper_calls.append((fn_name, key, loc))
        # Suffix evidence is taken only from a key-construction site - a
        # `format!("{prefix}_SUFFIX")` whose *whole* literal is the key - never from a
        # literal that merely embeds a rendered name (a bare `#[cfg(test)]`
        # enumeration, or an error/log message such as `"... {}_MAX_BACKFILL_RANGE
        # ..."`). Counting those kept a family looking "built in code" and masked a
        # genuine config-unread/undocumented-suffix result once its real lookup went.
        for lit, _off in _scan.iter_string_literals(src):
            cm = CONSTRUCT_SUFFIX.match(lit.strip())
            if cm:
                code_suffixes.add(cm.group(1))
        # Env-alias reads count toward this file's read tally (the qualified env-read
        # regexes cannot see a bare imported `var(...)`), so a file reading env only
        # through an imported primitive still registers as a reader.
        total_reads = reads + alias_reads
        if total_reads:
            read_files[rel] = total_reads

    # Resolve constant-passed keys once every declaration has been seen. A same-file
    # declaration wins (precise even when another module reuses the name); otherwise
    # fall back to the repo-wide map only when the name is unambiguous, so a reused
    # `RPC_URL_ENV` is skipped rather than misattributed to whichever file was
    # scanned last.
    for ident, file, loc in ident_calls:
        if ident in binds_by_file.get(file, ()):
            continue  # a local `let`/param binding shadows any same-named const
        key = const_by_file.get(file, {}).get(ident)
        if key is None and ident not in const_ambig:
            key = const_global.get(ident)
        if key and not _is_build_env(key):
            key_locs.setdefault(key, []).append(loc)

    # Attribute keys passed to discovered env-reading helpers (`_is_build_env` was
    # already applied when collecting the candidates). A same-file env-reading
    # definition is authoritative; otherwise credit the key only when the name is an
    # unambiguous env helper repo-wide (never also a non-env fn), so a cross-module
    # name collision is skipped rather than inventing keys.
    for fn_name, key, loc in helper_calls:
        if fn_name in helper_env_by_file.get(loc.file, ()):
            is_env = True
        elif fn_name in helper_env_names and fn_name not in helper_nonenv_names:
            is_env = True
        else:
            is_env = False
        if is_env:
            key_locs.setdefault(key, []).append(loc)

    # Collapse duplicate locations per key: the same call site can match more than one
    # pass (e.g. KEYCALL_HELPER and HELPER_CALL both see `exact_one_from_lookup("K", env_lookup)`),
    # which would otherwise inflate a key's read count and its multi-read severity.
    for key, locs in key_locs.items():
        seen: set[tuple[str, int]] = set()
        deduped: list[_report.Loc] = []
        for loc in locs:
            sig = (loc.file, loc.line)
            if sig not in seen:
                seen.add(sig)
                deduped.append(loc)
        key_locs[key] = deduped

    return read_files, struct_locs, key_locs, code_suffixes


def collect(root: str, docs_dir: str = "docs", env_example: str = ".env.example", include_tests: bool = False) -> list[_report.Finding]:
    # Resolve the relative docs/.env inputs against the *scanned* repo, not the
    # caller's CWD. Launched from outside the checkout with an absolute scan root, a
    # bare `docs/configuration.md`/`.env.example` reads nothing, and the whole
    # documented surface then looks undocumented and unread - a valid-looking but
    # false data contract. An absolute input, or a root outside any repo, is left as-is.
    repo = _scan.git_root_of(root)

    def _rooted(p: str) -> str:
        return p if (os.path.isabs(p) or not repo) else os.path.join(repo, p)

    docs = read_text(f"{_rooted(docs_dir)}/configuration.md")
    prefixes = parse_prefixes(docs)

    read_files, struct_locs, key_locs, code_suffixes = _inventory(root, include_tests)

    code_full = set(key_locs)
    example = env_keys(read_text(_rooted(env_example)))
    doc_full, doc_suffixes, doc_template_suffixes = doc_tokens(docs, prefixes)
    # Per-chain suffixes present in .env.example (e.g. NAMECOIN_RPC_URL -> _RPC_URL);
    # tracked separately (NOT merged into doc_suffixes) so the three-way drift stays
    # honest: a suffix present in code + .env.example but absent from the docs must
    # still surface as documentation drift, not be masked by the example.
    example_suffixes: set[str] = set()
    for k in example:
        for p in prefixes:
            if k.startswith(p + "_"):
                example_suffixes.add(k[len(p):])

    def suffix_of(key: str) -> str | None:
        for p in prefixes:
            if key.startswith(p + "_"):
                return key[len(p):]
        return None

    # A chain-scoped key read as a full literal (e.g. lookup("RSK_BACKFILL_...")) is
    # excluded from the shared-key drift check below; fold its suffix into the code
    # suffix set so it is still checked against the docs rather than dropped entirely.
    for k in code_full:
        s = suffix_of(k)
        if s:
            code_suffixes.add(s)

    # The reverse-drift (config-unread) check asks whether a documented+example key is
    # read *anywhere* in the project, so its code side must span the whole workspace.
    # A partial detector root (e.g. one crate) would otherwise flag keys read in
    # sibling crates as unread - a valid-looking but false data contract. Reuse the
    # root inventory when the root already is the workspace; else scan the git root
    # once more for the full read-key set. The per-file findings above stay scoped to
    # the requested root; only this reverse check widens. Uses the scanned repo (from
    # the root), so widening also works when launched from outside the checkout.
    ws_root = repo
    if ws_root and os.path.abspath(root) != os.path.abspath(ws_root):
        _rf, _sl, ws_key_locs, ws_suffixes = _inventory(ws_root, include_tests)
        full_code_full = set(ws_key_locs)
        full_code_suffixes = set(ws_suffixes)
        for k in full_code_full:
            s = suffix_of(k)
            if s:
                full_code_suffixes.add(s)
    else:
        full_code_full, full_code_suffixes = code_full, code_suffixes

    def in_docs(key: str) -> bool:
        return re.search(rf"\b{re.escape(key)}\b", docs) is not None

    findings: list[_report.Finding] = []

    findings.append(_report.Finding(
        tool="configscan", kind="config-surface",
        summary=f"env read in {len(read_files)} files; {len(struct_locs)} *Config structs; "
                f"{len(code_full)} full-key + {len(code_suffixes)} suffix literals",
        severity="info",
        locations=sorted((_report.Loc(f, 0, f"{n} reads") for f, n in read_files.items()), key=lambda l: -int(l.name.split()[0])),
        metrics={"read_files": read_files, "config_structs": [l.name for l in struct_locs], "prefixes": prefixes},
    ))

    if struct_locs:
        findings.append(_report.Finding(
            tool="configscan", kind="config-structs",
            summary=f"{len(struct_locs)} per-area *Config structs (no single config seam)",
            score=float(len(struct_locs)), severity="low", locations=sorted(struct_locs, key=lambda l: l.file),
        ))

    for key, locs in sorted(key_locs.items()):
        files = {l.file for l in locs}
        if len(files) >= 2:
            findings.append(_report.Finding(
                tool="configscan", kind="config-key-multi-read",
                summary=f"{key} read as a literal in {len(files)} files (centralize the read)",
                score=float(len(files)), severity="medium", locations=sorted(locs, key=lambda l: (l.file, l.line)),
            ))

    undoc = sorted(k for k in code_full if k not in example and not in_docs(k) and not is_chain_scoped(k, prefixes))
    if undoc:
        findings.append(_report.Finding(
            tool="configscan", kind="config-undocumented",
            summary=f"{len(undoc)} shared key(s) read in code but absent from docs and .env.example: {', '.join(undoc)}",
            score=float(len(undoc)), severity="medium",
            locations=[l for k in undoc for l in key_locs[k]], metrics={"keys": undoc},
        ))

    undoc_suf = sorted(s for s in code_suffixes if s not in doc_suffixes)
    if undoc_suf:
        findings.append(_report.Finding(
            tool="configscan", kind="config-suffix-undocumented",
            summary=f"{len(undoc_suf)} per-chain suffix(es) built in code but not documented: {', '.join(undoc_suf)}",
            score=float(len(undoc_suf)), severity="low", metrics={"suffixes": undoc_suf},
        ))

    ex_not_doc = sorted(k for k in example if not is_chain_scoped(k, prefixes) and not in_docs(k))
    if ex_not_doc:
        findings.append(_report.Finding(
            tool="configscan", kind="config-doc-drift",
            summary=f"{len(ex_not_doc)} key(s) in .env.example but not in docs/configuration.md: {', '.join(ex_not_doc)}",
            score=float(len(ex_not_doc)), severity="medium", metrics={"keys": ex_not_doc},
        ))

    # Reverse direction: a full key documented in configuration.md but absent from
    # .env.example (a per-chain key covered by an example suffix is not drift). A
    # documented per-chain *template family* (`<PREFIX>_X`) with no example instance
    # is the same drift one level up, so template suffixes are checked too - using
    # the template-only set so a concrete full key already in `doc_not_ex` is not
    # reported a second time as a family.
    doc_not_ex = sorted(
        k for k in doc_full
        if k not in example and (suffix_of(k) is None or suffix_of(k) not in example_suffixes)
    )
    doc_suf_not_ex = sorted(s for s in doc_template_suffixes if s not in example_suffixes)
    drift = doc_not_ex + [f"<PREFIX>{s}" for s in doc_suf_not_ex]
    if drift:
        findings.append(_report.Finding(
            tool="configscan", kind="config-example-drift",
            summary=f"{len(drift)} documented key(s)/family(ies) absent from .env.example: {', '.join(drift)}",
            score=float(len(drift)), severity="medium",
            metrics={"keys": doc_not_ex, "suffix_families": doc_suf_not_ex},
        ))

    # Fourth direction, closing the three-way audit: a full key advertised in BOTH
    # docs and .env.example that code no longer reads is stale operator config. No
    # other predicate catches it - the code-side checks flag only *undocumented*
    # reads, and the two reverse checks compare docs and example only with each
    # other. Requiring presence in both docs and example keeps this high-confidence
    # (a deliberately advertised key, not a stray mention). Per-chain instances are
    # compared as suffix families, not full keys, since code builds them via format!.
    # A key referenced only by tooling (justfile/shell) is still read, not stale, so
    # it is excluded rather than reported.
    tooling = tooling_referenced_keys(repo)
    unread_full = sorted(
        k for k in (doc_full & example)
        if not is_chain_scoped(k, prefixes) and k not in full_code_full and k not in tooling
    )
    unread_suf = sorted((doc_suffixes & example_suffixes) - full_code_suffixes)
    stale = unread_full + [f"<PREFIX>{s}" for s in unread_suf]
    if stale:
        findings.append(_report.Finding(
            tool="configscan", kind="config-unread",
            summary=f"{len(stale)} documented+example key(s)/family(ies) never read in code: {', '.join(stale)}",
            score=float(len(stale)), severity="medium",
            metrics={"keys": unread_full, "suffix_families": unread_suf},
        ))

    return findings


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--docs-dir", default="docs")
    ap.add_argument("--env-example", default=".env.example")
    ap.add_argument("--include-tests", action="store_true")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.docs_dir, args.env_example, args.include_tests)
    if args.json:
        _report.print_json(findings)
        return 0

    for f in findings:
        print(f"[{f.severity}] {f.kind}: {f.summary}")
        for loc in f.locations[:12]:
            tail = f" {loc.name}" if loc.name else ""
            where = f"{loc.file}:{loc.line}" if loc.line else loc.file
            print(f"        {where}{tail}")
    print(f"# {len(findings)} config findings")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
