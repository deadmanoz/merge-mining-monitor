#!/usr/bin/env python3
"""Find duplicated SQL embedded in Rust string literals.

jscpd tokenizes Rust, so a multi-line SQL string is largely one opaque token and
duplication *inside* it is under-counted. Here we pull string literals that look
like SQL, normalize whitespace and `$N` placeholders, and report exact groups
(same normalized SQL in >= 2 places) and near pairs across different files.

Production and test locations are tagged separately: test probe duplication is
common and lower-value than the same query hand-written across production crates.
Advisory; stdlib-only. Emits the shared finding schema with --json.
"""

from __future__ import annotations

import argparse
import re
from collections import defaultdict
from difflib import SequenceMatcher

import _report
import _scan

SQL_KW = re.compile(r"\b(SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|JOIN|WHERE|VALUES|ON\s+CONFLICT|RETURNING|WITH)\b", re.I)
# A literal that *begins* with a command keyword is a SQL statement even with only
# one recognized clause (`SELECT fn($1, $2)`, `UPDATE t SET ...`), which the
# two-keyword floor below would otherwise discard - dropping real duplicates like
# `SELECT pg_advisory_xact_lock_shared($1, $2)`. Leading whitespace/newlines from a
# raw string are skipped; `^` (no re.M) anchors to the literal's start.
SQL_STMT = re.compile(r"^\s*(SELECT|INSERT\s+INTO|UPDATE|DELETE\s+FROM|WITH)\b", re.I)
# A PostgreSQL dollar-quote opener: `$$` or `$tag$` where the tag is a valid
# identifier (never starting with a digit, so `$1` positional binds don't match).
DOLLAR_OPEN = re.compile(r"\$([A-Za-z_][A-Za-z0-9_]*)?\$")
# Rust string escapes that map to a single character. `iter_string_literals_ex`
# returns a non-raw literal's *source* bytes, so a SQL backslash is spelled `\\` and
# a quote may be written `\'`/`\"`; decoding them here (before SQL quote scanning)
# means a source-level `\\` collapses to one backslash and cannot later be misread as
# escaping an adjacent quote, and `E'ABC\\' DEF'` is compared as the string it truly
# denotes rather than being split at the `\\'`.
_SIMPLE_ESC = {"n": "\n", "r": "\r", "t": "\t", "0": "\0", "\\": "\\", "'": "'", '"': '"'}


def _decode_rust_escapes(s: str) -> str:
    """Decode the escapes in a non-raw Rust string literal's source text to the bytes
    the string actually denotes. Handles the simple/control escapes, `\\xHH`,
    `\\u{...}`, and a `\\`-newline line continuation (the break and the next line's
    leading indentation are dropped). An unknown escape is left verbatim (invalid
    Rust would not compile, so being lenient is harmless)."""
    out: list[str] = []
    i, n = 0, len(s)
    while i < n:
        c = s[i]
        if c != "\\" or i + 1 >= n:
            out.append(c)
            i += 1
            continue
        e = s[i + 1]
        if e in _SIMPLE_ESC:
            out.append(_SIMPLE_ESC[e])
            i += 2
        elif e in "\r\n":  # line continuation: drop the break and following indent
            j = i + 1
            if s[j] == "\r":
                j += 1
            if j < n and s[j] == "\n":
                j += 1
            while j < n and s[j] in " \t":
                j += 1
            i = j
        elif e == "x" and i + 3 < n:
            try:
                out.append(chr(int(s[i + 2 : i + 4], 16)))
                i += 4
            except ValueError:
                out.append(c)
                i += 1
        elif e == "u" and i + 2 < n and s[i + 2] == "{" and (k := s.find("}", i + 3)) != -1:
            try:
                out.append(chr(int(s[i + 3 : k], 16)))
                i = k + 1
            except ValueError:
                out.append(c)
                i += 1
        else:
            out.append(c)  # unknown escape: keep the backslash verbatim
            i += 1
    return "".join(out)
