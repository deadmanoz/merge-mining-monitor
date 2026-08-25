# Brief: reduce duplicated SQL

A duplication audit found the same (or near-identical) SQL written in more than
one place. Duplicated queries drift: one copy gets a `WHERE` fix the others miss.
Consolidate the query behind one owner, or justify keeping it.

## Finding

```json
<paste one sqldup/sql-exact-dup or sql-near-dup finding from `report.py --json` here>
```

## Method

1. **Read each site.** Confirm the queries are semantically the same, not just
   textually similar (a different `ORDER BY` or predicate is a real difference).
2. **Decide production vs test.** Production copies across crates are the priority.
   Repeated *test* probe queries are lower value; prefer a small shared helper in
   the test support module over a production change.
3. **Pick the owner.** Base-table writes belong to `mmm-store`; derived-table and
   read-model queries to `mmm-read-model`; read-only projections to `mmm-api`. Put
   the query (as a `const &str` or a small function returning rows) in the owning
   module and have the others call it - without crossing a layer boundary.
4. **Watch the boundary.** If two copies live in different layers on purpose (the
   `load_strict_bip34_height` pattern: API must not depend on the writer crate),
   the duplication is deliberate. Report it; do not merge.

## Hard constraints

- Byte-identical result semantics; column order and types unchanged.
- SQL migrations are append-only - this is a query-consolidation task, not a
  schema change. Do not edit historical migrations.
- Respect crate ownership (AGENTS.md); no new cross-layer imports.

## Output

1. A **plan**: the sites, the owning module for the shared query, prod vs test.
2. The **minimal diff**.
3. **Verify**: `just build && just test && just lint` green; for DB-touching
   paths, `just test-integration`. State that you ran them.
