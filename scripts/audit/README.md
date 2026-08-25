# Consolidation audit scripts

Supplementary, **advisory** analyses for spotting consolidation / deduplication /
simplification opportunities. They complement the committed token-literal
duplication gate (`.jscpd.json`, run from `scripts/arch-lint.sh`).

jscpd only matches tokens *verbatim*, so it is blind to:

- renamed ("Type-3") clones - `fetch_error_blocks` vs `fetch_stale_blocks`;
- SQL duplicated *inside* string literals (one opaque token to jscpd);
- configuration reconstructed across crates, and code/doc drift;
- structure that only shows up in **git history** (co-changing modules).

These scripts cover those blind spots. They are **heuristic** (regex scanning,
not a real Rust parser) and are **not** wired into any `just` gate. Treat the
output as leads to verify by reading the code, never as pass/fail signals.

Requires only Python 3 (standard library) and `git`.

## Quick start

```sh
just audit                        # run every detector over crates/ (human output)
just audit-report                 # one ranked Markdown consolidation report
just audit-report crates --json   # the machine-readable findings "data contract"

python3 scripts/audit/clones.py crates --help   # every tool takes --help + knobs
python3 scripts/audit/clones.py crates --json    # every tool also emits --json
```

## The tools

| Script | Finds | Key knobs |
|---|---|---|
| `clones.py` | Structural clones: normalizes identifiers/literals to placeholders, fingerprints each function with k-grams, and reports high-**Jaccard** pairs. Catches parallel-but-renamed functions jscpd cannot. `XFILE` (cross-file) pairs are usually the higher-value targets. | `--min-jaccard`, `--min-tokens`, `-k`, `--df-max`, `--containment`, `--include-tests` |
| `sqldup.py` | Duplicated SQL in string literals: EXACT normalized groups (a probe query inlined everywhere) and NEAR cross-file pairs. `PROD` vs `test` tagged - production cross-crate hits matter most. | `--near`, `--limit` |
| `configscan.py` | Scattered configuration: `env::var`/lookup read sites, per-area `*Config` structs, keys read in >1 file, and three-way drift between code, `docs/configuration.md`, and `.env.example`. | `--docs-dir`, `--env-example`, `--include-tests` |
| `naming.py` | Parallel function families by name skeleton (`run_*_backfill -> {rsk, hathor, elastos}`). A hint to cross-check with `clones.py`. | `--min` |
| `complexity.py` | Highest control-flow-density functions (decision-point proxy). Simplification, not dedup. | `--min`, `--limit` |
| `coupling.py` | Git **churn** + **temporal coupling**: file pairs that keep changing together (a concept smeared across modules). `XDIR` = different directories. | `--min-co`, `--min-ratio`, `--max-commit-files` |
| `report.py` | **Aggregator.** Runs every detector, merges findings into one schema, ranks by severity then a consolidation-first tool priority, and emits ranked Markdown or a single `--json` array. | `--top`, `--json` |

`_scan.py` (Rust file walking, comment/string stripping, `fn`-body extraction,
structural tokenization) and `_report.py` (the shared `Finding` schema + JSON
emission) are libraries, not runnable tools.

## One report + the JSON data contract

Every tool takes `--json`, and `report.py` merges them into one ranked array in a
stable schema (`tool, kind, summary, score, severity, locations[], metrics{}`).
That array is a deterministic **data contract**: hand it to a human, or to an LLM
refactoring loop via the prompt pack below, so the findings never have to be
re-derived (and possibly hallucinated) downstream.

## Refactoring prompt pack (`prompts/`)

`prompts/` turns a finding into a scoped, safe refactor with a coding agent -
`Introspect (report.py) -> Prescribe (a brief) -> Evolve (agent) -> Verify (just
test/lint)`. The briefs are repo-aware (crate ownership, the deliberate
`load_strict_bip34_height` duplication, migrations append-only) and enforce the
rule of two, the deletion test, and deep-module vocabulary. Start with
`prompts/README.md`.

## How this differs from jscpd

Run both. jscpd (`just arch-lint`) is the **gate** for literal copy-paste and
stays authoritative for CI. These scripts are the **exploratory** layer for the
renamed / SQL / config / historical duplication jscpd is structurally unable to
see.

## Complementary native tools

These stdlib heuristics are deliberately zero-dependency. When you want
AST-accurate or build-aware results, reach for the real tools:

- [`similarity-rs`](https://crates.io/crates/similarity-rs) - tree-sitter/AST
  (TSED) Rust clone detection; the accurate counterpart to `clones.py`.
  `cargo install similarity-rs && similarity-rs crates --skip-test`.
- [`cargo-machete`](https://github.com/bnjbvr/cargo-machete) /
  [`cargo-udeps`](https://github.com/est31/cargo-udeps) - unused dependencies.
- [`cargo-modules`](https://github.com/regexident/cargo-modules) - module graph,
  cycles, and orphans (`dependencies --acyclic`, `orphans`).

## Further reading

The design follows the evidence that agent-written code sprawls by *volume*, not
by prompt quality, and that a metric in the loop - not more prose - is what drives
structural fixes:

- GitClear, [AI Copilot Code Quality 2025](https://www.gitclear.com/ai_assistant_code_quality_2025_research) - cloned lines rising, refactoring falling.
- Zhu, Tsantalis, Rigby, [AI-Generated Smells](https://arxiv.org/abs/2605.02741) - LOC<->smells rho=0.94; prompt specificity has no measured effect.
- CodeScene, [agentic refactoring benchmark](https://codescene.com/blog/making-legacy-code-ai-ready-benchmarks-on-agentic-refactoring) - a metric in the loop yields 2-5x more structural refactors.

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
- `configscan.py` keys off env-read *call sites* (`env::var("KEY")`, `lookup(...)`)
  and filters build-time cargo vars, so opcode/status constants do not masquerade
  as config. Per-chain `<PREFIX>_SUFFIX` keys are compared by suffix; drift output
  is advisory (it will not model every chain-specific exception).
- `report.py` runs each detector with thresholds tuned stronger than their
  standalone defaults (e.g. clones `--min-jaccard 0.8`) to keep the aggregate
  focused; run a single tool directly for its full, lower-threshold output.