# Whitespace that sits between a word character and an adjacent operator/punctuation
# character (a non-word, non-space char such as `( ) , = < > + - * / | :`) is
# insignificant in SQL and removed, canonicalizing spacing variants like
# `exists (`/`exists(`, `x ,`/`x,`, `id =`/`id=`. Two boundaries are deliberately
# kept: whitespace between two WORD chars is a token separator (`SELECT a` is not
# `SELECTa`), and whitespace between two OPERATOR chars can be semantic - `x - -1`
# (subtract a negative) must not fold into `x --1` (which begins a `--` line
# comment), and `< >` must not fold into `<>` - so operator/operator spacing is
# preserved too. Two directions cover both orders; applied in a cascade because
# closing one gap can expose the next (`( a )` -> `(a)`). Unquoted segments only, so
# string/identifier contents keep their exact spacing.
WORD_THEN_PUNCT_WS = re.compile(r"(\w) +([^\w\s])")
PUNCT_THEN_WORD_WS = re.compile(r"([^\w\s]) +(\w)")
# A positional placeholder (`$1`, `$2`) is an OPERAND, not an operator, so a space
# between an operator/punctuation char and a following `$<digit>` is insignificant -
# `col = $1` and `col=$1` denote the same query and must fold together. `$` is a
# non-word char, so operator/operator spacing (kept above to protect `x - -1`) would
# otherwise preserve the `= $1` gap; this rule closes it. The trailing side (`$1 =`)
# already collapses via WORD_THEN_PUNCT_WS since the digit is a word char.
PUNCT_THEN_PLACEHOLDER_WS = re.compile(r"([^\w\s]) +(\$\d)")


def extract_sql(src: str):
    # Tokenize via the shared scanner: it recognizes raw/byte strings only at a
    # token boundary (so a word ending in `r` before a `"` is never a raw opener),
    # honors matched raw delimiters, and skips comments/char literals - the whole
    # class of quote-desync bugs a hand-rolled regex hits.
    for content, off, is_raw in _scan.iter_string_literals_ex(src):
        # Decode Rust escapes (non-raw only) so the literal is compared as the string
        # it actually denotes - crucially collapsing a source `\\` to one backslash so
        # a following quote is not misread as escaped. A raw string's bytes are already
        # literal, so it is passed through.
        if not is_raw:
            content = _decode_rust_escapes(content)
        # Admit a literal that either starts with a command keyword (an anchored
        # single-clause statement) or carries >= 2 recognized clauses (a fragment
        # like a `JOIN ... WHERE ...` builder piece that does not start a statement).
        if len(content) >= 40 and (SQL_STMT.match(content) or len(SQL_KW.findall(content)) >= 2):
            yield src[:off].count("\n") + 1, content


