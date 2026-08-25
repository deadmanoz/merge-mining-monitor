# Brief: centralize scattered configuration

A config audit found environment access or config keys spread across the
workspace, or drift between code and the documented ground truth. Express the
configuration once, at the owning boundary, and close the drift.

## Finding

```json
<paste one configscan finding from `report.py --json` here>
```

Kinds you may receive:
- `config-key-multi-read` - one key read as a literal in several files.
- `config-doc-drift` - a key in `.env.example` but not `docs/configuration.md` (or vice versa).
- `config-undocumented` - a key read in code but documented nowhere.
- `config-structs` / `config-surface` - many per-area `*Config` structs; scatter context.

## Method

1. **Read the sites.** For a multi-read key, open each `file:line`. Is it the same
   value with the same default and validation, or has one copy drifted?
2. **Respect the existing seams.** This repo already centralizes some env access:
   `crates/mmm-producers/src/chains/config.rs` is the *only* place `src/chains/`
   touches `std::env`, resolving per-chain `<PREFIX>_SUFFIX` keys. Do not scatter a
   new read; route it through the owning boundary (chains via that module; `PG*`
   and `BITCOIN_RPC_*` via the producer runtime; `SERVE_*` via `mmm-api`; DB via
   `mmm-pg`).
3. **Collapse a multi-read.** Read the key once at its owner, pass the resolved
   value inward. Prefer a pure `*_from_lookup(lookup)` function (as the chain
   config does) so tests never mutate the process environment.
4. **Close drift, do not hide it.** For `config-doc-drift` /
   `config-undocumented`, update `docs/configuration.md` and `.env.example`
   together so code, docs, and example agree. If a key is dead, remove the read.

## Hard constraints

- Behavior-preserving: same defaults, same validation, same error text. Auth and
  timeout contracts are pinned by tests - keep them verbatim.
- Keep the historical/live source lifecycle distinction in the shared source
  registry, not in per-chain schema branches (AGENTS.md).
- Do not hand-edit generated artifacts; regenerate via the documented `just`
  target if one applies.

## Output

1. A **plan**: the key(s), the owning boundary, and the doc/example updates.
2. The **minimal diff** (code + `docs/configuration.md` + `.env.example` as needed).
3. **Verify**: `just build && just test && just lint` green;
   re-run `python3 scripts/audit/configscan.py crates` and show the finding is gone.
