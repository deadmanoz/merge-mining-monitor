# Brief: deepen a shallow module / tame a hotspot

A complexity or structure finding points at code that is hard to reason about: a
branch-dense function, or a shallow module whose interface is nearly as complex
as its implementation. Make it a **deep module** - a lot of behavior behind a
small interface - without changing behavior.

## Finding

```json
<paste one complexity/complexity-hotspot (or a module you suspect is shallow) here>
```

## Vocabulary (use precisely)

- **Deep module**: small interface, substantial hidden implementation. The goal.
- **Shallow module**: interface almost as big as the implementation; little
  leverage for callers.
- **Seam**: the single place an interface lives.
- **Leverage / locality**: what callers and maintainers gain from depth.

## Method

1. **Read it and name the concepts.** In the flagged function, identify the
   distinct decisions (the `match` arms, the guard clauses, the retry/await steps).
   A high decision-point count usually means several responsibilities in one body.
2. **Extract by concept, behind a small interface.** Pull each cohesive
   responsibility into a well-named function/module whose *interface* is small even
   though its body is not. Do not just split by line count into shallow helpers
   that callers must re-orchestrate - that moves complexity to the caller.
3. **Apply the deletion test to every new boundary.** If a helper you introduce
   could be inlined with no loss, it was shallow; fold it back.
4. **Keep the orchestration honest.** If the real complexity is call *ordering*
   (not pure logic), make the ordering explicit and testable at the interface,
   rather than extracting pure functions that miss the actual bug surface.

## Hard constraints

- Behavior-preserving: identical outputs, errors, and side-effect ordering.
- Stay within the function's crate and its ownership boundary (AGENTS.md); this is
  a local deepening, not a cross-crate move.
- The workspace gates function length (`too_many_lines = 100`) and duplication;
  fix by structure, never by relaxing a gate.

## Output

1. A **plan**: the responsibilities you found and the deep interface you propose
   (before/after in one paragraph each).
2. The **minimal diff**.
3. **Verify**: `just build && just test && just lint` green; the hotspot's
   decision-point count drops when you re-run `python3 scripts/audit/complexity.py`.
