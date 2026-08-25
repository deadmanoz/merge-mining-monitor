"""Shared Rust source-scanning primitives for the audit tools.

Deliberately regex-based and heuristic: this is a lint-adjacent *aid*, not a
compiler. It trades a real parser for zero dependencies (Python stdlib only) and
"good enough to point a human at the right function" precision. Every consumer
treats the output as leads to verify, never as ground truth.

Consolidated here so the function-extraction and identifier-normalization logic
lives in exactly one place (the clone and complexity tools both used to carry
their own copy).
"""

from __future__ import annotations

import os
import re
from dataclasses import dataclass

RUST_KEYWORDS_KEPT = {
    # Control flow + result/option shapes worth preserving through
    # normalization so two functions that differ only in *names* still compare
    # as structurally identical, while genuinely different control flow does not.
    "let", "mut", "if", "else", "match", "for", "in", "while", "loop", "return",
    "break", "continue", "fn", "async", "await", "move", "impl", "struct",
    "enum", "trait", "Ok", "Err", "Some", "None", "self", "Self", "as", "ref",
    "where", "dyn", "use", "pub", "const",
}

# Multi-character punctuation kept as single structural tokens (order matters:
# longest first so "::" is not split into ":" ":").
_PUNCT = [
    "::", "=>", "->", "==", "!=", "<=", ">=", "&&", "||", "..=", "..", "+=",
    "-=", "?", ";", ",", ".", "(", ")", "{", "}", "[", "]", "<", ">", "&", "|",
    "=", "+", "-", "*", "/", "%", "!", ":",
]


@dataclass(frozen=True)
class Function:
    name: str
    path: str  # repo-relative
    line: int  # 1-based line of the `fn` keyword
    body: str  # from the opening brace to the matching close brace, inclusive


def _is_test_file(name: str) -> bool:
    return name in ("tests.rs", "test_fixtures.rs") or name.endswith(("_tests.rs", "_test.rs"))


def iter_rust_files(root: str, skip_tests: bool = True):
    """Yield every `.rs` path under `root`, skipping build output and (by
    default) test code, where repetition is often intentional and readability-
    serving. "Test code" is the `tests/` integration trees plus unit-test files
    (`tests.rs`, `*_tests.rs`, `test_fixtures.rs`). Inline `#[cfg(test)]` modules
    inside production files are not stripped (that needs a real parser); the
    heuristic accepts that residue."""
    for dirpath, dirs, files in os.walk(root):
        dirs.sort()  # deterministic traversal -> stable output across machines
        if "/target/" in dirpath or dirpath.endswith("/target"):
            continue
        if skip_tests and ("/tests/" in dirpath or dirpath.endswith("/tests")):
            continue
        for name in sorted(files):
            if not name.endswith(".rs"):
                continue
            if skip_tests and _is_test_file(name):
                continue
            yield os.path.join(dirpath, name)


_CHAR_LIT = re.compile(r"'(?:\\u\{[0-9a-fA-F]+\}|\\.|[^'\\\n])'")


