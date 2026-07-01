# Documentation

These documents describe the project for maintainers, operators, and curious
readers. They are intentionally human-focused: enough context to understand the
system, run it, and change it carefully.

## Start Here

- `architecture.md` - the system structure: crates, data flow diagram, and ownership
  boundaries.
- `capture.md` - how each merge-mined source is fetched, verified, classified,
  and written.
- `data-model.md` - the Postgres tables, migration rules, read model, and
  Bitcoin parent classification semantics.
- `attribution.md` - pool attribution, child-chain identity registries, and the
  operator-cluster interpretation model.

## Operating The Monitor

- `configuration.md` - environment variables and per-chain config contracts.
- `operations.md` - local setup, migrations, serving, live test deployment, and
  routine operator commands.
- `historical-ingest.md` - importing recovered historical AuxPoW evidence.
- `testing.md` - Rust, Postgres, fixture, and Playwright test layers.
- `release-versioning.md` - version ownership and release-note behavior.

## Product And API

- `product-brief.md` - product intent and audience.
- `api-contract.md` - HTTP JSON contract and shared fixture expectations.
- `tree-semantics.md` - implementation notes for deriving `/api/v1/tree` and
  orphan navigator responses (compact context, orphan placement, tree reduction).
- `ui-model.md` - frontend state model and user-facing tree behavior.

## Data And Fixtures

- `fixtures/api/` contains shared API examples validated by tests.
- `data/` contains embedded pool registries, child-identity registries, source
  profiles, consensus lookup tables, and historical provenance manifests.
- `data/historical/historical-source-manifest.json` records checksums for historical AuxPoW CSV
  provenance inputs; the raw data files are intentionally not committed here.
