#!/usr/bin/env bash
# scripts/arch-lint.sh - mechanical architecture gates.
#
# Gate 1: file-size budget - no src/**/*.rs over 1000 lines, no tests/**/*.rs
#         over 2000 lines, and no hand-authored www JS/CSS/HTML over their
#         frontend budgets (plain find/git-ls-files + wc, offenders printed).
# Gate 2: duplication - jscpd over src/ + tests/ (Rust, .jscpd.json) and over
#         www/js (JavaScript, .jscpd.www.json), each with its own threshold.
# Gate 3: attribution-replay clone tripwire - a grep guard against the per-chain
#         Existing*Attribution family regrowing (jscpd misses copy-with-rename).
#
# The structural clippy lints (too_many_lines, self_named_module_files) are
# denied workspace-wide via [workspace.lints.clippy] in Cargo.toml, so every
# clippy run enforces them; this script needs no Rust toolchain and adds the
# size + duplication gates. It runs inside `just lint` (and stays
# independently invokable as `just arch-lint`). There is no grandfathering
# and there will be none: red means refactor, never allowlist.

set -u

ROOT="${ARCH_LINT_ROOT:-$(cd "$(dirname "$0")/.." && pwd)}"
cd "$ROOT"

MAX_SRC_LINES="${ARCH_LINT_MAX_SRC_LINES:-1000}"
MAX_TEST_LINES="${ARCH_LINT_MAX_TEST_LINES:-2000}"
MAX_WWW_JS_LINES="${ARCH_LINT_MAX_WWW_JS_LINES:-750}"
MAX_WWW_CSS_LINES="${ARCH_LINT_MAX_WWW_CSS_LINES:-400}"
MAX_WWW_HTML_LINES="${ARCH_LINT_MAX_WWW_HTML_LINES:-450}"
JSCPD_CONFIG="${ARCH_LINT_JSCPD_CONFIG:-$ROOT/.jscpd.json}"
JSCPD_WWW_CONFIG="${ARCH_LINT_JSCPD_WWW_CONFIG:-$ROOT/.jscpd.www.json}"

fail=0

# Workspace-aware path sets, built from directories that EXIST: the root
# src/ and tests/ trees (present until the workspace finale empties them)
# plus every crates/*/src and crates/*/tests. Literal paths would make find
# and jscpd error once the root trees are gone - and silently skip crate
# sources before that.
src_paths=()
test_paths=()
[ -d src ] && src_paths+=(src)
[ -d tests ] && test_paths+=(tests)
for d in crates/*/src; do [ -d "$d" ] && src_paths+=("$d"); done
for d in crates/*/tests; do [ -d "$d" ] && test_paths+=("$d"); done

echo "== arch-lint: file-size gate (src <= ${MAX_SRC_LINES} lines, tests <= ${MAX_TEST_LINES} lines) =="
size_violations="$(
    {
        find "${src_paths[@]}" -type f -name '*.rs' -print0 | xargs -0 wc -l |
            awk -v max="$MAX_SRC_LINES" '$2 != "total" && $1 > max { printf "  %6d  %s\n", $1, $2 }'
        find "${test_paths[@]}" -type f -name '*.rs' -print0 | xargs -0 wc -l |
            awk -v max="$MAX_TEST_LINES" '$2 != "total" && $1 > max { printf "  %6d  %s\n", $1, $2 }'
    } | sort -rn
)"
if [ -n "$size_violations" ]; then
    echo "FAIL: files over the line budget:"
    printf '%s\n' "$size_violations"
    fail=1
else
    echo "OK: no oversized files."
fi