def strip_noise(src: str) -> str:
    """Remove comments and neutralize string/char literals in one pass.

    A single scanner (not ordered regexes) is required for correctness: a naive
    "strip `//` comments, then strings" pass corrupts any string containing `//`
    (e.g. an `"https://..."` URL), which unbalances quotes and then braces and
    makes `find_functions` capture far too much.

    Guarantees preserved for downstream line-mapping and brace-matching:
      * newline COUNT is preserved at every position (removed comments and
        collapsed multi-line strings keep their `\\n`s), so a line number taken
        on the result equals the line number in the original file;
      * string/char contents collapse to `"s"` / `'c'` so their bytes can never
        masquerade as code (braces, quotes);
      * lifetimes / labels (`'a`) are left as a bare `'` and do not start a char
        literal.

    Handles line comments, block comments, normal strings, and raw strings
    (`r"..."`, `r#"..."#`, ...). Raw/byte-string prefixes are only recognized at
    a token boundary so the `r` in `for`/`ptr` is never mistaken for one.
    """
    out: list[str] = []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        two = src[i : i + 2]
        if two == "//":
            j = src.find("\n", i)
            i = n if j < 0 else j  # keep the newline itself
            continue
        if two == "/*":
            j = src.find("*/", i + 2)
            end = n if j < 0 else j + 2
            out.append("\n" * src.count("\n", i, end))
            i = end
            continue
        # Raw / byte-raw string: (b?r) #* " ... " #*  at a token boundary.
        if c in "rb" and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            k = i
            if src[k] == "b" and k + 1 < n and src[k + 1] == "r":
                k += 1
            if src[k] == "r":
                h = k + 1
                while h < n and src[h] == "#":
                    h += 1
                if h < n and src[h] == '"':
                    closer = '"' + "#" * (h - (k + 1))
                    j = src.find(closer, h + 1)
                    end = n if j < 0 else j + len(closer)
                    out.append('"s"' + "\n" * src.count("\n", i, end))
                    i = end
                    continue
        if c == '"':
            j = i + 1
            while j < n:
                if src[j] == "\\":
                    j += 2
                    continue
                if src[j] == '"':
                    break
                j += 1
            end = min(j + 1, n)
            out.append('"s"' + "\n" * src.count("\n", i, end))
            i = end
            continue
        if c == "'":
            m = _CHAR_LIT.match(src, i)
            if m:
                out.append("'c'")
                i = m.end()
            else:
                out.append("'")  # lifetime / label
                i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def find_functions(src: str, path: str) -> list[Function]:
    """Extract `fn` bodies via brace matching.

    `src` should already be `strip_noise`d so that braces inside strings or
    comments cannot unbalance the counter. A `fn` with no body (trait method
    signature ending in `;`) is skipped.
    """
    out: list[Function] = []
    n = len(src)
    for m in re.finditer(r"\bfn\s+([A-Za-z0-9_]+)", src):
        # Walk to the body's opening brace at paren-depth 0 (skipping the
        # signature's own parens/generics). A `;` at depth 0 means no body.
        j = m.end()
        paren = 0
        while j < n:
            c = src[j]
            if c == "(":
                paren += 1
            elif c == ")":
                paren -= 1
            elif c == "{" and paren <= 0:
                break
            elif c == ";" and paren <= 0:
                j = -1
                break
            j += 1
        if j < 0 or j >= n:
            continue
        # Brace-match the body.
        depth = 0
        k = j
        while k < n:
            c = src[k]
            if c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    break
            k += 1
        out.append(Function(m.group(1), path, src[: m.start()].count("\n") + 1, src[j : k + 1]))
    return out


def tokenize_structural(body: str) -> list[str]:
    """Normalize a function body to a structural token stream.

    Identifiers become `ID`, numbers `NUM`, neutralized literals `LIT`; kept
    keywords and punctuation pass through. Two functions that differ only in
    identifiers therefore produce identical streams.
    """
    toks: list[str] = []
    i, n = 0, len(body)
    while i < n:
        c = body[i]
        if c.isspace():
            i += 1
            continue
        if c.isalnum() or c == "_":
            j = i
            while j < n and (body[j].isalnum() or body[j] == "_"):
                j += 1
            w = body[i:j]
            if w in RUST_KEYWORDS_KEPT:
                toks.append(w)
            elif w in ("s", "c"):
                toks.append("LIT")
            elif w.isdigit():
                toks.append("NUM")
            else:
                toks.append("ID")
            i = j
            continue
        for p in _PUNCT:
            if body.startswith(p, i):
                toks.append(p)
                i += len(p)
                break
        else:
            i += 1
    return toks


def load_functions(root: str, skip_tests: bool = True) -> list[Function]:
    """`find_functions` across the whole tree, on `strip_noise`d sources."""
    funcs: list[Function] = []
    for path in iter_rust_files(root, skip_tests=skip_tests):
        try:
            raw = open(path, encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        funcs.extend(find_functions(strip_noise(raw), rel(path)))
    return funcs


def rel(path: str) -> str:
    """Trim the noisy `crates/` prefix for compact reporting."""
    return path[len("crates/"):] if path.startswith("crates/") else path
