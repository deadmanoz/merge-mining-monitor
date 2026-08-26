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


# A single char literal: a `\xNN` byte escape, a `\u{...}` unicode escape, any other
# `\<c>` escape, or one ordinary char. The `\xNN` form must precede the generic `\.`
# alternative, which would otherwise match only `\x` and leave `NN'` as stray source -
# so `'\x41'` would neutralize to identifiers/numbers instead of one `LIT`, giving a
# function using `'\x41'` a different structural fingerprint from an equivalent `'A'`.
_CHAR_LIT = re.compile(r"'(?:\\x[0-9a-fA-F]{2}|\\u\{[0-9a-fA-F]+\}|\\.|[^'\\\n])'")


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


_USE_LEAF_NAME = re.compile(r"(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s*$")
_USE_ALIAS = re.compile(r"\bas\s+(?:r#)?([A-Za-z_][A-Za-z0-9_]*)\s*$")


def use_bound_names(body: str) -> set[str]:
    """Simple names a `use <body>;` brings into scope, descending nested brace groups.

    `body` is the text between `use ` and the terminating `;` (the root path
    included). For each leaf the bound name is its alias (`x as y` -> `y`) or its
    final path segment (`a::b::C` -> `C`); a `{...}` group fans out into its
    comma-separated items, recursively, so `std::{fmt::{Display}, io::Write}` yields
    `Display` and `Write`. Glob (`*`) and `self` leaves bind no new simple name and
    are skipped. Root-agnostic: a caller that only cares about external imports gates
    on the path root separately.

    Recursion (not just an outer-group split) is required: a single-level split of
    `std::{fmt::{Display}}` leaves the item `fmt::{Display}`, whose trailing `}` hides
    the bound `Display` - so a bare `impl Display` would be misread as naming a local
    trait rather than the imported `std::fmt::Display`.
    """
    names: set[str] = set()

    def walk(seg: str) -> None:
        seg = seg.strip()
        if not seg or seg == "self" or seg.endswith("*"):
            return
        b = seg.find("{")
        if b != -1:
            depth, close = 0, len(seg)
            for k in range(b, len(seg)):
                if seg[k] == "{":
                    depth += 1
                elif seg[k] == "}":
                    depth -= 1
                    if depth == 0:
                        close = k
                        break
            inner = seg[b + 1 : close]
            depth = start = 0
            for k, ch in enumerate(inner):
                if ch == "{":
                    depth += 1
                elif ch == "}":
                    depth -= 1
                elif ch == "," and depth == 0:
                    walk(inner[start:k])
                    start = k + 1
            walk(inner[start:])
            return
        am = _USE_ALIAS.search(seg)  # `path::x as y` -> y
        if am:
            names.add(am.group(1))
            return
        lm = _USE_LEAF_NAME.search(seg)  # `path::C` -> C
        if lm:
            names.add(lm.group(1))

    walk(body)
    return names


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
    where doc comments have collapsed to blank lines and string/char brackets are
    neutralized, so bracket matching below sees only real attribute delimiters.

    Walk: keep the fn's own line prefix (covers a rare inline `#[test] fn foo`),
    then ascend over the preceding attributes by matching each `#[ ... ]` (or inner
    `#![ ... ]`) as a *balanced bracket span* rather than a single line. A one-line
    `#[test]` and a wrapped `#[tokio::test(\n flavor = "multi_thread"\n)]` are then
    both captured whole - the latter's closing `)]` line does not start with `#[`,
    so the old line-prefix heuristic dropped the attribute and reported the test as
    a complexity hotspot. Blank/collapsed-doc lines between attributes are skipped;
    the walk stops at the first non-attribute token.
    """
    line_start = src.rfind("\n", 0, fn_start) + 1
    collected = [src[line_start:fn_start]]  # same-line prefix (inline `#[test] fn`)
    i = line_start
    while i > 0:
        j = i
        while j > 0 and src[j - 1] in " \t\r\n":  # skip blank/collapsed-doc gap
            j -= 1
        if j == 0 or src[j - 1] != "]":
            break  # preceding token is ordinary code, not an attribute close
        depth, k, open_idx = 0, j, None
        while k > 0:  # bracket-match backward from the ']' to its matching '['
            ch = src[k - 1]
            if ch == "]":
                depth += 1
            elif ch == "[":
                depth -= 1
                if depth == 0:
                    open_idx = k - 1
                    break
            k -= 1
        if open_idx is None:
            break
        pre = open_idx - 1 if open_idx > 0 and src[open_idx - 1] == "!" else open_idx
        if pre == 0 or src[pre - 1] != "#":
            break  # a ']' that is not part of a `#[...]` / `#![...]` attribute
        attr_start = pre - 1
        collected.append(src[attr_start:j].strip())
        i = attr_start
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


def _consume_number(body: str, i: int, n: int) -> int:
    """Consume one whole Rust numeric literal starting at `body[i]` (a digit);
    return the index just past it.

    Emitting a single token for the *entire* literal matters for structural
    comparison: a k-gram window that saw `1.0` as `NUM . NUM` but `1e0` as one
    `NUM` would shift out of alignment and hide a real clone. So the full grammar
    is consumed as a unit - base-prefixed integers (`0xFF`, `0o17`, `0b1010`),
    digit-separated and suffixed forms (`1_000`, `255u8`), and floats with a
    fractional part and/or exponent and/or suffix (`1.0`, `1e0`, `3.14e-2f64`).

    A `.` is taken as a decimal point only when a digit follows it, so `1..10`
    (range) stays `NUM .. NUM` and `tuple.0` field access is unaffected; a hex/
    octal/binary literal never grows a fractional part (Rust has no hex floats).
    """
    j = i
    if body[j] == "0" and j + 1 < n and body[j + 1] in "xXoObB":
        j += 2  # base prefix: the digits/letters/`_` run carries no float part
        while j < n and (body[j].isalnum() or body[j] == "_"):
            j += 1
        return j
    while j < n and (body[j].isdigit() or body[j] == "_"):
        j += 1
    if j + 1 < n and body[j] == "." and body[j + 1].isdigit():
        j += 1
        while j < n and (body[j].isdigit() or body[j] == "_"):
            j += 1
    if j < n and body[j] in "eE":
        k = j + 1
        if k < n and body[k] in "+-":
            k += 1
        if k < n and body[k].isdigit():
            j = k
            while j < n and (body[j].isdigit() or body[j] == "_"):
                j += 1
    while j < n and (body[j].isalnum() or body[j] == "_"):
        j += 1  # type suffix directly abutting the literal (`f64`, `i32`, `u8`)
    return j


def tokenize_structural(body: str) -> list[str]:
    """Normalize a function body to a structural token stream.

    Identifiers become `ID`, numbers `NUM`, neutralized literals `LIT`; kept
    keywords and punctuation pass through. Two functions that differ only in
    identifiers therefore produce identical streams.

    `body` is expected `strip_noise`d, so every literal already reads as its
    delimited sentinel - a string/raw string as `"s"` and a char as `'c'`. The
    literal token is recognized by those *quote delimiters*, never by the inner
    letter, so a real variable named `s` or `c` still tokenizes as `ID` (matching
    its renamed twin) instead of collapsing to `LIT` and corrupting the surrounding
    k-grams. A lone `'` is a lifetime/label tick and is skipped.
    """
    toks: list[str] = []
    i, n = 0, len(body)
    while i < n:
        c = body[i]
        if c.isspace():
            i += 1
            continue
        # Neutralized string sentinel (`"s"`): the only `"` in strip_noise'd source
        # opens one. Emit a single `LIT` and skip to the closing quote.
        if c == '"':
            end = body.find('"', i + 1)
            toks.append("LIT")
            i = end + 1 if end != -1 else n
            continue
        # Neutralized char sentinel is exactly `'c'`; a bare `'` is a lifetime tick.
        if c == "'":
            if body[i : i + 3] == "'c'":
                toks.append("LIT")
                i += 3
            else:
                i += 1
            continue
        # A Rust identifier can never start with a digit, so a digit here opens a
        # numeric literal; consume the whole literal grammar (see `_consume_number`)
        # as one `NUM` rather than letting a `.` or signed exponent split it.
        if c.isdigit():
            toks.append("NUM")
            i = _consume_number(body, i, n)
            continue
        if c.isalpha() or c == "_":
            j = i
            while j < n and (body[j].isalnum() or body[j] == "_"):
                j += 1
            w = body[i:j]
            # Raw identifier `r#name`: the `r`, the `#`, and `name` are ONE identifier,
            # not `r` + a dropped `#` + `name`. Consume the `#name` tail and emit a
            # single `ID`, so a raw-identifier call (`r#type()`) yields the same token
            # stream its renamed twin (`value()`) does and a structural clone is not
            # hidden by the extra `ID`. An escaped keyword (`r#match`) is used as a
            # name, so `ID` (not the kept `match` keyword) is the correct token.
            if (w == "r" and j < n and body[j] == "#"
                    and j + 1 < n and (body[j + 1].isalpha() or body[j + 1] == "_")):
                j += 1
                while j < n and (body[j].isalnum() or body[j] == "_"):
                    j += 1
                toks.append("ID")
                i = j
                continue
            toks.append(w if w in RUST_KEYWORDS_KEPT else "ID")
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


def git_root_of(path: str) -> str | None:
    """Absolute work-tree root of the repository that *contains* `path`, or None.

    Unlike `_git_root` (which asks about the process's CWD), this resolves the repo
    from the scanned path via `git -C`, so a detector launched outside the checkout
    with an absolute scan root still finds the repo's `docs/`/`.env.example` sitting
    next to the code instead of reading nothing and reporting every key undocumented.
    Not cached: the answer depends on the argument.
    """
    d = path if os.path.isdir(path) else (os.path.dirname(os.path.abspath(path)) or ".")
    try:
        out = subprocess.run(
            ["git", "-C", d, "rev-parse", "--show-toplevel"],
            capture_output=True, text=True, check=True,
        ).stdout.strip()
        return out or None
    except (OSError, subprocess.CalledProcessError):
        return None


# Work-tree roots already resolved by `rel`, newest first. One scan almost always
# lives under a single root, so this stays a 1-element list and the per-call cost is
# one `relpath`; a second root is only resolved (and cached) on the first path that
# escapes the known ones.
_REL_ROOTS: list[str] = []


def rel(path: str) -> str:
    """Repository-relative path for compact, portable reporting.

    A scan root may be passed as `crates`, `./crates`, or an absolute path; each
    would otherwise leak a different location string (`mmm-api/...` vs `./crates/...`
    vs `/abs/.../crates/...`). Normalizing every path against the git work-tree root
    yields one deterministic form (`crates/mmm-api/src/...`) - the same
    repository-relative shape `coupling.py` already emits from `git` - so the
    aggregate JSON is a single consistent format across clones and invocation styles.

    The work-tree root is resolved from the scanned *path* (`git_root_of`), not the
    process CWD (`_git_root`): a detector launched outside the checkout with an
    absolute scan root must still emit repository-relative locations, or the promised
    CWD-independent JSON contract leaks checkout-specific absolute paths. Outside a
    git tree (or for a path above the root) it falls back to a plain `normpath`, and
    the historical bare `crates/`-stripped form is no longer emitted.
    """
    # realpath (not abspath): `git rev-parse --show-toplevel` reports a symlink-
    # resolved root (e.g. macOS `/private/var/...` for `/var/...`), so an abspath under
    # the unresolved alias would test as "outside" the work tree and leak an absolute
    # path. Resolving here matches git's own form; the plain-normpath fallback below
    # still preserves the caller's path shape when the file is outside any repo.
    ap = os.path.realpath(path)
    # Reuse a root already resolved for an earlier path in this run.
    for root in _REL_ROOTS:
        try:
            r = os.path.relpath(ap, root)
        except ValueError:  # e.g. different drive on Windows
            continue
        if not r.startswith(".."):  # inside this work tree
            return r
    root = git_root_of(ap)
    if root:
        if root not in _REL_ROOTS:
            _REL_ROOTS.append(root)
        try:
            r = os.path.relpath(ap, root)
            if not r.startswith(".."):
                return r
        except ValueError:
            pass
    return os.path.normpath(path)
