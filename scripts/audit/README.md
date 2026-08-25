# Consolidation audit scripts

Supplementary, **advisory** analyses for spotting consolidation / deduplication /
simplification opportunities. They complement the committed token-literal
duplication gate (`.jscpd.json`, run from `scripts/arch-lint.sh`).

jscpd only matches tokens *verbatim*, so it is blind to:

- renamed ("Type-3") clones - `fetch_error_blocks` vs `fetch_stale_blocks`;
- SQL duplicated *inside* string literals (one opaque token to jscpd);
- structure that only shows up in **git history** (co-changing modules).

These scripts cover those blind spots. They are **heuristic** (regex scanning,
not a real Rust parser) and are **not** wired into any `just` gate. Treat the
output as leads to verify by reading the code, never as pass/fail signals.

Requires only Python 3 (standard library) and `git`.

## Quick start

```sh
./scripts/audit/run.sh            # run everything over crates/
./scripts/audit/run.sh crates     # explicit root

python3 scripts/audit/clones.py crates --help   # every tool takes --help + knobs
```

## The tools

| Script | Finds | Key knobs |
|---|---|---|
| `clones.py` | Structural clones: normalizes identifiers/literals to placeholders, fingerprints each function with k-grams, and reports high-**Jaccard** pairs. Catches parallel-but-renamed functions jscpd cannot. `XFILE` (cross-file) pairs are usually the higher-value targets. | `--min-jaccard`, `--min-tokens`, `-k`, `--df-max`, `--containment`, `--include-tests` |
| `sqldup.py` | Duplicated SQL in string literals: EXACT normalized groups (a probe query inlined everywhere) and NEAR cross-file pairs. `PROD` vs `test` tagged - production cross-crate hits matter most. | `--near`, `--limit` |
| `naming.py` | Parallel function families by name skeleton (`run_*_backfill -> {rsk, hathor, elastos}`). A hint to cross-check with `clones.py`. | `--min` |
| `complexity.py` | Highest control-flow-density functions (decision-point proxy). Simplification, not dedup. | `--min`, `--limit` |
| `coupling.py` | Git **churn** + **temporal coupling**: file pairs that keep changing together (a concept smeared across modules). `XDIR` = different directories. | `--min-co`, `--min-ratio`, `--max-commit-files` |

`_scan.py` is the shared library (Rust file walking, comment/string stripping,
`fn`-body extraction, structural tokenization) used by `clones.py` and
`complexity.py`; it is not a runnable tool.

## How this differs from jscpd

Run both. jscpd (`just arch-lint`) is the **gate** for literal copy-paste and
stays authoritative for CI. These scripts are the **exploratory** layer for the
renamed / SQL / historical duplication jscpd is structurally unable to see.

## Caveats

- Regex `fn` extraction is approximate and can mis-handle exotic macro bodies;
  confirm every flagged pair by reading it.
- High structural similarity does **not** always mean "consolidate". Some pairs
  are already thin adapters over a shared helper (e.g. the per-chain
  `upsert_*_pool_identities` functions), and some `cmd_*`-style families are
  clearer left explicit. Use judgment; the tool finds candidates, not verdicts.
- Test code is excluded by default (`tests/` trees, `tests.rs`, `*_tests.rs`,
  `test_fixtures.rs`) because repetition there is often readability-serving; pass
  `--include-tests` to include it. Inline `#[cfg(test)]` modules inside
  production files are not stripped.
