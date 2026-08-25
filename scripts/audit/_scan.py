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
import subprocess
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

# Multi-character punctuation kept as single structural tokens. Order matters: the
# tokenizer takes the first `startswith` hit, so every operator must precede any
# shorter operator that is its prefix (`<<=` before `<<` before `<`). The full Rust
# operator set is retained - notably `^`/`^=` and the shift/compound-assign forms -
# so two functions differing only in an operator (`x ^= y` vs `x = y`) do not collapse
# to the same structural token stream and get promoted to a false clone.
_PUNCT = [
    # 3-char
    "<<=", ">>=", "..=",
    # 2-char
    "::", "=>", "->", "==", "!=", "<=", ">=", "&&", "||", "..",
    "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=", "<<", ">>",
    # 1-char
    "?", ";", ",", ".", "(", ")", "{", "}", "[", "]", "<", ">",
    "&", "|", "^", "=", "+", "-", "*", "/", "%", "!", ":", "@",
]


@dataclass(frozen=True)
class Function:
    name: str
    path: str  # repo-relative
    line: int  # 1-based line of the `fn` keyword
    body: str  # from the opening brace to the matching close brace, inclusive
    attrs: str = ""  # outer attributes/qualifiers preceding `fn` (e.g. `#[tokio::test]`)


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
        # Prune heavy/irrelevant subtrees in place so os.walk never descends into
        # them (a populated `target/` can be hundreds of thousands of entries).
        dirs[:] = [d for d in dirs if d != "target" and not (skip_tests and d == "tests")]
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


def _raw_string_span(src: str, i: int):
    """Recognize a raw string literal opening at `src[i]` on a token boundary.

    Handles every raw prefix - plain `r`, byte-raw `br`, and raw C-string `cr` - each
    with any number of `#` hashes: `(b|c)?r #* " ... " #*`. Returns
    `(content_start, content_end, literal_end)`, where content is the inner text and
    `literal_end` is just past the closing delimiter (or `len(src)` if unterminated),
    or `None` when no raw string opens here.

    Centralized so the three scanners (`strip_noise`, `strip_comments`,
    `iter_string_literals_ex`) admit exactly the same openers. Missing `cr"..."`
    previously left a raw C string's inner `"` to terminate a phantom normal string,
    leaking its braces into brace matching and truncating the enclosing function.
    """
    n = len(src)
    if src[i] not in "rbc" or (i and (src[i - 1].isalnum() or src[i - 1] == "_")):
        return None
    k = i
    if src[k] in "bc" and k + 1 < n and src[k + 1] == "r":
        k += 1  # br (byte-raw) or cr (raw C string)
    if src[k] != "r":
        return None
    h = k + 1
    while h < n and src[h] == "#":
        h += 1
    if h >= n or src[h] != '"':
        return None
    closer = '"' + "#" * (h - (k + 1))
    j = src.find(closer, h + 1)
    content_end = n if j < 0 else j
    literal_end = n if j < 0 else j + len(closer)
    return h + 1, content_end, literal_end


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
            # Rust block comments nest: `/* /* */ */` is one comment. Track depth
            # so an inner terminator does not leak the remainder back as source.
            depth, j = 1, i + 2
            while j < n and depth > 0:
                pair = src[j : j + 2]
                if pair == "/*":
                    depth, j = depth + 1, j + 2
                elif pair == "*/":
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            end = j if depth == 0 else n
            out.append("\n" * src.count("\n", i, end))
            i = end
            continue
        # Raw / byte-raw / raw-C string: (b|c)?r #* " ... " #*  at a token boundary.
        if c in "rbc":
            span = _raw_string_span(src, i)
            if span is not None:
                end = span[2]
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