echo
echo "== arch-lint: frontend file-size gate (www JS <= ${MAX_WWW_JS_LINES}, CSS <= ${MAX_WWW_CSS_LINES}, HTML <= ${MAX_WWW_HTML_LINES} lines) =="
frontend_size_violations_file="$(mktemp "${TMPDIR:-/tmp}/mmm-arch-lint.XXXXXX")" || exit 1
while IFS= read -r -d '' file; do
    if [ ! -e "$file" ]; then
        continue
    fi
    case "$file" in
        www/vendor/*|www/js/source-registry.generated.js)
            continue
            ;;
    esac
    case "$file" in
        *.js)
            max="$MAX_WWW_JS_LINES"
            kind="js"
            ;;
        *.css)
            max="$MAX_WWW_CSS_LINES"
            kind="css"
            ;;
        *.html)
            max="$MAX_WWW_HTML_LINES"
            kind="html"
            ;;
        *)
            continue
            ;;
    esac
    lines="$(wc -l < "$file" | tr -d '[:space:]')"
    if [ "$lines" -gt "$max" ]; then
        printf "  %6d  %-4s max=%s  %s\n" "$lines" "$kind" "$max" "$file"
    fi
done < <(git ls-files -z -- www) > "$frontend_size_violations_file"
frontend_size_violations="$(cat "$frontend_size_violations_file")"
rm -f "$frontend_size_violations_file"
if [ -n "$frontend_size_violations" ]; then
    echo "FAIL: frontend files over the line budget:"
    printf '%s\n' "$frontend_size_violations" | sort -rn
    fail=1
else
    echo "OK: no oversized frontend files."
fi

echo
echo "== arch-lint: duplication gate (jscpd, threshold from .jscpd.json) =="
if npx --yes jscpd --config "$JSCPD_CONFIG" "${src_paths[@]}" "${test_paths[@]}"; then
    echo "OK: duplication under threshold."
else
    echo "FAIL: duplication over threshold (clones listed above)."
    fail=1
fi

echo
echo "== arch-lint: frontend duplication gate (jscpd, threshold from .jscpd.www.json) =="
if [ ! -d www/js ]; then
    echo "OK: no www/js tree."
elif npx --yes jscpd --config "$JSCPD_WWW_CONFIG" www/js; then
    echo "OK: frontend duplication under threshold."
else
    echo "FAIL: frontend duplication over threshold (clones listed above)."
    fail=1
fi

# Copy-with-rename tripwire for the attribution/reward-replay family. jscpd is
# token-based and cannot see a clone once its identifiers are renamed, which is
# exactly how this family regrew before it was seamed. New chain reward/identity
# replay MUST extend the shared seam
# (mmm_capture::attribution_policy::ExistingAttributionSet + WritePolicy), never
# add another per-chain `Existing*Attribution` struct. Only the two single-row
# structs (reclassify_pools, rsk_miner_identities) are legitimate; a third is a
# regression. Raise the budget only by removing a struct, never to admit a clone.
echo
echo "== arch-lint: attribution-replay clone tripwire =="
EXISTING_ATTR_BUDGET="${ARCH_LINT_EXISTING_ATTR_MAX:-2}"
existing_attr_count="$(
    rg -c --no-filename 'struct Existing\w*Attribution\b' "${src_paths[@]}" 2>/dev/null |
        awk '{ sum += $1 } END { print sum + 0 }'
)"
if [ "$existing_attr_count" -gt "$EXISTING_ATTR_BUDGET" ]; then
    echo "FAIL: ${existing_attr_count} Existing*Attribution structs (budget ${EXISTING_ATTR_BUDGET}):"
    rg -n 'struct Existing\w*Attribution\b' "${src_paths[@]}"
    echo "Extend mmm_capture::attribution_policy::ExistingAttributionSet; do not clone a per-chain struct."
    fail=1
else
    echo "OK: attribution-replay family within budget (${existing_attr_count}/${EXISTING_ATTR_BUDGET})."
fi

echo
if [ "$fail" -ne 0 ]; then
    echo "arch-lint: RED."
    echo "Fix by refactoring the offender, never by raising a threshold or allowlisting."
else
    echo "arch-lint: GREEN."
fi
exit "$fail"
