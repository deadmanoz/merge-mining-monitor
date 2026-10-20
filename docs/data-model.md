# Data Model

The database separates captured evidence from derived presentation state.
Producers append or update base evidence; the read model computes the Bitcoin tree view from that evidence.

## Core Tables

| Table | Purpose |
|---|---|
| `source` | Registered evidence sources such as `auxpow:namecoin`, `auxpow:rsk`, or `live-chaintip:bitcoin:core`. |
| `pool` | Stable pool identities loaded from `data/pools/current.json`. |
| `pool_identity` | Native child-chain identities that map to a pool, such as RSK miner addresses or child reward addresses. |
| `merge_mining_event` | Source evidence keyed by exact or partial authenticated child identity and its Bitcoin parent header; `observation_id` links the event to its child block observation. |
| `child_block_observation` | One row per child block on every chain, written by live capture, migration 0033's fill and per-chain backfills (see `docs/capture.md`), with the block's own child evidence and its `observation_kind`: `evidence` (an event holds the parent's Bitcoin classification), `near` (the parent misses Bitcoin's target; its header and coinbase are kept on the observation), `not_merge_mined` (no AuxPoW), `non_bitcoin_parent` (the parent failed the lineage gate; no parent field is stored), or `unverified_parent` (the child block is authenticated but its AuxPoW proof is malformed or incomplete; no parent field is stored). Child identities, the child-side pool, and RSK chain role, uncle placement and reported difficulty live here. |
| `child_miner_identity` | Raw child identifiers seen on any child block (RSK miner, child payout and reward addresses, Elastos minerinfo), keyed like `pool_identity`; a matching `pool_identity` row maps one to a pool. |
| `historical_event_provenance` | Publication-side chain, source row, classification, validation, and relevance provenance attached to imported events; multiple source rows can map to one event. The `error-block-observations` scope keeps catalogue-backed error witnesses separate from replaceable normal snapshots, while requiring the same archive coordinate to identify the same event across pinned publications. |
| `historical_reconcile_queue` | Durable parent rebuild state and dependent-cascade seeds for resumable historical imports. |
| chain sidecars | One-to-one evidence details for chains with extra structured data, such as RSK and Hathor. |
| `event_pool_attribution` | Attribution rows connecting an event to a pool with source/provenance details. |
| `poll_cursor` | Live poll progress. Backfills never move the cursor. |
| `capture_error` | Producer-owned operational state: one row per `(source_id, height)` a producer could not capture, with the failure kind (`malformed_auxpow_proof` or `height_capture_failed`), a diagnostic detail, and first/last seen times. Written before returning from a failing height and cleared only when that same height is reprocessed successfully; `/api/v1/sources` reads the earliest unresolved height per source. |
| `child_chain_head` | Producer-owned operational state: one row per `(source_id, child_height)` recording the block hash, parent, outcome, and evidence marker a producer last observed at that height, used by trailing rescans. Each poll tick prunes rows below the rescan floor (`cursor + 1 - reorg_depth`) in bounded batches; `child_block_observation` keeps the per-block history. |
| `known_stale_block` | Operator-imported known-stale membership loaded by `import-known-stales` from the upstream `bitcoin-data/stale-blocks` dataset. Consulted as an orphan-classification exclusion gate: a catalogued stale is `excluded`, never labelled strict/weak. |
| `block` | Derived Bitcoin parent block state: canonical, stale, consensus-invalid error block, or unknown. |
| `attestation_proof` | Derived proof rows supporting a block. |
| `source_health` | Per-source rollup counters for UI/API health reporting. |

## Parent Classification