def norm(s: str) -> str:
    """Normalize SQL for comparison, but ONLY outside quoted content.

    Keyword case and structural whitespace are noise (`SELECT` == `select`), so
    they are folded - but a single-quoted string literal (`'ABC'`) or a
    double-quoted identifier (`"Col"`) is case- and whitespace-significant in
    PostgreSQL (`code = 'ABC'` and `code = 'abc'` can return different rows), so its
    bytes are preserved verbatim. SQL escapes a quote by doubling it (`''`), which
    is handled here. A PostgreSQL escape string (`E'...'`) additionally escapes with
    a backslash, so `E'ABC\\' DEF'` is one literal whose `\\'` is an embedded quote;
    that prefix is detected so the span is not split at the escaped quote (which
    would leak ` DEF` as unquoted SQL and reopen a phantom string on the trailing
    quote). (A quote written as a Rust `\\"` escape inside a non-raw string literal is
    a rare edge this heuristic does not unescape.)

    A PostgreSQL dollar-quoted span (`$$...$$` or `$tag$...$tag$`) is likewise
    case- and whitespace-significant (it commonly carries a function body or a
    literal), so it is copied verbatim too. Its tag cannot start with a digit, so a
    positional placeholder like `$1` is never mistaken for a dollar-quote opener.

    Positional placeholders (`$1`, `$2`, ...) are kept verbatim - neither collapsed
    to one token nor renumbered - so both the reuse pattern and the bind order carry
    semantics. Renumbering by first occurrence would fold `a = $1 AND b = $2` and
    `a = $2 AND b = $1` (different results from one argument list) into one query;
    leaving the original numbers keeps those permutations distinct, and still keeps
    a reused bind (`a = $1 OR b = $1`) distinct from two independent ones
    (`a = $1 OR b = $2`). Only the whitespace *around* a placeholder is normalized
    (`col = $1` == `col=$1`), since a placeholder is an operand, not an operator.
    """
    out: list[str] = []
    i, n = 0, len(s)
    while i < n:
        c = s[i]
        if c in "'\"":
            q = c
            # A PostgreSQL escape-string literal `E'...'` (single-quoted only) uses C
            # backslash escapes, so `\'` is an embedded quote, not a terminator. The
            # `E`/`e` prefix was already emitted (lowercased) into the previous
            # segment, so detect it by looking back one char at a standalone word
            # boundary. Standard-conforming strings (no `E`) treat backslash
            # literally, so escapes are only honored for a detected E-string.
            estr = (
                q == "'"
                and i > 0
                and s[i - 1] in "Ee"
                and (i == 1 or not (s[i - 2].isalnum() or s[i - 2] == "_"))
            )
            j = i + 1
            buf = [c]
            while j < n:
                if estr and s[j] == "\\" and j + 1 < n:  # escape: keep both chars quoted
                    buf.append(s[j : j + 2])
                    j += 2
                    continue
                if s[j] == q:
                    if j + 1 < n and s[j + 1] == q:  # doubled-quote escape stays quoted
                        buf.append(q + q)
                        j += 2
                        continue
                    buf.append(q)
                    j += 1
                    break
                buf.append(s[j])
                j += 1
            out.append("".join(buf))  # quoted span: preserved verbatim
            i = j
            continue
        if c == "$":
            mo = DOLLAR_OPEN.match(s, i)
            if mo:
                closer = mo.group(0)  # "$$" or "$tag$"
                end = s.find(closer, mo.end())
                stop = n if end < 0 else end + len(closer)  # unterminated -> rest verbatim
                out.append(s[i:stop])  # dollar-quoted span: preserved verbatim
                i = stop
                continue
        # SQL comments carry no query meaning, but WHERE a comment ends does: `--`
        # runs to the newline, `/* */` to its closer, so two queries whose comment
        # ends at different points denote different SQL (`SELECT 1 -- x\n + 2` vs
        # `SELECT 1 -- x + 2\n`). Strip the comment to its terminator and emit one
        # space, so neighboring tokens neither fuse nor lose the boundary. `--`/`/*`
        # start a comment only here (outside the quoted and dollar-quoted spans peeled
        # off above), and a lone `-` operator (`x - 1`) is untouched.
        if c == "-" and i + 1 < n and s[i + 1] == "-":
            nl = s.find("\n", i)
            i = n if nl < 0 else nl  # keep the newline; it collapses to a separator
            out.append(" ")
            continue
        if c == "/" and i + 1 < n and s[i + 1] == "*":
            # PostgreSQL block comments NEST: `/* a /* b */ c */` is one comment, so
            # stopping at the first `*/` would leak ` c */` back as unquoted SQL (and
            # reopen a phantom on the stray `*`). Track depth to consume the whole,
            # possibly nested, comment.
            depth = 1
            j = i + 2
            while j < n and depth:
                if s[j] == "/" and j + 1 < n and s[j + 1] == "*":
                    depth += 1
                    j += 2
                elif s[j] == "*" and j + 1 < n and s[j + 1] == "/":
                    depth -= 1
                    j += 2
                else:
                    j += 1
            i = j  # unterminated -> consumed to end
            out.append(" ")
            continue
        j = i
        while j < n:
            if s[j] in "'\"":
                break
            if s[j] == "$" and DOLLAR_OPEN.match(s, j):
                break
            if s[j] == "-" and j + 1 < n and s[j + 1] == "-":
                break  # start of a line comment (handled at top of loop)
            if s[j] == "/" and j + 1 < n and s[j + 1] == "*":
                break  # start of a block comment (handled at top of loop)
            j += 1
        # Placeholders (`$1`, `$2`) are left as-is; keyword case is folded and any
        # whitespace run collapses to a single space. Whitespace separating a word
        # char from an adjacent operator/punctuation char is then dropped (spacing
        # there is insignificant), while word/word and operator/operator spacing is
        # preserved so distinct SQL never collapses together. `$` here is a positional
        # bind (dollar-quote openers were peeled off above), so none can leak in.
        seg = re.sub(r"\s+", " ", s[i:j]).lower()
        prev = ""
        while prev != seg:  # cascade: removing one gap can expose an adjacent one
            prev = seg
            seg = WORD_THEN_PUNCT_WS.sub(r"\1\2", seg)
            seg = PUNCT_THEN_WORD_WS.sub(r"\1\2", seg)
            seg = PUNCT_THEN_PLACEHOLDER_WS.sub(r"\1\2", seg)
        out.append(seg)
        i = j
    return "".join(out).strip()


