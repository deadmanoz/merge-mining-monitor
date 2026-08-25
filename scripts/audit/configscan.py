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
import re

import _report
import _scan

# Cargo/build-time env vars are not application configuration; exclude them so
# they do not masquerade as scattered runtime config (they cluster in fixtures).
BUILD_ENV = {"OUT_DIR", "TARGET", "HOST", "PROFILE", "NUM_JOBS", "RUSTC", "RUSTDOC"}


def _is_build_env(key: str) -> bool:
    return key in BUILD_ENV or key.startswith(("CARGO_", "RUST", "LD_", "DYLD_"))


# A full env key: either the usual UNDERSCORE_SEPARATED shape, or a single
# all-caps run of >= 4 chars (e.g. `PGHOST`, `PGPASSWORD`) so underscore-free
# keys are recognized. The >= 4 floor and prefix filter in `doc_tokens` keep short
# words (RPC/SQL/URL) and bare chain prefixes out.
FULLKEY = re.compile(r"^[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)+$|^[A-Z][A-Z0-9]{3,}$")
SUFFIX = re.compile(r"^_[A-Z0-9]+(?:_[A-Z0-9]+)*$")
# A per-chain key built as format!("{prefix}_RPC_USER"): the literal is
# `{...}_SUFFIX`, so the suffix follows the placeholder, not the string start.
FORMAT_SUFFIX = re.compile(r"\{[^}]*\}(_[A-Z0-9]+(?:_[A-Z0-9]+)*)")
# Only actual read calls, not the `std::env` namespace itself (which also prefixes
# `std::env::args()`, `std::env::Args`, ...). `env::var(_os)` also matches the tail
# of `std::env::var(_os)`.
ENV_READ = re.compile(r"\benv::var(?:_os)?\b|\benv_lookup\b")
# The env key passed directly to a read call. Keying off the call site (not any
# ALL_CAPS literal) avoids matching opcode/status constants like OP_RETURN.
KEYCALL = re.compile(
    r'(?:env::var(?:_os)?|std::env::var(?:_os)?|env_lookup|\blookup|getenv)\s*\(\s*&?\s*"([A-Z][A-Z0-9_]{2,})"'
)
# The same read calls, but with a bare identifier argument (a key passed through a
# constant). Paired with CONST_KEY, this recovers keys KEYCALL's literal form misses.
KEYCALL_IDENT = re.compile(
    r'(?:env::var(?:_os)?|std::env::var(?:_os)?|env_lookup|\blookup|getenv)\s*\(\s*&?\s*([A-Z][A-Z0-9_]{2,})\s*\)'
)
CONST_KEY = re.compile(
    r'\b(?:const|static)\s+([A-Z][A-Z0-9_]*)\s*:\s*&\s*(?:\'static\s+)?str\s*=\s*"([A-Z][A-Z0-9_]{2,})"'
)
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


def doc_tokens(docs: str, prefixes: list[str]) -> tuple[set[str], set[str]]:
    """Full keys and per-chain suffixes documented in configuration.md.

    A `<PREFIX>_RPC_URL` template contributes the suffix `_RPC_URL`; a bare
    backticked `PGHOST` contributes a full key. `` `X` / `Y` `` splits are
    handled by scanning each backticked token independently.
    """
    full: set[str] = set()
    suffixes: set[str] = set()
    for tok in re.findall(r"`([^`]+)`", docs):
        tok = tok.strip()
        if tok.startswith("<PREFIX>_") or tok.startswith("<PREFIX>"):
            suf = tok[len("<PREFIX>"):]
            if SUFFIX.match(suf):
                suffixes.add(suf)
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
    return full, suffixes


def is_chain_scoped(key: str, prefixes: list[str]) -> bool:
    return any(key.startswith(p + "_") for p in prefixes)


def collect(root: str, docs_dir: str = "docs", env_example: str = ".env.example", include_tests: bool = False) -> list[_report.Finding]:
    docs = read_text(f"{docs_dir}/configuration.md")
    prefixes = parse_prefixes(docs)

    read_files: dict[str, int] = {}
    struct_locs: list[_report.Loc] = []
    key_locs: dict[str, list[_report.Loc]] = {}
    code_suffixes: set[str] = set()
    # `const KEY_ENV: &str = "SOME_KEY"` declarations, resolved after the scan so a
    # `var_os(KEY_ENV)` call site counts against the real key, not a phantom.
    const_map: dict[str, str] = {}
    ident_calls: list[tuple[str, _report.Loc]] = []

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
        reads = len(ENV_READ.findall(stripped))
        if reads:
            read_files[rel] = reads
        for m in CONFIG_STRUCT.finditer(stripped):
            struct_locs.append(_report.Loc(rel, stripped.count("\n", 0, m.start()) + 1, m.group(1)))
        for m in KEYCALL.finditer(decommented):
            key = m.group(1)
            if not _is_build_env(key):
                key_locs.setdefault(key, []).append(_report.Loc(rel, decommented.count("\n", 0, m.start()) + 1))
        for m in CONST_KEY.finditer(decommented):
            const_map[m.group(1)] = m.group(2)
        for m in KEYCALL_IDENT.finditer(decommented):
            ident_calls.append((m.group(1), _report.Loc(rel, decommented.count("\n", 0, m.start()) + 1)))
        for lit, _off in _scan.iter_string_literals(src):
            if SUFFIX.match(lit):
                code_suffixes.add(lit)
            for fm in FORMAT_SUFFIX.finditer(lit):
                code_suffixes.add(fm.group(1))

    # Resolve constant-passed keys once every declaration has been seen (a const may
    # be defined in a different file from its use).
    for ident, loc in ident_calls:
        key = const_map.get(ident)
        if key and not _is_build_env(key):
            key_locs.setdefault(key, []).append(loc)

    code_full = set(key_locs)
    example = env_keys(read_text(env_example))
    doc_full, doc_suffixes = doc_tokens(docs, prefixes)
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
    # .env.example (a per-chain key covered by an example suffix is not drift).
    doc_not_ex = sorted(
        k for k in doc_full
        if k not in example and (suffix_of(k) is None or suffix_of(k) not in example_suffixes)
    )
    if doc_not_ex:
        findings.append(_report.Finding(
            tool="configscan", kind="config-example-drift",
            summary=f"{len(doc_not_ex)} key(s) in docs/configuration.md but not in .env.example: {', '.join(doc_not_ex)}",
            score=float(len(doc_not_ex)), severity="medium", metrics={"keys": doc_not_ex},
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
