# Brief: consolidate a structural-clone cluster

You are refactoring the `merge-mining-monitor` workspace. A duplication audit
found functions that are structurally identical modulo renames. Your job is to
remove the duplication **through a shared seam** without changing behavior - or
to justify, in writing, why the duplication should stay.

## Finding

```json
<paste one clones/structural-clone finding from `report.py --json` here>
```

If it is one pair, treat it as a cluster of two. If several findings share a
member, treat the whole set as one cluster.

## Method

1. **Read every member.** Open each `file:line` and read the full function. Do not
   trust the similarity score; confirm the logic is genuinely the same.
2. **Apply the deletion test and rule of two.** Extract only if >= 2 real
   instances collapse into one seam that *removes* complexity. A single caller, or
   siblings that only look alike, is not a refactor.
3. **Find the right home for the seam.** The workspace is split by ownership (see
   AGENTS.md): `mmm-pg`, `mmm-capture`, `mmm-rpc`, `mmm-bitcoin-core`, `mmm-store`,
   `mmm-read-model`, `mmm-producers`, `mmm-api`. The shared function lives in the
   crate/module that *owns* the responsibility, and callers depend inward only.
4. **Check for deliberate duplication first.** If the members live in different
   layers (e.g. `mmm-api` and `mmm-read-model`), consolidating may force an illegal
   dependency. `load_strict_bip34_height` is duplicated on purpose so `mmm-api`
   need not import the writer crate. If this cluster is the same shape, **stop and
   report it as intentional**, do not merge it.
5. **Extract the seam.** Parameterize the differences (the chain label, the query,
   the type). Prefer one function or a small trait method with real dynamic
   dispatch - not a facade whose surface is as large as one implementation.

## Hard constraints

- Behavior-preserving: identical SQL results, API payloads, and error text.
- `crates/mmm-api/` must not import producer internals; cross-layer needs an
  explicit shared boundary type or API.
- Hash byte order is fixed (`to_byte_array()` bytes stored directly).
- Keep the change scoped to this cluster. Do not opportunistically refactor
  neighbors.

## Output

1. A short **plan**: the members, the chosen seam and its crate/module, and why
   (or a clear statement that the duplication is deliberate and must stay).
2. The **minimal diff** implementing it.
3. **Verify**: `just build && just test && just lint` all green, and
   `just arch-lint` duplication not worse. State that you ran them.