def is_test(path: str) -> bool:
    # Delegate the filename check to the shared predicate so the two stay in sync;
    # notably it also recognizes the singular `_test.rs`, which a local suffix list
    # missed - misclassifying such SQL as production and over-grading duplicates.
    return "/tests/" in path or _scan._is_test_file(path.rsplit("/", 1)[-1])


def _pick_cross(locs_i, locs_j):
    """A representative pair from *different* files (preferring prod/prod), so the
    two reported locations actually demonstrate the cross-file duplication."""
    li_s, lj_s = sorted(locs_i), sorted(locs_j)
    fallback = None
    for a in li_s:
        for b in lj_s:
            if a[0] != b[0]:
                if not is_test(a[0]) and not is_test(b[0]):
                    return a, b
                if fallback is None:
                    fallback = (a, b)
    return fallback if fallback else (li_s[0], lj_s[0])


def collect(root: str, near: float = 0.85) -> list[_report.Finding]:
    items = []  # (path, line, normalized)
    for path in _scan.iter_rust_files(root, skip_tests=False):
        try:
            src = open(path, encoding="utf-8", errors="ignore").read()
        except OSError:
            continue
        for line, s in extract_sql(src):
            items.append((_scan.rel(path), line, norm(s)))

    findings: list[_report.Finding] = []

    exact = defaultdict(list)
    for path, line, n in items:
        exact[n].append((path, line))
    for n, locs in sorted(exact.items(), key=lambda x: -len(x[1])):
        if len(locs) < 2:
            continue
        prod = [l for l in locs if not is_test(l[0])]
        prod_files = {p for p, _ in prod}
        sev = "high" if len(prod_files) >= 2 else ("medium" if prod_files else "low")
        findings.append(_report.Finding(
            tool="sqldup", kind="sql-exact-dup",
            summary=f"{len(locs)}x identical SQL ({len(prod_files)} prod file(s)): {n[:90]}",
            score=float(len(locs)), severity=sev,
            locations=[_report.Loc(p, line) for p, line in locs],
            metrics={"count": len(locs), "prod_files": sorted(prod_files), "sql": n[:400]},
        ))

    # Keep every location per normalized query (an exact query in N files keeps
    # all N), so exact duplication can never hide a valid cross-file near-dup and
    # the prod/test classification sees all sites, not an arbitrary last one.
    groups: dict[str, list[tuple[str, int]]] = defaultdict(list)
    for path, line, n in items:
        groups[n].append((path, line))
    norms = sorted(groups, key=len)
    # Longest admissible length ratio for a pair that can still reach `near`: with
    # the shorter string fully matched, ratio = 2*La/(La+Lb), so Lb/La <=
    # (2-near)/near. Deriving it from the threshold (not a fixed 1.3) stops the
    # length prune from discarding pairs that would clear a lower `near`. `near == 0`
    # (every similarity level) has no length bound, so skip the prune rather than
    # dividing by zero.
    max_len_ratio = (2.0 - near) / near if near > 0 else float("inf")
    sm = SequenceMatcher(None, autojunk=False)
    for i in range(len(norms)):
        ni = norms[i]
        files_i = {p for p, _ in groups[ni]}
        sm.set_seq2(ni)
        for j in range(i + 1, len(norms)):
            nj = norms[j]
            if len(nj) > len(ni) * max_len_ratio:
                break
            files_j = {p for p, _ in groups[nj]}
            if len(files_i | files_j) == 1:
                continue  # both queries confined to the same single file
            sm.set_seq1(nj)
            if sm.real_quick_ratio() < near or sm.quick_ratio() < near:
                continue
            r = sm.ratio()
            # Report every distinct pair at or above `near`; exclude only exact
            # equality (r == 1.0), which the exact-group pass already owns. A fixed
            # 0.999 upper cutoff wrongly dropped the strongest near-dups - two ~2000
            # char queries differing by one character score ~0.9995 yet are not an
            # exact group (distinct normalized keys can never reach 1.0 here).
            if near <= r < 1.0:
                li, lj = _pick_cross(groups[ni], groups[nj])
                both_prod = not is_test(li[0]) and not is_test(lj[0])
                findings.append(_report.Finding(
                    tool="sqldup", kind="sql-near-dup",
                    summary=f"{r:.2f} similar SQL across files ({'prod' if both_prod else 'test'})",
                    score=round(r, 4), severity="medium" if both_prod else "low",
                    locations=[_report.Loc(li[0], li[1]), _report.Loc(lj[0], lj[1])],
                    metrics={"ratio": round(r, 4), "both_prod": both_prod},
                ))
    findings.sort(key=lambda f: f.sort_key(), reverse=True)
    return findings


