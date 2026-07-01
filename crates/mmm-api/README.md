# mmm-api

The read-only HTTP/JSON projection of the merge-mining monitor's SQL read model.
It is the crate behind the `serve` subcommand: an `axum` router over a
`deadpool-postgres` pool that turns the reconciler-derived tables (`block`,
`attestation_proof`, `source_health`) plus block-derived stale competition into
the release API endpoints, plus `ServeDir` static serving for the `www/`
frontend.
It runs read-only `SELECT`s and writes nothing.

## The invariants

`mmm-api` is an I/O leaf that depends on NO writer crate. Its dependency list is
`mmm-capture` + `mmm-pg` plus the transport libraries
(`axum`/`deadpool-postgres`/`tokio-postgres`/`tower`/`tower-http`/`serde`/
`serde_urlencoded`/`time`/`bitcoin`/`hex`). It never links `mmm-store`,
`mmm-read-model`, `mmm-producers`, `mmm-bitcoin-core`, or `mmm-rpc`: it reaches the
database through a `tokio_postgres::GenericClient` and reaches no other crate's
internals. The only sanctioned cross-crate decode edge is
`mmm_capture::auxpow::evidence` (re-parsing stored AuxPoW/coinbase blobs on the
block-detail path) plus `mmm_capture::source_registry` (the source-code
vocabulary). One declared duplication follows from this: `projection/shared/`'s
`load_strict_bip34_height` is a deliberate API-local copy of `mmm-read-model`'s
strict-BIP34 SQL shell, kept here precisely so the api need not depend on the
writer crate.

The HTTP/JSON wire contract, not the Rust API, defines compatibility. The
endpoint set (`/api/v1/tree`, `/block/:hash`, `/navigator/{target}`,
`/sources`, and `/version`), their query params, the success envelope, the
error envelope, and every serialized `serde` field name form that contract. The
response DTO Rust types are internal: they may be renamed, moved, or re-split
freely (only the `serve` binary and the integration tests link this crate). Keep
`fixtures/api/manifest.json`, `docs/api-contract.md`, and
`crates/merge-mining-monitor/tests/api_fixture_contract.rs` aligned when the
example fixtures change. Runtime behavior is enforced by projection, route, and
frontend tests rather than by a field-by-field fixture contract harness.

Byte order is locked wherever the block-detail path re-decodes stored bytes:
rust-bitcoin newtypes hold wire/internal bytes and reverse only on `Display` for
explorer/RPC hex. A "simplification" near a decoder that changes byte semantics is
a bug.

## Layout

Modules with internal structure are directories with a `mod.rs` (the repo bans
self-named module files). `projection/` is one submodule per endpoint family over
a neutral materialization pipeline and a shared row/loader layer.

| Module | Responsibility |
|--------|----------------|
| `lib` | The `serve` subcommand: `ServeConfig::from_env`, the axum `router`, `AppState` + the `deadpool` pool (`build_pool`), route-aware cache headers, and the `ServeDir` static fallback. `serve` and `ServeConfig` are the only production-reached items; endpoint helper modules are private in normal builds and re-open only under the `db-integration` feature for projection/query integration tests. |
| `handlers` | One small per-route handler: validate query params, check out a pooled client, wrap the typed projection in the success envelope. Also owns `/health` liveness and `/ready` database readiness, which intentionally do not use the JSON envelope. |
| `envelope` | The success-envelope writer (`SuccessEnvelope<T>`) and the shared `generated_at` clock. |
| `error` | The `ApiError` contracts (`invalid_query`/`invalid_hash`/`unsupported_source`/`not_found`/`range_too_large`/`backbone_unsynced`/`backbone_conflict`) and the shared error envelope, value-compatible with `fixtures/api/error-*.json`. |
| `normalize` | Shared query-param normalization: the source-code three-tier rejection ladder, hash normalization, `kinds` parsing, and the separate orphan-class `classification` filter. |
| `query` | Strict per-endpoint query parsing and bounds. `query/mod.rs` owns the tree-query ladder (tip / explicit range / `at_height` / `at_time` / compact context / `unheighted_anchor`); `query/navigator.rs` owns the unified `navigator/{target}` parser, opaque cursor contract, and target/mode validation; `query/params.rs` the shared param + UTC date helpers. |
| `projection` | Read-model projection root: `mod.rs` is the curated prelude (not an exhaustive re-export wall) and owns `ProjectionError`; `materialize.rs` is the neutral parent-projection pipeline shared by the tree and block-detail paths. |
| `projection/shared` | Multi-family infrastructure: `mod.rs` holds the shared row types, per-hash proof/observation loaders, row mappers, hash display/parse, cross-endpoint DTOs (`PoolObject`, `SourceSummary`, `ProofState`, `ChildChainEvidence`, `TreeCompetition`), and the declared-dup `load_strict_bip34_height`; `backbone.rs` is the self-contained Bitcoin Core backbone-coverage subsystem (window completeness + the `backbone_unsynced`/`backbone_conflict` checks). |
| `projection/tree` | The `/api/v1/tree` projection. `mod.rs` is the entry + payload DTOs; `window.rs` the exact-height / timestamp-nearest / tip-window resolution; `orphan_component.rs` the anchor-mode orphan-component placement engine; `anchor.rs` the `unheighted_anchor` orphan-fork flow; `compact.rs` the sparse wide-context loader; `build.rs` competition decoration / stale-branch membership / materialization / branch assignment; `reduction.rs` the server-side canonical-context stripping and 500-node cap. |
| `projection/block` | The `/block/:hash` projection: `mod.rs` the payload paths + DTOs, `loaders.rs` the detail SQL, `detail.rs` the commitment/coinbase rendering (re-decoding stored blobs via `auxpow::evidence`) and RSK evidence. |
| `projection/navigator`, `projection/stale_navigation`, `projection/branch_summary` | The unified navigator projection for stale blocks, stale branches, BTC orphans, and orphan branches; the `TreeNavigation` readiness subsystem shared by height-backed navigator targets; and the generic connected-component extraction shared by both branch targets. |
| `projection/sources` | Source health for `/api/v1/sources`: `mod.rs` the wire DTOs + SQL loaders (reads the precomputed `source_health` + `read_model_invariant`, O(sources), fails closed until `source_health_ready`); `sync_status.rs` the pure, I/O-free sync-status classification state machine. |

## See also

- `docs/api-contract.md` - the endpoint, query-param, envelope, and fixture contract (the reference contract for this crate).
- `docs/architecture.md` - the workspace crate map and dependency-enforced invariants.
- `docs/data-model.md` - the reconciler-derived tables this crate projects, the write path it never touches, and `block.btc_orphan_class` semantics.
