# Documentation

[Open the live monitor](https://mmm.deadmanoz.xyz/) to browse the evidence.

These documents describe the project for maintainers, operators, and curious readers.
They are intentionally human-focused: enough context to understand the system, run it, and change it carefully.
Contributor and coding-agent implementation references (testing, round-trip budgets, release checklist, and internal tree-reduction semantics) live in [`../agent_docs/README.md`](../agent_docs/README.md).

## Start Here

- [architecture.md](architecture.md) - the system structure: crates, data flow diagram, and ownership boundaries.
- [capture.md](capture.md) - how each merge-mined source is fetched, verified, classified, and written.
- [data-model.md](data-model.md) - the Postgres tables, migration rules, read model, and Bitcoin parent classification semantics.
- [attribution.md](attribution.md) - pool attribution, child-chain identity registries, protocol-account associations, and the operator-cluster interpretation model.

## Operating The Monitor

- [configuration.md](configuration.md) - environment variables and per-chain config contracts.
- [operations.md](operations.md) - [setup](operations.md#local-setup), [dependencies](operations.md#dependencies), a [brief database-size example](operations.md#database-size), migrations and routine operator commands.
- [historical-ingest.md](historical-ingest.md) - importing recovered historical AuxPoW evidence.

## Product And API

- [api-contract.md](api-contract.md) - HTTP JSON contract and shared fixture expectations.
- [ui-model.md](ui-model.md) - product intent, frontend state model, and user-facing tree behavior.

## Data And Fixtures

- `fixtures/api/` contains shared API examples validated by tests.
- `data/` contains embedded pool registries, child-identity registries, source profiles, consensus lookup tables, and historical provenance manifests.
- `data/historical/historical-source-manifest.json` records checksums for historical AuxPoW CSV provenance inputs; the raw data files are intentionally not committed here.
