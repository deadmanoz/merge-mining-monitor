# Refactoring prompt pack

Ready-to-paste briefs that turn audit findings into a scoped, safe refactor with
a coding agent. They are the **Prescribe** step of a metric-in-the-loop workflow:

```
Introspect   python3 scripts/audit/report.py crates --json   # deterministic findings
Prescribe    pick one finding + the matching brief below       # this pack
Evolve       agent proposes a plan, then a minimal diff        # human reviews
Verify       just build && just test && just lint              # behavior + gates hold
```

Why route findings through a deterministic front-end at all: feeding an agent the
exact issues and line numbers stops it inventing problems, and a metric in the
loop drives markedly more *structural* refactors than free-form prompting
([CodeScene](https://codescene.com/blog/making-legacy-code-ai-ready-benchmarks-on-agentic-refactoring),
[RefAgent, ICSE'26](https://homepages.dcc.ufmg.br/~figueiredo/disciplinas/papers/icse2026oueslati.pdf)).
Prompt *specificity alone* does not improve architecture
([arXiv 2605.02741](https://arxiv.org/abs/2605.02741)); the finding is what carries the signal.

## How to use

1. Generate findings: `just audit-report crates --json > /tmp/findings.json`
   (or run a single tool, e.g. `python3 scripts/audit/clones.py crates --json`).
2. Pick the finding you want to act on. Copy that one JSON object.
3. Open the matching brief, paste the finding where indicated, and run it with an
   agent **in a dedicated worktree** (this repo's convention; see AGENTS.md).
4. Review the proposed plan first, then the diff. Require the Verify step to pass.

## Briefs

- `consolidate-clone-cluster.md` - a `clones` structural-clone cluster.
- `reduce-sql-duplication.md` - a `sqldup` exact/near SQL group.
- `centralize-scattered-config.md` - a `configscan` scatter/drift finding.
- `deepen-module.md` - a shallow module / complexity hotspot (Ousterhout deep modules).

## Guardrails baked into every brief

- **Rule of two.** The second instance pays for the extraction; do not abstract a
  single use.
- **Deletion test.** Before removing an abstraction, check that deleting it
  *removes* complexity rather than spreading it to callers.
- **Behavior-preserving.** No observable change; the read model, API payloads, and
  hash byte order are invariants.
- **Respect crate ownership.** The workspace is split by ownership (AGENTS.md).
  Some duplication is *deliberate* to preserve a boundary - e.g.
  `load_strict_bip34_height` is a declared duplication so `mmm-api` need not depend
  on the writer crate. Confirm a clone is not one of these before consolidating.
- **No gate-gaming.** `just arch-lint` red is fixed by refactoring, never by
  raising a threshold or adding an allowlist. SQL migrations are append-only.
- **Honest scope.** Autonomous smell reduction shows a small effect size
  ([arXiv 2511.04824](https://arxiv.org/html/2511.04824)): treat these as
  human-reviewed assists, keep each change small, and commit only when asked.
