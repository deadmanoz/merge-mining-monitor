# mmm-capture

The pure offline-evidence foundation of the merge-mining monitor: parse,
normalize, and classify. Every chain adapter, the store, the read model, the
Bitcoin Core layer, the read API, and the binary call INTO this crate with raw
bytes (AuxPoW payloads, coinbase scripts, block headers) and get back normalized,
database-ready evidence and offline verdicts. It writes nothing and talks to
nothing.

## The invariant

`mmm-capture` is PURE. Its entire dependency list is `anyhow`, `bitcoin`, `hex`,
`serde`, `serde_json`, and `tracing`. It links no `tokio-postgres`, no `reqwest`,
no `corepc`, and no chain adapter, and the workspace dependency graph enforces
that (checked with `cargo tree -e normal`). This purity is the crate's core
boundary: the six crates built on top supply the I/O and call pure functions
here, so the parsing, classification, and attribution logic stays testable in
isolation with no database, node, or network. Moving anything that touches a DB,
an RPC, or a client INTO this crate is forbidden.

The second locked contract is byte order. Hashes are rust-bitcoin newtypes
(`BlockHash`, `Txid`, `TxMerkleNode`) that hold wire (internal) bytes and reverse
only on `Display` for the explorer/RPC hex form. Code here stores
`to_byte_array()` output directly and never reverses a hash by hand. The merge
folds, marker parsing, and nBits checks all operate on wire bytes; the one
deliberate reversal (the AuxPoW chain-merkle fold in `auxpow::verify`) is
documented at its site. A "simplification" near any parser that changes byte
semantics is a bug, not a cleanup.

## Layout

Most modules are flat single-job files; the four with internal structure
(`auxpow`, `capture`, `pool_resolver`, `source_registry`) are directories with a
`mod.rs` (the repo bans self-named module files). Where a module is split, a
re-export keeps that restructuring API-compatible for the six consumer crates,
so a split is never a breaking change. Separately, a few helpers reachable only
from tests are gated behind `#[cfg(any(test, feature = "test-support"))]`
(`auxpow::auxpow_blob_summary`, the `nbits_table::classify_nbits_by_time` free
function, `source_registry::{live, historical}`): they are absent from the normal
build intentionally and are not part of the consumer API.

| Module | Responsibility |
|--------|----------------|
| `auxpow` | The AuxPoW wire-format layer: `parse_namecoin_block` (full `getblock 0` bytes), `parse_auxpow_header_blob` (Fractal `[header+CAuxPow]`), `parse_elastos_auxpow` (CAuxPow-only), the child-block coinbase enrichment, `verify_auxpow_commitment` (the full untrusted-endpoint CAuxPow check), PoW-target and BIP34 helpers, and the `evidence` facade (the sanctioned api -> capture decoding edge). Split into `decode`/`reader`/`verify` over a shared bounded byte `Reader`. |
| `capture` | The chain-agnostic core: `NormalizedEventEvidence` -> `MergeMiningEventPayload` via `build_event_payload_from_evidence`, the `EventPoolAttribution` provenance type and its constructors, and the offline pool-attribution resolvers. `capture::sidecars` holds the per-chain data formats (RSK/Hathor evidence sidecar payloads, proof-format and revoke-reason constants), re-exported at `capture::*`. |
| `attribution_policy` | The shared keyed write-decision policy for child-side attribution replays: `ExistingAttributionSet` keyed by `(namespace, matched_value)` with a full-tuple equality check, and `WritePolicy` for the per-replay overwrite-vs-promote-only rule. |
| `child_payout` | Chain-aware child payout/reward address formatting (Namecoin, Syscoin, Fractal) with each chain's own address parameters, deliberately NOT Bitcoin address formatting. |
| `pool_resolver` | Offline pool attribution from two embedded registries: `btc` (BTC coinbase-tag substring + payout-address resolution over `data/pools/current.json`) and `identity` (RSK miner-address resolution over `data/pools/child-identities/rsk_miner_registry.json`), sharing `error` and the `validate_non_empty` helper. |
| `btc_orphan` | The offline BTC orphan classifier: maps a Core-attested-absent, PoW-valid `unknown` parent into strict/weak/excluded, or pending when the committed nBits table cannot yet decide. The Core non-membership gate lives in the read model, not here. |
| `nbits_table` | The embedded BTC nBits-by-DAA-epoch table and its strict (BIP34-height) and weak (timestamp) contamination verdicts, used to reject non-BTC (BCH/BSV) parents offline. Horizon overruns abort the poller rather than misclassify. |
| `core_coinbase` | Bitcoin Core full-block coinbase pool resolution, shared between the Core write paths and the enrichment command. |
| `source_registry` | `SOURCE_REGISTRY` defines every `source` row (code/chain/kind/trust/lifecycle/display) and the `*_SOURCE_CODE` constants. Bound bidirectionally to the producers' `chains::spec` table by a conformance test; feature-gated `generate` deterministically emits the seed SQL and frontend JS. |
| `pool_snapshot_gen` | The feature-gated, pure, unit-tested generator behind `data/pools/current.json` (field map, slug remap, deterministic ordering, byte-stable JSON, churn diff). The committed `current.json` is `include_str!`-embedded and never hand-edited. |
| `test_support` | The shared, feature-gated fixture helpers (headers, fixture loaders) reused by this crate's tests and by the producer and integration-test crates. |

## See also

- `docs/architecture.md` - the workspace crate map and dependency-enforced invariants.
- `docs/data-model.md` - the `mmm_read_model::capture_in_txn` entry point, parent classification, and orphan-class semantics.
- `docs/capture.md` - how chain producers feed the chain-agnostic capture path.
- `docs/attribution.md` - the pool snapshot scope, resolver behavior, and registry discipline.