Every stored parent is a Bitcoin header.
Merge mining lets a child chain commit to any SHA-256 parent, so a proof that verifies says nothing about the parent's chain: a Bitcoin Cash block or share, or another SHA-256 chain's block, verifies just as well.
The capture lineage gate (`mmm_capture::lineage::bitcoin_lineage`, applied in `mmm_read_model::capture_in_txn`) refuses such a parent before any parent evidence is written: the event is never stored, and only the child block's `non_bitcoin_parent` observation records that the block exists.
See [Capture](capture.md#bitcoin-lineage-gate).

`btc_parent_kind` is one of:

- `near` - a Bitcoin share: the parent is a Bitcoin header that fails its own target.
- `unknown` - a Bitcoin header that meets its own target, but no Bitcoin-chain membership proof is available.
- `canonical` - Bitcoin Core proves the parent is on the active chain.
- `stale` - Bitcoin Core proves the parent is a valid off-chain Bitcoin block.
- `error_block` - parent hash meets Bitcoin's target but violates a Bitcoin consensus rule.
  The live classifier currently derives `time_below_mtp` from the exact eleven-header predecessor window returned by Core; the pinned research catalogue remains the fallback for other reproducibly validated rules.
  It is neither stale nor an orphan; `block.error_block_reason` records its primary rejection token and `btc_height_source` records whether its height came from the catalogue or a canonical/stale predecessor.

Orphan status is not a `btc_parent_kind`.
It is the derived `block.btc_orphan_class` (`strict_btc_orphan`, `weak_btc_orphan`, or `excluded`; NULL while pending), set only after a Core-absence-attested verdict and the Core-cache-backed strict/weak orphan classifier, which reads the lineage rule: a coinbase height consistent with the header's time and its epoch's bits is strict evidence, bits matching the time's epoch are weak evidence, and a header newer than the cache waits for it.
A verdict attests absence only when the candidate was found absent and every further consensus lookup completed or found nothing; a lookup the lenient live policy tolerated leaves the row pending for the next recheck.
A header on a placed Bitcoin prev whose bits are not that height's gets no absence verdict: it is another chain's header, not an orphan.
Before the strict/weak resolution runs, the classifier consults the operator-imported `known_stale_block` membership (loaded by `import-known-stales` from the upstream `bitcoin-data/stale-blocks` dataset): a catalogued stale is `excluded` outright, never labelled strict/weak, and `import-known-stales` atomically demotes rows classified before the membership was imported.
Published direct-stale and stale-descendant provenance is also an exclusion from strict/weak orphan classification while a branch remains derived `unknown`.

Reviewed body-rule failures use the same `error_block` classification as header-context failures.
Research authenticates complete body evidence and pins the independently established verdicts; Monitor consumes that catalogue.
Migration `0029` drops the obsolete `body_invalid_stale` annotation table.
Its importer, API joins, `body_invalid` and `body_invalid_rule` fields, and compatibility types are removed.
Child witnesses and their publication provenance remain in the normal evidence tables.

Historical error witnesses retain `classification=error_block` in `historical_event_provenance`, while `block.error_block_reason` is derived by the shared Core-plus-catalogue classifier.
They replace a prior `stale` or `unknown` classification for the same parent, rather than acting as a separate display-only override.
Their pinned research publication reference and original source coordinates make an exact replay idempotent; evidence that changes at an existing coordinate conflicts.
Ordinary authoritative-snapshot cleanup intentionally excludes this scope.

## Child Observation Identity

Child height, hash, header, time, and `nBits` are nullable, independent evidence fields.
Missing source evidence remains `NULL`; producers and importers do not fabricate substitutes.

- A real child hash is exact identity under `(source_id, child_block_hash)`.
- One exact identity has one stored Bitcoin parent.
  A second observation with the same source and child hash but a different parent is contradictory source evidence and fails closed.
- Historical child hashes use the exact bytes stored by live capture.
  For SHA256d child headers this is raw `sha256d::Hash::to_byte_array()` order, not reversed display/RPC order.
- A hashless observation uses `(source_id, child_height, btc_parent_header_hash)`.
- Every event has at least a child hash or a child height.
- A later exact observation can promote one unambiguous partial event in place.
- A partial observation represented by one exact event reuses that event.
- Non-null contradictions and ambiguous refinement fail rather than choosing a writer.
- The store returns the resulting inserted, updated, promoted, or exact-satisfied disposition with the event id so import accounting cannot diverge from identity resolution.

The API exposes these fields as nullable values and additionally surfaces an authenticated `child_header_hex` and `child_nbits` when present.

### What `child_block_time` Means

`child_block_time` is the child block's own miner-set timestamp as reported by its chain (header `nTime` for Namecoin-family chains, RSK block timestamp, Hathor transaction timestamp).
Because child data is committed into the Bitcoin coinbase before Bitcoin work exists, it is fixed before the parent is mined and is neither a broadcast time nor a capture time (`event_discovered_at` records the first database write time and is never advanced).

Whether a stored timestamp is verified depends on the row's other columns (which can be filled by separate observations via `fill_existing_event`):
- `child_header_hex` present: header bytes are stored to cross-check the stamp.
- `aux_proof` present: the Bitcoin parent coinbase is proven to commit to the child data (live Namecoin-family, Qbit, and Elastos capture).
- Neither (e.g., RSK, unverified Hathor RPC, or proof-less historical imports): the value is the source's reported number.

A negative offset (`child_block_time < btc_header_time`) is ordinary because a child template is reused across successive Bitcoin jobs (and may be built by a proxied child-chain operator; see `docs/attribution.md`).
A positive offset is an ordering disagreement internal to the block only when both the commitment and the header timestamp were verified.

Historical parent-coinbase evidence is also lossless: structured full transactions populate the normal txid, script, and serialized-output fields, while the complete transaction bytes and normalized publication output text remain attached to the event.

## Child Displacement

When a child chain replaces the block at `child_height`, the replaced block's `merge_mining_event` remains valid Bitcoin-side evidence (its parent header and coinbase commitment are unchanged), so displacement is recorded on the event rather than through `event_revoked_at`:

- `child_displaced_at` (epoch second observed) and `child_displaced_by` (internal-order hash of the replacing child block) are set or cleared together on `merge_mining_event` and never self-reference.
  The same pair is kept on `child_block_observation`, by the same rule in the same transaction, so a block without an event (such as a `not_merge_mined` one) is displaced too.
  Already-displaced events keep their first displacement record; a chain that flips back to a block clears its displacement.
- Bitcoin-side aggregates (`block`, `attestation_proof`, `source_health`, and parent projections) ignore these columns; `event_revoked_at` is reserved for bad evidence.
- `mmm-store::record_child_chain_block(txn, source, height, record)` is the single write path (called by every live producer except RSK, under a per-height advisory lock taken before the event upsert).
  For hashless partial rows, a matching `btc_parent_header_hash` identifies the current block, a different parent or a known non-AuxPoW block displaces it, and an unverified block leaves it untouched.
  Events inserted without a live chain observation (such as historical imports) remain undisplaced until a poller rescan or backfill observes the height.
- The same write updates `child_chain_head` (`(source_id, child_height)` -> observed block hash, nullable parent hash, `outcome` in `captured`/`recorded`/`non_auxpow`/`unverified`/`held`, nullable `evidence_marker`, and `observed_at`).
  Trailing rescans compare a single block-hash lookup against `child_chain_head`: an unchanged hash with a final outcome (`captured`, `recorded`, `non_auxpow`) and unchanged `evidence_marker` skips full proof fetch/capture, whereas a changed hash, missing row, open `capture_error`, or provisional `unverified` parent verdict re-captures the height. Heights below the rescan floor have no row once a tick has pruned them; no rescan reaches them.

## Read-Model Rules

- Derived rows (`block`, `attestation_proof`, `source_health`) are written only through `mmm-read-model` mutation entry points.
- Historical base snapshots enqueue parents in `historical_reconcile_queue` transactionally.
  Primary parent rebuilds commit one at a time, store their changed-hash seeds transactionally, and delete queue work only after the dependent cascade succeeds.
- A bounded Bitcoin Core reorg replaces the complete divergent canonical suffix in one read-model transaction (ending at the tip or at a lagged contiguous cursor): new active headers become `canonical`, displaced headers become Core-attested `stale` competitors retaining their coinbase/pool/proof evidence, existing stales are rebound to the same-height replacement, and every displaced and replacement hash enters `bitcoin_core_reconcile_queue` before commit (`backbone_reorg_reconcile_pending` remains visible on `/api/v1/sources` until both primary strict reconciliation and dependent expansion drain).
- A transient classifier `unknown` never demotes a previously proven `canonical` or `stale` row.
- Bad evidence is removed with explicit event revocation, then the read model recomputes the affected parent state.
  A parent that stops being a Bitcoin header after a Core-cache change is not revoked: the next capture of its child block deletes the event (scoped to that exact child block identity, never every event at the height), and any `block` or `attestation_proof` no evidence attests any longer is deleted with it.
- Bitcoin Core backbone rows are written by `sync-bitcoin-core` and are required for tree windows the UI browses.

## Migrations

Public migration history starts with `0001_canonical_schema.sql` (squashed baseline schema) and `0002_seed_sources.sql` (registry-generated source seed for fresh databases), followed by append-only numbered forward migrations (`0003` through `0032`).
After a migration has reached a persistent database, never edit it; add a new forward migration instead (regenerating `0002` only alongside an idempotent forward migration when adding a source).
Run all real database migrations through `just db-migrate-dev` or `just db-migrate-deploy` (`scripts/migrate-safe.sh`); see `migrations/README.md` for baseline-reset and scheduled-recheck conventions.