def strip_comments(src: str) -> str:
    """Remove line/block comments while preserving string and char literals
    verbatim.

    Unlike `strip_noise` (which also collapses string *contents* to `"s"`), this
    keeps literal text intact, so a caller can still read an embedded key such as
    `env::var("PGHOST")` while a commented-out `// env::var("PHANTOM")` is dropped.
    Newline counts are preserved (block comments collapse to their `\\n`s), so a
    match offset on the result still maps to the original line. Raw/byte-string
    prefixes are recognized only at a token boundary, mirroring `strip_noise`.
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
            depth, j = 1, i + 2
            while j < n and depth > 0:
                pair = src[j : j + 2]
                if pair == "/*":
                    depth, j = depth + 1, j + 2
                elif pair == "*/":
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            end = j if depth == 0 else n
            out.append("\n" * src.count("\n", i, end))
            i = end
            continue
        if c in "rbc":
            span = _raw_string_span(src, i)
            if span is not None:
                end = span[2]
                out.append(src[i:end])  # raw string kept verbatim
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
            out.append(src[i:end])  # string kept verbatim (contents preserved)
            i = end
            continue
        if c == "'":
            m = _CHAR_LIT.match(src, i)
            if m:
                out.append(src[i : m.end()])
                i = m.end()
            else:
                out.append("'")  # lifetime / label
                i += 1
            continue
        out.append(c)
        i += 1
    return "".join(out)


def iter_string_literals_ex(src: str):
    """Yield `(content, start_index, is_raw)` for each Rust string literal (normal
    and raw), skipping line/block comments and char literals.

    A naive `"..."` regex on raw source pairs quotes across a `'"'` char literal
    or a `"` inside a comment and swallows whole spans of code; this single-pass
    scanner (mirroring `strip_noise`) tokenizes correctly. Content is the literal's
    inner text verbatim, so callers can inspect embedded keys/placeholders that
    `strip_noise` would have collapsed to `"s"`.

    `is_raw` distinguishes raw/byte-raw literals (`r"..."`, `br#"..."#`) - where a
    backslash is an ordinary character - from normal literals, where a backslash
    introduces an escape (so a caller can decode escapes only where they apply).
    """
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        two = src[i : i + 2]
        if two == "//":
            j = src.find("\n", i)
            i = n if j < 0 else j
            continue
        if two == "/*":
            depth, j = 1, i + 2
            while j < n and depth > 0:
                pair = src[j : j + 2]
                if pair == "/*":
                    depth, j = depth + 1, j + 2
                elif pair == "*/":
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            i = j if depth == 0 else n
            continue
        if c in "rbc":
            span = _raw_string_span(src, i)
            if span is not None:
                content_start, content_end, literal_end = span
                yield src[content_start:content_end], i, True
                i = literal_end
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
            yield src[i + 1 : j], i, False
            i = min(j + 1, n)
            continue
        if c == "'":
            m = _CHAR_LIT.match(src, i)
            i = m.end() if m else i + 1
            continue
        i += 1


def iter_string_literals(src: str):
    """`(content, start_index)` for each string literal (raw-ness dropped).

    A thin projection of `iter_string_literals_ex` for callers that do not care
    whether the literal was raw."""
    for content, start, _is_raw in iter_string_literals_ex(src):
        yield content, start


def find_signature_end(src: str, start: int) -> tuple[str, int]:
    """Classify a `fn` signature starting at `start` (just past `fn <name>`).

    Returns `("body", idx)` when the signature ends at the body's opening `{`,
    `("decl", idx)` when it ends at a top-level `;` (a bodyless declaration, e.g. a
    required trait method), or `("eof", last)` if neither is found.

    Depth is tracked for four bracket kinds because each can carry a character that
    would otherwise be misread as the terminator:
      * `( )` params;
      * `[ ]` a fixed-size array type (`-> Result<[u8; N]>`) whose `;` is not a
        bodyless-declaration terminator;
      * `< >` generics, so a const-generic brace argument is seen as nested;
      * `{ }` a const-generic / const expression (`-> Foo<{ N + 1 }>`) whose brace
        is NOT the body brace.
    `->` is consumed as a unit so its `>` never closes a generic; angle tracking is
    suspended inside a const-expression brace, where `<`/`>` are shift/compare
    operators, not generics. `src` should be `strip_noise`d so braces/semicolons in
    strings or comments cannot unbalance the walk. Shared by `find_functions` and
    the trait-surface detector so both classify signatures identically.
    """
    n = len(src)
    j = start
    paren = bracket = angle = brace = 0
    while j < n:
        c = src[j]
        if c == "-" and src[j : j + 2] == "->":
            j += 2
            continue
        if c == "(":
            paren += 1
        elif c == ")":
            paren -= 1
        elif c == "[":
            bracket += 1
        elif c == "]":
            bracket -= 1
        elif c == "<" and brace == 0:
            angle += 1
        elif c == ">" and brace == 0:
            if angle > 0:
                angle -= 1
        elif c == "{":
            if paren <= 0 and bracket <= 0 and angle <= 0 and brace == 0:
                return "body", j
            brace += 1  # a const-generic / const-expression brace
        elif c == "}":
            if brace > 0:
                brace -= 1
        elif c == ";" and paren <= 0 and bracket <= 0 and angle <= 0 and brace == 0:
            return "decl", j
        j += 1
    return "eof", n - 1


def _leading_attrs(src: str, fn_start: int) -> str:
    """Outer attributes (and same-line qualifiers) that precede the `fn` at
    `fn_start`, joined by newlines.

    A `Function.body` starts at the opening `{`, so an attribute written above the
    signature - `#[test]`, `#[tokio::test]` - is never in the body. Any classifier
    that must react to such an attribute (e.g. excluding attributed tests from a
    complexity report) needs this region instead. `src` is expected `strip_noise`d,
    where doc comments have collapsed to blank lines and attribute tokens survive,
    so the backward walk collects only real outer attributes.

    Walk: take the fn's own line prefix (covers a rare inline `#[test] fn foo`),
    then ascend over contiguous lines that are blank or start with `#[`, stopping at
    the first line of ordinary code. Multi-line attributes are only partially
    captured, which the single-line test attributes this serves do not need.
    """
    line_start = src.rfind("\n", 0, fn_start) + 1
    collected = [src[line_start:fn_start]]
    p = line_start
    while p > 0:
        prev_end = p - 1  # the '\n' ending the previous physical line
        prev_start = src.rfind("\n", 0, prev_end) + 1
        line = src[prev_start:prev_end].strip()
        if line == "" or line.startswith("#["):
            if line:
                collected.append(line)
            p = prev_start
        else:
            break
    return "\n".join(collected)


def find_functions(src: str, path: str) -> list[Function]:
    """Extract `fn` bodies via brace matching.

    `src` should already be `strip_noise`d so that braces inside strings or
    comments cannot unbalance the counter. A `fn` with no body (trait method
    signature ending in `;`) is skipped.

    The optional `r#` raw-identifier prefix is consumed but not captured, so
    `fn r#match` yields the logical name `match` rather than a truncated `r`. Each
    function also carries its leading attribute region (see `_leading_attrs`).
    """
    out: list[Function] = []
    n = len(src)
    for m in re.finditer(r"\bfn\s+(?:r#)?([A-Za-z0-9_]+)", src):
        kind, j = find_signature_end(src, m.end())
        if kind != "body" or j >= n:
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
        out.append(Function(
            m.group(1), path, src[: m.start()].count("\n") + 1, src[j : k + 1],
            _leading_attrs(src, m.start()),
        ))
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


_GIT_ROOT_CACHE: list = []  # [resolved_root_or_None]; one-shot cache


def _git_root() -> str | None:
    """Absolute path of the enclosing git work tree, or None outside one."""
    if not _GIT_ROOT_CACHE:
        try:
            out = subprocess.run(
                ["git", "rev-parse", "--show-toplevel"],
                capture_output=True, text=True, check=True,
            ).stdout.strip()
            _GIT_ROOT_CACHE.append(out or None)
        except (OSError, subprocess.CalledProcessError):
            _GIT_ROOT_CACHE.append(None)
    return _GIT_ROOT_CACHE[0]


def rel(path: str) -> str:
    """Repository-relative path for compact, portable reporting.

    A scan root may be passed as `crates`, `./crates`, or an absolute path; each
    would otherwise leak a different location string (`mmm-api/...` vs `./crates/...`
    vs `/abs/.../crates/...`). Normalizing every path against the git work-tree root
    yields one deterministic form (`crates/mmm-api/src/...`) - the same
    repository-relative shape `coupling.py` already emits from `git` - so the
    aggregate JSON is a single consistent format across clones and invocation styles.
    Outside a git tree (or for a path above the root) it falls back to a plain
    `normpath`, and the historical bare `crates/`-stripped form is no longer emitted.
    """
    root = _git_root()
    if root:
        try:
            r = os.path.relpath(os.path.abspath(path), root)
            if not r.startswith(".."):  # inside the work tree
                return r
        except ValueError:  # e.g. different drive on Windows
            pass
    return os.path.normpath(path)
