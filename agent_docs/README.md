# Agent Documentation

Focused operational and architectural references for coding agents working in `merge-mining-monitor`.
Keep `AGENTS.md` concise and place detailed checklists or subsystem notes here.

- [`round-trip-budget.md`](round-trip-budget.md) - production 340 ms latency context, remote RPC round-trip budgeting, batching/locking rules, transport counter tests, and per-item SQL `EXPLAIN` review requirements.
- [`testing.md`](testing.md) - Rust, Postgres, fixture, and Playwright test layers and schema/lock isolation rules.
- [`release-versioning.md`](release-versioning.md) - SemVer ownership and the 7-step release checklist.
- [`tree-semantics.md`](tree-semantics.md) - backend implementation notes for deriving `/api/v1/tree` compact context, anchor-mode orphan placement, and deterministic tree reduction.