def _near_arg(v: str) -> float:
    """A similarity floor in [0, 1]. `0` requests every level (no length pruning); a
    value outside the range is a usage error caught here, not a later traceback."""
    try:
        f = float(v)
    except ValueError:
        raise argparse.ArgumentTypeError(f"not a number: {v!r}")
    if not (0.0 <= f <= 1.0):
        raise argparse.ArgumentTypeError(f"must be within [0, 1], got {f}")
    return f


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("root", nargs="?", default="crates")
    ap.add_argument("--near", type=_near_arg, default=0.85, help="near-duplicate similarity floor in [0,1] (default: 0.85)")
    ap.add_argument("--limit", type=int, default=25, help="max near-duplicate pairs (default: 25)")
    ap.add_argument("--json", action="store_true", help="emit the shared finding schema as JSON")
    args = ap.parse_args()

    findings = collect(args.root, args.near)
    if args.json:
        _report.print_json(findings)
        return 0

    exact = [f for f in findings if f.kind == "sql-exact-dup"]
    near = [f for f in findings if f.kind == "sql-near-dup"]
    print("=== EXACT (whitespace/param-normalized) SQL duplicates ===")
    for f in exact:
        tag = "PROD" if f.metrics["prod_files"] else "test"
        print(f"x{f.metrics['count']:<3d} {tag}  {f.metrics['sql'][:110]}")
        for loc in (f.locations if not f.metrics["prod_files"] else [l for l in f.locations if not is_test(l.file)])[:6]:
            print(f"        {loc.file}:{loc.line}")
    print(f"# {len(exact)} exact-duplicate SQL groups\n")
    print(f"=== NEAR-duplicate SQL (>= {args.near}, different files) ===")
    for f in near[: args.limit]:
        a, b = f.locations
        tag = "PROD" if f.metrics["both_prod"] else "test"
        print(f"{f.score:.2f} {tag}  {a.file}:{a.line}  <=>  {b.file}:{b.line}")
    print(f"# {len(near)} near-duplicate cross-file pairs")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
