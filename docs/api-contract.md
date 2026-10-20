# API Contract

This document defines the release JSON contract for the merge-mining monitor UI.
Fixture examples live in `fixtures/api/`; fixture coverage is listed by `fixtures/api/manifest.json`.

The current implementation supports Namecoin, RSK, Syscoin, Fractal Bitcoin, Hathor, Elastos, Qbit, and Terracoin capture.
See `docs/data-model.md` for schema details, `docs/architecture.md` for current code flow, and `agent_docs/tree-semantics.md` for implementation notes on deriving `/api/v1/tree` and orphan navigator responses.

## Compatibility

All JSON responses use `schema_version = "v1"`.
This project is pre-production, so breaking contract changes are allowed before release when they simplify the shipping API.

The `/api/v1` endpoints return JSON only.
RSS feeds, pagination beyond the documented keyset endpoints, CSV export, and SSE are out of scope.
Runtime health and readiness probes exist outside `/api/v1` and do not use the JSON envelope.

## Health And Readiness

`GET /health` is a liveness probe.
It returns `204 No Content` when the process can answer HTTP, without checking out a database connection.

`GET /ready` is a readiness probe.
It returns `204 No Content` only when the process can check out a Postgres connection and complete `SELECT 1` inside the bounded readiness timeout; otherwise it returns `503 Service Unavailable`.

Both probes send `Cache-Control: no-store`.

## Cache Headers

Successful `/api/v1/*` responses send short public cache headers suitable for a reverse proxy or CDN.
The JSON envelope includes a wall-clock `generated_at`, so conditional JSON requests must be keyed on read-model freshness if they are added later, not on a body hash.

The static frontend is buildless on disk (`www/`): `mmm-api` serves `/` and `/index.html` with a short edge TTL and release-prefixed `/v/<semver>/...` asset URLs, hand-authored `www/` assets use moderate TTLs with revalidation, and only the vendored versioned D3 file is marked immutable.
Error responses and probe responses are not cached.

## Common Envelope

Successful responses include:

- `schema_version`: currently `"v1"`;
- `generated_at`: epoch seconds;
- `query`: normalized query echo for endpoints with query parameters.

Endpoints that take no query parameters omit the `query` field: `/api/v1/version`, `/api/v1/block/:hash` (a path hash only), `/api/v1/sources`, and `/api/v1/competitions`.
`/api/v1/tree` and `/api/v1/navigator/{target}` have bounded query contracts and therefore include a `query` echo.

Field names are snake_case.
Hash values are 64 lowercase display/RPC hex strings.
Non-hash byte fields use a `_hex` suffix and are lowercase hex or JSON `null`.
Empty byte arrays are serialized as `null`, not `""`.

Epoch seconds are used for instant fields ending in `_at`, plus `header_time`, `block.header.time`, and `child_block_time`.
Calendar period fields use UTC dates.
Week periods use ISO weeks, Monday through Sunday.

## Version Metadata

`GET /api/v1/version` has no database dependency.
It returns the running application SemVer compiled from the Cargo workspace package version, plus the release-note data parsed from `RELEASE_NOTES.md` at build time.
The release-note list is unbounded (every section, every item): the About dialog renders it in a dedicated scrollable, per-release collapsible pane.
The `truncated` and `*_count` fields below are retained as a forward-compatible safety net and are currently always consistent with `releases` / `items` (no truncation).

Response fields:

- `version`: SemVer string from root `Cargo.toml` `[workspace.package].version`.
- `release_notes.source`: currently `"RELEASE_NOTES.md"`.
- `release_notes.release_count`: total release-note sections present in `RELEASE_NOTES.md`.
- `release_notes.truncated`: true when `releases` omits older release-note sections; currently always `false`.
- `release_notes.releases`: every release-note section, newest first.
- `release_notes.releases[].version`: section version, for example `"0.3.0"`.
- `release_notes.releases[].date`: optional release date from headings like `## [0.3.0] - 2026-07-28`.
- `release_notes.releases[].items`: top-level bullet entries with wrapped Markdown lines flattened to text.
- `release_notes.releases[].item_count`: total top-level bullet count in that release-note section.
- `release_notes.releases[].truncated`: true when `items` omits later entries from that section; currently always `false`.

Illustrative example (2 release entries; see `fixtures/api/version.json` for the current release's complete machine-checked payload):

```json
{
  "schema_version": "v1",
  "generated_at": 1783792800,
  "version": "0.2.0",
  "release_notes": {
    "source": "RELEASE_NOTES.md",
    "release_count": 2,
    "truncated": false,
    "releases": [
      {
        "version": "0.2.0",
        "date": "2026-07-11",
        "items": [
          "Add complete Lyncoin (11 canonical parents) and SixEleven (7 canonical parents) recoveries, VCash's 68 canonical explorer mappings, and Doichain's completed zero-row survey.",
          "Make source IDs permanent and remove Mazacoin after verifying its source contains no AuxPoW implementation."
        ],
        "item_count": 2,
        "truncated": false
      },
      {
        "version": "0.1.0",
        "date": "2026-07-02",
        "items": [
          "Initial source release of `merge-mining-monitor`, including the Rust workspace, Postgres schema, capture and reconciliation pipeline, read API, and static frontend."
        ],
        "item_count": 1,
        "truncated": false
      }
    ]
  }
}
```

## Source Codes

Source codes use `<kind>:<chain>` or `<kind>:<chain>:<instance>`:

```text
^[a-z][a-z0-9]*(-[a-z0-9]+)*(:[a-z][a-z0-9]*(-[a-z0-9]+)*){1,2}$
```

| Lifecycle | Source Codes | Notes |
|---|---|---|
| `live` | `auxpow:namecoin`, `auxpow:rsk`, `auxpow:syscoin`, `auxpow:fractal`, `auxpow:hathor`, `auxpow:elastos`, `auxpow:qbit`, `auxpow:terracoin` | Active AuxPoW child-chain pollers and backfills. |
| `bitcoin-core-backbone` | `live-chaintip:bitcoin:core` | Bitcoin Core backbone and parent classifier source. |
| `historical` | `auxpow:argentum`, `auxpow:bitcoin-vault`, `auxpow:bitmark`, `auxpow:coiledcoin`, `auxpow:crown`, `auxpow:devcoin`, `auxpow:emercoin`, `auxpow:geistgeld`, `auxpow:groupcoin`, `auxpow:huntercoin`, `auxpow:i0coin`, `auxpow:ixcoin`, `auxpow:lyncoin`, `auxpow:myriadcoin`, `auxpow:rod`, `auxpow:sixeleven`, `auxpow:unobtanium`, `auxpow:xaya`, `auxpow:elcash` | Complete in-scope recovered AuxPoW datasets; no live producer. |
| `partial` | `auxpow:vcash` | Ingestible recovered evidence subset; complete child chain unavailable. |
| `surveyed` | `auxpow:doichain` | Child chain recovered and reviewed; zero admissible Bitcoin evidence rows. |
| `catalogued` | `auxpow:jax-network`, `auxpow:blast`, `auxpow:fusioncoin`, `auxpow:jincoin`, `auxpow:bitcoin-stash` | Known BTC-merge-mined chains with no recovered data, no producer, no `poll_cursor` or `source_health` row, and zero evidence counts. |

Source ids are permanent and never reused (id 32 was retired when Mazacoin was removed for having no consensus AuxPoW implementation).

The `source` query parameter accepts comma-delimited source codes.
Response echoes use `query.sources`, sorted and deduplicated.
Normalization trims ASCII whitespace, rejects empty members as `invalid_query`, rejects unknown enum members as `invalid_query`, rejects syntactically valid but unregistered source codes as `unsupported_source`, and sorts the resulting arrays lexicographically.
When no source filter is provided, `query.sources` is an empty array meaning no source filter.

Malformed source syntax returns `invalid_query`.
Well-formed but unregistered source codes return `unsupported_source`.
Source codes are case-sensitive and must be lowercase.

The `source` registry object has `id`, `code`, `kind`, `chain`, `instance`, `created_at`, `last_seen_at`, `status`, `sync`, and `counts`.
A nested `source_ref` uses only `id`, `code`, `kind`, `chain`, and `instance`.
`last_seen_at` is a derived API field: for AuxPoW sources it is `source_health.last_event_seen`, the latest evidence event or near observation, counting a near from when it was first recorded (re-capturing an unchanged near does not refresh it); live-chaintip freshness is carried by `sync` progress and leaves `last_seen_at` null.

Source enums:

- `kind`: `auxpow`, `live-chaintip`
- `status`: `fresh`, `stale`, `not_started`

`source.chain` is non-null in v1 because source-code syntax requires a chain segment.
`live-chaintip:bitcoin:core` is the Bitcoin Core classifier source.
If another instance is ever chosen, update `fixtures/api/sources.json`, every fixture reference, this document, the manifest, and the source seed migration together.

Historical `auxpow:<chain>` sources are populated by the operator `import-all` or `import-dataset` command and then appear through the same block/proof/source summary projections as live AuxPoW evidence.
The API does not expose a separate dataset row schema for them.

## Response Ordering

Response arrays use stable ordering:

- `tree.nodes`: ascending `height`, then lexicographic `hash`; null heights are rendered after height-backed nodes and then sorted by hash.
  EXCEPTION: in anchor-component mode (`unheighted_anchor`) the placed orphan members all have null `height` and are instead ordered by their derived `placement_height` then stored hash (the layout ranks them by `placement_height`); their root-to-tip order is also carried in `branches[].member_hashes`.
- `tree.edges`: by the index of `to_hash` in sorted `tree.nodes`, then lexicographic `from_hash`.
- `tree.branches`: ascending `btc_height_min`, then lexicographic `root_hash`.
  Orphan branches sort among stale branches by their placement `btc_height_min`.
- `tree.branches[].member_hashes`: root-to-tip order.
- `competitions`: ascending `btc_height`, then lexicographic `stale_hash`.
- `competitions[].sources`: ascending source code, unique.
- `block.proofs`: ascending `source.code`, then smallest `evidence.contributing_event_ids` value.
- `block.event_details`: `event_confirmed_at`, `source`, `child_height` with nulls last, `child_block_hash` with nulls last, then `id`.
- `sources.sources`: ascending `id`.

## Core Entities

See `docs/data-model.md` for base tables (`merge_mining_event`, chain sidecars, `event_pool_attribution`), parent classification kinds (`near`, `unknown`, `canonical`, `stale`, `error_block`), and the Bitcoin lineage gate (`docs/capture.md`, "Bitcoin Lineage Gate": non-Bitcoin SHA-256 headers are refused at capture and never stored).
A `near` parent has no events: the API projects it from its child block observations (`observation_kind = 'near'`), and it is not a tree node. For `unknown` without a `block` row the API projects directly from active `merge_mining_event` rows; for `canonical`, `stale`, and `error_block`, it reads the reconciled `block` / `attestation_proof` model.

API-computed miner and branch entities:

- `bitcoin_miner_pool`: miner/pool resolved strictly from Bitcoin parent coinbase evidence (never child-chain evidence).
- `child_miner_pool`: miner/pool resolved for one child-chain event from that child chain's block evidence (`Unknown` when unresolved).
- `display_miner_pool`: presentation-layer best-available miner/pool on tree nodes and block details (never overrides `bitcoin_miner_pool`).
  Equals `bitcoin_miner_pool` when known; when `bitcoin_miner_pool` is `Unknown` (e.g., an RSK-only stale block with no recoverable Bitcoin coinbase), falls back to the single distinct known `child_miner_pool` across active events for the parent (zero or conflicting known child pools resolve to `Unknown`).
- `display_miner_basis`: how `display_miner_pool` was resolved: `bitcoin_coinbase` | `child_inferred` | `unknown`.
  Near pages and direct-projected `unknown` parents are never child-inferred.
- `stale_branch`: one or more stale blocks linked by parent-child edges, with branch depth, member hashes, and canonical competitor references.

## Pool Objects

Known pools expose identity only:

```json
{"id": 7, "slug": "antpool", "name": "AntPool", "known": true}
```

Unknown pools use:

```json
{"id": null, "slug": null, "name": "Unknown", "known": false}
```

Known slugs must match lowercase letters, digits, and hyphens.
RSK miner-address attribution remains visible in `event_details[].rsk.pool_identity` and is not treated as identical to Namecoin parent coinbase attribution.

Event attribution rows expose `namespace`, `match_kind`, `matched_value`, `pool`, nullable `pool_identity`, `source`, `confidence`, and `details`.
Known attributions use the same pool object as the legacy `pools` field; unresolved observed identifiers use the Unknown pool object with `pool_identity: null`.

## Counter Semantics

`source_summary.sources` is the sorted unique source-code array that contributes active evidence for the parent header in the response.
`/api/v1/tree` may also emit canonical context nodes with `sources: []` when source filters remove all evidence but the node is still needed to connect visible structure; those nodes are context, not evidence, and do not satisfy `min_sources`.

`source_summary.distinct_sources` is `COUNT(DISTINCT source_id)` across active evidence for that parent header.
Direct `unknown` projections compute that distinct source count from non-revoked `merge_mining_event` rows, and a near page from its observations, so two Namecoin child blocks for one parent still produce `distinct_sources = 1`.
Canonical and stale projections count non-revoked `attestation_proof.source_id` values and add the synthetic Bitcoin source when the `block` row is Core-attested or live-observed.

`source_summary.auxpow_chain_count` counts distinct AuxPoW child chains among active evidence.
Namecoin plus RSK for the same parent gives `2`.

`source_summary.live_observed` is true when the parent header is represented by live Bitcoin Core evidence.
Core-only backbone rows carry that provenance on the canonical `block` row and expose it through `block.source_summary`.

`source_summary.pow_validates_btc_target` describes the Bitcoin parent header, not the child-chain target.
Child-chain target validation stays on `event_details[].pow_validates_child_target`.

## Endpoint List

### `/api/v1/tree`

Purpose: project the windowed Bitcoin header tree from the synced Bitcoin Core backbone and active merge-mining evidence.

Query:

```text
GET /api/v1/tree?at_height&at_time&context=compact&from_height&to_height&kinds=unknown,canonical,stale,error_block&classification=strict_btc_orphan,weak_btc_orphan&source=auxpow:namecoin,auxpow:rsk,auxpow:syscoin,auxpow:fractal,auxpow:hathor&include_unheighted=true&unheighted_from=2026-05-01&unheighted_to=2026-05-26&min_sources=1
```

#### Lookup Modes (Mutually Exclusive)

| Mode (`query.window_mode`) | Parameters | Behavior & Constraints |
|---|---|---|
| `"tip"` (default) | omit all lookup bounds | Returns a 16-block canonical window ending at the highest complete contiguous canonical backbone window (not raw `MAX(block.btc_height)`). If no complete canonical backbone height exists, returns an empty tree with nullable window bounds and `empty_reason = "no_canonical_tip"`. |
| `"height"` | `at_height` (optional `context=compact`) | Exact single-height lookup `H..H` (or bounded fork-explaining context when `context=compact`). Manual UI `tree_height` maps to `at_height&context=compact`. |
| `"time"` | `at_time` (`YYYY-MM-DDTHH:MM:SSZ`, optional `context=compact`) | Resolves to the complete canonical block at or before `at_time` using deterministic ordering `btc_header_time DESC, btc_height DESC, btc_header_hash ASC`, then renders its height. If none exists, returns an empty non-tip window with `empty_reason = "no_complete_canonical_at_or_before_time"`. Manual UI `tree_time` maps to `at_time&context=compact`. |
| `"explicit"` | `from_height` and `to_height` | Legacy/internal explicit height range (max 2,016 heights) for direct API callers and backend-generated navigator `tree_window=generated` links (`tree_from`/`tree_to`). Both bounds are required together; supplying only one is `invalid_query`. |
| `"unheighted_anchor"` | `unheighted_anchor` (64-char hex hash) | Anchor-centered BTC-orphan view: renders the proven `prev_hash`-linked orphan component containing the anchor against a local `±16` canonical context window (`tip_height = null`, `defaulted_to_tip = false`, `empty_reason = null`). The anchor must be an existing PoW-valid `unknown` block in the active `classification` set (else `not_found`). Ignores `kinds`, `source`, and `min_sources`. Falls back to a flat time-ordered strip (250-node cap, null window bounds) when no placement height or in-window canonical attachment exists. See `agent_docs/tree-semantics.md`. |

#### Query Modifiers And Backbone Integrity

| Parameter / Rule | Contract |
|---|---|
| `context` | `"exact"` (default) or `"compact"`. `context=compact` is accepted only with `at_height` or `at_time`; combining it with `from_height`/`to_height`, `unheighted_anchor`, `include_unheighted=true`, `unheighted_from`, or `unheighted_to` is `invalid_query`. Collapses uneventful canonical spans into `hidden` edges while preserving the requested target, stale rows, canonical competitors, and branch/root/tip context. Does not auto-attach BTC orphan branches (use `/api/v1/navigator/orphan` and `unheighted_anchor`). |
| `classification` | Comma-delimited filter over orphan classes (`strict_btc_orphan`, `weak_btc_orphan`, `excluded`, `pending`; default `strict_btc_orphan,weak_btc_orphan`). Separate from `kinds`: orphan classes refine `kind='unknown'` (`block.btc_orphan_class`) and are never passed through `kinds`. Scopes the `unknown` population in `unheighted_anchor` and `include_unheighted` modes; inert in height-window and tip modes. Unknown members return `invalid_query`. |
| `include_unheighted`, `unheighted_from`, `unheighted_to` | Direct-projected `unknown` nodes have `height = null` and are excluded from height windows unless `include_unheighted=true` with required inclusive UTC date bounds `unheighted_from`..`unheighted_to` (capped to 31 UTC calendar days; returns `range_too_large` if `> 250` unheighted nodes survive filters). Block-backed unknowns are filtered by `block.btc_orphan_class`; direct-evidence unknowns with no `block` row are treated as `pending`. |
| Backbone integrity (`backbone_unsynced` / `backbone_conflict`) | The request path never calls Bitcoin Core. Every canonical height in the resolved window must exist as a single `kind = 'canonical'` row with `btc_coinbase_status = 'complete'` (populated by `sync-bitcoin-core`), independent of `kinds` and `classification` filters. Missing rows or incomplete/failed Core coinbase evidence return HTTP 409 `backbone_unsynced` (`first_missing_height` names the first uncovered or partial height; the contiguous-complete cursor stays pinned before any failed coinbase height). Duplicate canonical rows or broken `prev_hash` links return HTTP 409 `backbone_conflict`. Clients should surface the 409 action rather than retrying in a loop. |

#### Response Envelope

- `query`: echoes `from_height`, `to_height`, `at_height`, `at_time`, `window_mode` (`"tip"` | `"explicit"` | `"height"` | `"time"` | `"unheighted_anchor"`), `context` (`"exact"` | `"compact"`), `include_unheighted`, `unheighted_from`, `unheighted_to`, and `unheighted_anchor`.
- `window`: nullable `btc_height_min`, nullable `btc_height_max`, nullable `tip_height`, `defaulted_to_tip`, nullable `empty_reason`, and `hidden_linear_block_count`.
- `nodes[]`: sorted by height then hash (in `unheighted_anchor` mode, placed orphan members sort by `placement_height` then hash).
- `edges[]`: drawn-edge graph including `hidden` edges for collapsed canonical context.
- `branches[]`: stale-branch and multi-block orphan-branch summaries.
- `legend`: `kinds` and `edge_kinds`.

#### Tree Node Fields

| `nodes[]` Field | Contract |
|---|---|
| `id` | Response-local integer layout ID. |
| `hash`, `height`, `kind` | Parent header display hash, nullable Bitcoin height (`null` for direct-projected `unknown` and placed orphans), and structural parent kind. |
| `prev_id`, `prev_hash` | Layout ID of visible predecessor and stored parent `prev_hash`. A `hidden` edge may make `node.prev_hash` differ from the visible `prev_id` node's hash. |
| `btc_orphan_class` | Derived refinement of `kind='unknown'` (`strict_btc_orphan` / `weak_btc_orphan` / `excluded`), or `null` for canonical/stale/error-block nodes and pending unknowns. Per-node detail field, not a `kind`. |
| `pool` | Best-available display miner pool object (`display_miner_pool`). |
| `source_summary` | Evidence source summary for the parent header. |
| `child_chain_evidence[]` | Grouped by active AuxPoW `source` and `child_chain`, with event count and child-height min/max. |
| `proof_state` | `{ has_auxpow_evidence, has_live_observation }`. |
| `competition` | Paired canonical competitor details for `stale` nodes, else `null`. |
| `branch` | `{ "branch_id": "<id>" }` for stale members (`stale-<btc_height_min>-<root_hash>`) and multi-block orphan component members (`orphan-<root_hash>`), else `null`. Orphan `TreeBranch` entries in `tree.branches[]` use placement heights for `btc_height_min`/`btc_height_max` and `canonical_competitor_hashes: []`. |
| `placement_height` | Optional; present on placed orphan nodes in `unheighted_anchor` mode only (omitted elsewhere and on `/api/v1/block/:hash`). Derived layout height (`btc_height` stays `null`). |
| `placement_approx` | Optional; present with `placement_height`. `true` when placement is approximate rather than a validated strict BIP34 height (UI prefixes `~`). See `agent_docs/tree-semantics.md`. |

#### Tree Edge Kinds

Clients must derive hidden context from `edges[]`.
`edges[]` is the drawn-edge graph: a node may carry `prev_id`/`prev_hash` for a visible predecessor without a matching `edges[]` entry when that transition has no drawable edge kind (an error-block or unknown child, or a canonical child with a non-canonical predecessor).

| `edge_kind` | Meaning | Extra fields |
|---|---|---|
| `canonical` | visible canonical parent-child edge | none |
| `stale_entry` | visible edge entering a stale branch root | none |
| `stale` | visible stale-member to stale-member edge | none |
| `hidden` | collapsed canonical context | `hidden_count`, greater than zero |
| `orphan` | anchor-mode fork edge from a verified strict root to its canonical predecessor (`placement_height - 1`), or a proven orphan-to-orphan prev_hash link inside a multi-block component | none |
| `orphan_approx` | anchor-mode fork edge for an approximate placement (weak/excluded/pending root, or a strict root whose predecessor is absent/mismatched), to the nearest in-window canonical | none |

Tree reduction is deterministic: collapsed canonical context is represented by `hidden` edges with `hidden_count` and summarized by `window.hidden_linear_block_count`.
Responses return at most 500 nodes, yielding `range_too_large` (HTTP 422) when required nodes cannot fit within that cap (see `agent_docs/tree-semantics.md`).

### `/api/v1/block/:hash`

Purpose: fetch details for a selected parent header.

The hash path parameter may be uppercase or mixed-case when valid hex; the API normalizes valid hashes to lowercase before lookup and returns `invalid_hash` for invalid syntax.
Direct `unknown` projections group active non-revoked `merge_mining_event` rows by `btc_parent_header_hash` (the canonical/stale/error-block read model takes precedence).
A `near` parent's page is built from its child block observations: `block.kind = "near"`, the header from the stored 80-byte parent header, `bitcoin_miner_pool` from the observations' parent pool, `display_miner_pool` Unknown, `event_details: []`, and `observations[]`.
Core-only backbone rows represent Core provenance on `block` and `block.source_summary` without a separate observation payload.

| Response Field | Contract |
|---|---|
| `block` | `hash`, nullable `height`, `kind`, nullable `btc_orphan_class` (`strict_btc_orphan` / `weak_btc_orphan` / `excluded` for classified `unknown`, else `null`), nullable `error_block_reason` (primary consensus-rejection token for `kind = 'error_block'`, else `null`), nullable `coinbase_tag`, `header`, `bitcoin_miner_pool`, `display_miner_pool`, `display_miner_basis`, and `source_summary`. Near pages and direct-projected unknown blocks carry `btc_orphan_class: null` and `error_block_reason: null`. |
| `block.coinbase_tag` | For Core-attested canonical blocks with stored `block.btc_coinbase_script`, derived from the Core block row (even when child events exist), preferring the BTC pool resolver's matched tag and falling back to the normalized printable run with surrounding `/` delimiters trimmed. When no Core tag exists, derived from `commitment.parent_coinbase_script_hex` (printable ASCII `0x20..=0x7e`, split on other bytes, trimmed runs of `>= 4` chars joined by a single space). `null` for RSK and Hathor commitments. |
| `commitment` | Parent-level AuxPoW commitment, or `null` when no recognized AuxPoW-format event exists. Carries `format` (chosen by family priority: `namecoin-aux` > `qbit-aux` > `rsk-opaque` > `hathor-rfc0006`), `parent_coinbase_txid`, `parent_coinbase_script_hex`, and `marker`. `namecoin-aux` and `qbit-aux` persist the parent coinbase and populate txid/script; `rsk-opaque` and `hathor-rfc0006` are format-only with `null` coinbase fields and `marker: null`. |
| `commitment.marker` | Non-null only for `namecoin-aux` or `qbit-aux` parents whose coinbase scriptSig yields a `0xfabe6d6d` marker (Qbit mainnet also accepts legacy no-marker placement, where `marker` is `null`). Carries `magic_present`, display-order `aux_merkle_root`, `merkle_size`, and `merkle_nonce`. For `namecoin-aux` the scriptSig commits wire order (reversed for display); for `qbit-aux` the scriptSig commits display order as-is. |
| `proofs[]` | Derived AuxPoW proofs (`proofs[].source` is a `source_ref` object). `proofs[].evidence.contributing_event_ids` hydrates into sibling `event_details[]` (canonical and stale fixtures require exact set equality between proof IDs and hydrated event detail IDs). |
| `event_details[]` | Hydrated per-child events (`event_details[].source` is a source-code string). Includes `chain_id` (reference AuxPoW chain ID: Namecoin = 1, Elastos = 1224, Qbit = 47, Terracoin = 50, else `null`) and `slot_index` (slot in the parent's aux merkle tree, selected by chain slug format — classic CAuxPow for Namecoin family, Qbit native format for `qbit` — and gated on the blob's embedded parent header matching `btc_parent_header_hash`). |
| `observations[]` | Present only on a near parent's page (omitted when empty), one entry per child block observation in `recorded_at`, source, child height, child hash order: `observation_id`, `source`, `child_chain`, nullable child height, hash, header hex, time and nBits, nullable `child_chain_role` (`canonical` / `uncle`), `uncle_index` and `uncle_parent_height`, displacement (`child_displaced_at`, `child_displaced_by`), nullable parent and child coinbase txid, script hex and outputs hex, nullable `pow_validates_child_target`, `recorded_at`, `child_identities[]` (`namespace`, `identifier`, nullable `pool_identity`, `pool`), `child_pool`, and `btc_parent_pool`. A near keeps no proof bytes, event times, revocation state, or per-match attribution detail. |
| `event_details[].aux_proof` | Decoded merge-mining merkle proof (`null` when absent or failing the `slot_index` format/parent-match gate): `hash_block` (`CAuxPow::hashBlock`, present for classic CAuxPow and absent for `qbit`), `coinbase_branch`, and `blockchain_branch`, each with `index` (side-mask; `blockchain_branch.index` is the slot) and display-order `siblings`. Raw `aux_merkle_proof_hex` remains alongside it. |
| `competition` | Nullable object (`null` except for paired `stale` blocks): `btc_height`, `stale_hash`, `canonical_hash`, `stale_bitcoin_miner_pool`, `canonical_bitcoin_miner_pool`, `header_time_delta_s`, and nullable `propagation_delta_s`. |
| `stale_branch` | Nullable branch summary. Positions are `root` (no parent, `>= 1` child), `interior`, `tip` (parent, no children), and `root_and_tip` (requires `depth = 1`). |

### `/api/v1/navigator/{target}`

```
GET /api/v1/navigator/{target}?limit&cursor&direction&anchor_hash&classification
```

Purpose: one bounded navigation API for the header-tree controls.
The five targets are:

| Target | Axis | Item scope | View |
|---|---|---|---|
| `stale` | `height` | proven stale blocks | `tree_window` |
| `stale-branch` | `height` | current multi-block stale branches | `tree_window` |
| `error-block` | `height` | consensus-invalid full-PoW blocks | `tree_window` |
| `orphan` | `time` | BTC-orphan blocks | `unheighted_anchor` |
| `orphan-branch` | `time` | multi-block BTC-orphan branches | `unheighted_anchor` |

All targets use the same modes:

- latest mode: omit `cursor`, `direction`, and `anchor_hash`; returns the newest item(s) for the target.
- page mode: pass an opaque response `cursor` plus `direction=older|newer`; returns the next page on that side of the cursor.
- anchor mode: pass `anchor_hash=<64 lowercase display hex>`; returns the item containing that hash.
  For branch targets, interior member hashes resolve to their owning branch, not just the root.

Query parameters:

- `limit`: optional page size, default `1`, maximum `2000`; `limit=0` is `invalid_query`.
- `cursor`: opaque hex-encoded server cursor from a previous navigator item or `next_cursor` / `prev_cursor`.
  It is target-bound and axis-bound; a cursor from another target is `invalid_query`.
- `direction`: required with `cursor`, and only valid with `cursor`.
- `anchor_hash`: mutually exclusive with `cursor`.
- `classification`: accepted only by `orphan` and `orphan-branch`, defaulting to `strict_btc_orphan,weak_btc_orphan`.
  The valid classes are `strict_btc_orphan`, `weak_btc_orphan`, `excluded`, and `pending`.

Supplying any other parameter, mixing `cursor` with `anchor_hash`, supplying only one of `cursor` / `direction`, or passing `classification` to a stale or error-block target is `invalid_query`.
`error-block` rejects `classification` because catalogue membership is not a refinement of `unknown` and so has no orphan-class axis.

Response fields:

- `query`: normalized target, mode, cursor, direction, anchor hash, classification, and limit.
- `target`: the target string from the path.
- `items[]`: target-specific navigator items, newest-first regardless of whether the page was reached by an older or newer cursor.
- `total`: the global count for the active target and classification filter.
- `facets`: `{}` except for `orphan`, where it includes `orphan_classes: { strict, weak, excluded, pending }`, the full per-class breakdown over PoW-valid unknown rows independent of the active filter.
- `next_cursor`: opaque cursor for the next older page, or `null` at the oldest edge.
- `prev_cursor`: opaque cursor for the next newer page, or `null` at the newest edge.

Each `items[]` entry has this format:

- `id`: stable target-local id.
- `kind`: one of the target strings.
- `primary_hash`: display/RPC hash for the block or branch root.
- `label`: display label for the UI.
- `position`: `{ axis, min, max }`; the stale and error-block targets use Bitcoin height, orphan targets use `btc_header_time`.
  For `error-block`, `min` and `max` are always equal, because each item is one block.
- `index`: 1-based newest-first rank in the active target (and classification filter).
  `1` is the newest item; `total` is the oldest. Items in one response share one snapshot with `total`, so `index <= total`.
- `cursor`: opaque item cursor.
  Clients must send it back as-is.
- `branch`: `null` for single-block targets, or `{ branch_id, root_hash, tip_hashes, depth }` for branch targets.
- `orphan`: `null` except for the `orphan` target, where it is `{ btc_orphan_class }` (`null` only when `pending` is in the active filter).
- `view`: either `null`, `{ mode: "tree_window", target_height, tree_from, tree_to, select_hash, center_hash }`, or `{ mode: "unheighted_anchor", anchor_hash, select_hash, center_hash }`.
- `view_error`: `null` unless the backend cannot advertise a safe tree-window view, in which case it is `{ code, target_height, message, action }`.

Ordering is stable and target-specific:

- `stale`: `btc_height` descending, then stored stale hash bytes ascending.
- `stale-branch`: `btc_height_max` descending, then `btc_height_min` descending, then stored root hash bytes ascending.
- `error-block`: `btc_height` descending, then stored header hash bytes ascending.
  The hash tie-break is required rather than cosmetic: the catalogue carries more than one block at some heights, so paging on height alone would skip or repeat members of a group (`branch` and `orphan` are always `null`).
- `orphan`: `btc_header_time` descending, then stored header hash bytes descending.
- `orphan-branch`: `btc_header_time_max` descending, then `btc_header_time_min` descending, then stored root hash bytes ascending.

Single-block stale items round-trip to `/api/v1/block/:hash` and to tree node hashes.
Stale branch items include only current components whose stale members have a derivable canonical competitor, with `depth >= 2`; one-block stale branches remain single `stale` items and block detail.
Orphan branch items include only proven prev_hash-linked orphan components with `depth >= 2`; one-block orphan components remain single `orphan` items. Branch endpoints intentionally omit member lists, pool detail, and placement heights; the tree and `/api/v1/block/:hash` endpoints expose those when a branch is selected.
See `agent_docs/tree-semantics.md` for where `btc_orphan_class` appears across endpoints.

### `/api/v1/sources`

Purpose: drive source filters and health panels.

Response fields:

- `sources[]` sorted by `id`.
- `status`: evidence freshness only (`fresh`, `stale`, or `not_started`; `not_started` requires derived `last_seen_at = null`).
- `sync`: source capture progress, tracked separately from evidence freshness.

| `sync` Field | Contract |
|---|---|
| `sync.mode` | `live`, `bitcoin-core-backbone`, `historical`, `partial`, `surveyed`, `catalogued`, or `unknown`. |
| `sync.state` | `live`, `catching_up`, `stale`, `error`, `not_started`, `historical`, `partial`, `surveyed`, `catalogued`, or `unknown`. |
| `sync.progress_height` | Generalized progress height for live AuxPoW producers or the Bitcoin Core contiguous backbone, else `null`. Present iff `progress_updated_at` is present. |
| `sync.progress_updated_at` | Unix seconds when the progress row was last seeded or advanced, else `null`. `poll_cursor.updated_at` is the cursor seed/advance time, not a producer heartbeat; a caught-up source with no recent cursor advance can age into `stale` without the poller being down. |
| `sync.target_height` | Observed AuxPoW chain tip for live AuxPoW sources or current Bitcoin Core backbone target tip height, else `null`. |
| `sync.latest_evidence_at` | Newest AuxPoW evidence timestamp observed for a live AuxPoW source (independent of cursor progress and may lag or lead `progress_updated_at`), else `null`. |
| `sync.error_code` | Current capture error, else `null`. For `live-chaintip:bitcoin:core`, the latest backbone error code from `bitcoin_core_sync_state`. For live AuxPoW sources, `auxpow_capture_error` when at least one unresolved `capture_error` height exists. |
| `sync.error_height` | Height associated with `sync.error_code`, else `null` (Bitcoin height for the backbone; lowest unresolved child height from `capture_error` for live AuxPoW sources, bounding trustworthy coverage). |

| `sync.mode` | `sync.state` Derivation Rules |
|---|---|
| `live` (AuxPoW) | `error` whenever `sync.error_code` is set (ahead of all other verdicts; clears only when each held height is reprocessed, while progress fields continue reporting the monotonic cursor). Otherwise uses a 1-hour cursor-age window: missing cursor -> `not_started`; cursor older than 1 hour -> `stale`; fresh cursor below `target_height` -> `catching_up` (including transiently over the reorg window after tip-anchored seed); fresh cursor with no target or `>= target_height` -> `live`. |
| `bitcoin-core-backbone` | Derived from `bitcoin_core_sync_state`: no row -> `not_started`; `last_error_code` set -> `error`; row older than 1 hour -> `stale`; fresh row with no target or no contiguous progress -> `not_started`; fresh row below target -> `catching_up`; fresh row at/beyond target -> `live`. |
| `historical`, `partial`, `surveyed`, `catalogued` | Reports the lifecycle token as both `sync.mode` and `sync.state` with `null` progress fields. |

Per-source `counts` (`counts.events`, distinct-parent `near`/`unknown`/`canonical`/`stale`/`error_block`, and `strict_orphan`/`weak_orphan`) are served from precomputed `source_health` (O(sources)). `counts.events` counts active evidence events (parents that meet Bitcoin's target); `counts.near` counts distinct near parents from near observations, which event revocation does not change: revocation covers evidence events.
This endpoint **fails closed** (HTTP 500 `internal_error`) rather than returning zeros when `source_health_ready` is false (run `reconcile-read-model --rebuild-source-health` after initial schema creation or bulk backfills) or if any active `unknown` parent fails its Bitcoin target.

### `/api/v1/competitions`

```
GET /api/v1/competitions
```

Purpose: serve every derivable stale-vs-canonical competition in one read, so a client can bin, window and filter the header-time-delta distribution without a request per interaction.
There are no query parameters; the set is small and slow-growing, and all selection is client-side.

Response fields:

- `competitions[]` ordered by ascending `btc_height`, then lexicographic `stale_hash` (the same rule as `tree.nodes`).
  Height alone is not a stable key: one Bitcoin height can carry many stale blocks.

Per row:

- `btc_height`, `stale_hash`, `stale_header_time`;
- `header_time_delta_s`: `canonical.btc_header_time - stale.btc_header_time`.
  Positive means the stale block carries the earlier timestamp.
  JSON `null` when the difference falls outside i32; consumers must treat `null` as unavailable and exclude it from aggregates, never coerce it to `0`;
- `stale_bitcoin_miner_pool` and `canonical_bitcoin_miner_pool`: the same pool objects the tree and block endpoints serve;
- `sources[]`: sorted, unique active evidence source codes for the stale block.
  These are the non-revoked `attestation_proof` source codes plus the synthetic `live-chaintip:bitcoin:core` when the stale block is Core-attested or live-observed, matching what `block.source_summary.sources` reports for the same block so a shared Source filter selects the same rows in both views.

A row appears only when the stale block's `canonical_competitor_hash` resolves to a block that is both `canonical` and at the **same** `btc_height`.
Those predicates are load-bearing: the column is only a foreign key to `block` with a not-self check, so nothing in the schema prevents it pointing at a non-canonical row, or at a canonical row from another height.
Two blocks at different heights never raced, so such a pair is not a competition: its delta and pool pair would be meaningless, and `canonical_hash` would not be recoverable from the reported `btc_height` as described below.

`canonical_hash` is deliberately not served here.
It is reachable from `btc_height` through `/api/v1/tree`, and a second 64-character hash per row roughly doubles the payload for a value this endpoint's clients do not need.

## Nullability By Parent Kind

| Field | near | unknown | canonical | stale | error_block |
|---|---|---|---|---|---|
| `block.hash` | required | required | required | required | required |
| `block.height` | null (direct-projected) | null (direct-projected) | required | required | required |
| `block.kind` | required | required | required | required | required |
| `block.error_block_reason` | null | null | null | null | required |
| `block.coinbase_tag` | null or printable tag | null or printable tag | null or printable tag | null or printable tag | null or printable tag |
| `block.header` | required | required | required | required | required |
| `block.bitcoin_miner_pool` | required | required | required | required | required |
| `block.display_miner_pool` | required (Unknown) | required | required | required | required |
| `block.display_miner_basis` | `unknown` | one of `bitcoin_coinbase` / `child_inferred` / `unknown` | one of `bitcoin_coinbase` / `child_inferred` / `unknown` | one of `bitcoin_coinbase` / `child_inferred` / `unknown` | one of `bitcoin_coinbase` / `child_inferred` / `unknown` |
| `block.source_summary.sources` | sorted non-empty array | sorted non-empty array | sorted non-empty array for evidence; may be empty for `/tree` canonical context | sorted non-empty array | sorted non-empty array |
| `block.source_summary.distinct_sources` | required | required | required | required | required |
| `block.source_summary.auxpow_chain_count` | required | required | required | required | required |
| `block.source_summary.live_observed` | false | false unless observed | required | required | false |
| `block.source_summary.pow_validates_btc_target` | false | true | true | true | true |
| `proofs` | empty array | empty array until proof derivation | array | array | array |
| `event_details` | empty array | non-empty array | array | array | array |
| `observations` | non-empty array | omitted | omitted | omitted | omitted |
| `competition` | null | null | null | required when paired | null |
| `stale_branch` | null | null | null | required | null |

The `unknown` invariant depends on producer behavior: rows with failing Bitcoin target are `near`, not `unknown`.
The first non-AuxPoW producer migration must preserve `btc_parent_kind != 'unknown' OR pow_validates_btc_target`, and the read API must fail loudly if it observes an active `unknown` event with `pow_validates_btc_target = false`.

## Event Details

Every event detail includes source, child evidence, parent hash, event-parent kind, generic AuxPoW fields, `child_miner_pool`, pool identity objects, target flags, event lifecycle fields, and `pool_attributions`.
`child_height`, `child_block_hash`, `child_header_hex`, `child_block_time`, and `child_nbits` (8-character lowercase hex compact target when present) are nullable independent evidence fields; missing source evidence is JSON `null`, never zero or a placeholder.
Rows sort by `event_confirmed_at`, `source`, nullable `child_height` and `child_block_hash` (nulls last), then `id`.

| Field / Object | Contract |
|---|---|
| `child_displaced_at`, `child_displaced_by` | Child-side displacement pair: the epoch second a producer observed the child chain no longer carrying this event's block at `child_height`, and the replacement block's display-order hash. Always set or cleared together (`null` when no displacement has been observed; historical imports and Hathor archive-cache rows stay `null` until the height is observed again). Recorded by every live producer except RSK. Displacement is child-side only: a displaced event still contributes to all Bitcoin-side fields (`block`, proofs, source summaries, source health) and is distinct from `event_revoked_at`. |
| `pool_attributions` | Always present with `btc_parent` and `child_block` arrays of provenance objects (`namespace`, `match_kind`, `matched_value`, `pool`, `pool_identity`, `source`, `confidence` in `high`/`medium`/`low`, `details`). Source labels: `btc_pool_snapshot`, `btc_pool_snapshot_legacy_child_script`, `child_payout_registry` (Namecoin/Syscoin/Qbit child payout or Fractal/Hathor/Elastos child reward address resolved via `pool_identity`), `child_coinbase_output` (unresolved observed chain-native child address), `rsk_miner_registry`, and `rsk_rpc_miner`. Qbit payouts use `qbit_payout_address` / `payout_address` (curated F2Pool emits `high` confidence, SECPOOL/Luxor emit `medium`, unmapped `qb` addresses emit `child_coinbase_output` with an Unknown pool; matching tag/payout pools collapse to one child miner, conflicting known pools to Unknown). |
| `child_miner_pool` | Child-chain miner fact for that single event, derived from child-chain evidence (not inferred by clients from `pool_attributions.child_block`). |
| `rsk` | Required for `source = "auxpow:rsk"` (`null` otherwise). Fields: `block_hash`, `height` (equals `child_height`), `is_uncle`, `uncle_index` and `uncle_referencing_height` (both non-null iff `is_uncle = true`; `uncle_referencing_height` is the canonical RSK height that referenced the uncle, stored as `uncle_parent_height`), 40-char lowercase hex `miner_address` and `pool_identity.identifier` (without `0x`), `merge_mining_hash`, `merkle_proof_hex`, `coinbase_tail_hex`, and `proof_format` (`"rskj_rpc_opaque"`). RSK rows set generic parent/child coinbase fields, `aux_merkle_proof_hex`, and `pow_validates_child_target` to `null`. |
| Fractal & Hathor notes | Fractal rows populate `aux_merkle_proof_hex` from `getblockheader <hash> false true`, and populate child coinbase fields plus `fractal_reward_address` attributions only after full `getblock <hash> 0` capture/replay verifies the child transaction merkle root. Hathor rows use generic parent evidence fields and expose `hathor_reward_address` in `pool_attributions.child_block` from persisted `funds_graph` bytes; child coinbase fields and `aux_merkle_proof_hex` remain `null`. |

## Event Detail Field Availability

| Field family | Namecoin / Syscoin / Terracoin | Fractal Bitcoin | Qbit | Elastos | RSK | Hathor |
|---|---|---|---|---|---|---|
| Identity and source | populated from `merge_mining_event` | populated | populated | populated | populated | populated |
| Parent coinbase fields | populated when parsed | populated when parsed | populated when parsed | populated when parsed | `null` | populated when reconstructed |
| Child coinbase fields | populated when parsed | populated after full child block capture/replay | populated when full child body verifies | `null` | `null` | `null` |
| `aux_merkle_proof_hex` | raw classic CAuxPow bytes | raw Fractal CAuxPow bytes | raw Qbit extended-header proof bytes | raw Elastos AuxPoW bytes | `null` | `null` |
| `aux_proof` | decoded branches (`hash_block` present) when blob parses, else `null` | same | decoded branches (`hash_block` absent) when blob parses, else `null` | decoded branches (`hash_block` present) when blob parses, else `null` | `null` | `null` |
| `chain_id` / `slot_index` | `chain_id` cited-or-null (Namecoin=1, Terracoin=50); `slot_index` when blob parses and parent matches | `chain_id: null`; `slot_index` when blob parses and parent matches | `chain_id = 47`; `slot_index` when blob parses and parent matches | `chain_id = 1224`; `slot_index` when blob parses and parent matches | `null` | `null` |
| `rsk` | `null` | `null` | `null` | `null` | required object | `null` |
| `child_miner_pool` | known pool or `Unknown` | known pool or `Unknown` | known pool or `Unknown` | known pool or `Unknown` | known pool or `Unknown` | known pool or `Unknown` |
| `pool_attributions` | BTC parent matches, legacy child script tags, and chain-native child payout addresses | BTC parent matches and `fractal_reward_address` rows | BTC parent matches and verified Qbit child coinbase tag matches | BTC parent matches and `elastos_reward_address` / `elastos_minerinfo` rows | child `rsk_miner_address` when observed | BTC parent matches and `hathor_reward_address` rows from `funds_graph` |
| `pow_validates_btc_target` | populated | populated | populated | populated | populated | populated |
| `pow_validates_child_target` | populated | populated | populated | populated | `null` | `null` |
| `difficulty_epoch_ok` | `true` when Core placed the parent, else `null` | same | same | same | same | same |
| Lifecycle fields | populated | populated | populated | populated | populated | populated |

## Proof Lifecycle

AuxPoW proofs are derived from contributing `merge_mining_event` rows:

- `discovered_at`: earliest contributing event `discovered_at`;
- `confirmed_at`: earliest current contributing event `confirmed_at`;
- `revoked_at`: `null` while any contributing event remains active, otherwise the maximum contributing `revoked_at`;
- `revocation_reason`: null while live, optional summary when all events are revoked;
- `pow_validates_btc_target`: proof-level Bitcoin target validation.

Child target validation remains on event details.

## Errors

Error responses use:

```json
{
  "schema_version": "v1",
  "generated_at": 1779792000,
  "error": {
    "code": "invalid_query",
    "message": "from_height must be less than or equal to to_height",
    "details": {
      "from_height": 700010,
      "to_height": 700000
    }
  }
}
```

| Code | HTTP status | Details |
|---|---:|---|
| `invalid_query` | 400 | object keyed by invalid parameter |
| `invalid_hash` | 400 | `{ "hash": "<raw path value>" }` |
| `unsupported_source` | 400 | `{ "source": "<source code>" }` |
| `not_found` | 404 | `{ "hash": "<normalized lowercase hash>" }` |
| `range_too_large` | 422 | `{ "parameter": "...", "limit": n, "received": n }` |
| `backbone_unsynced` | 409 | `{ "from_height": n, "to_height": n, "first_missing_height": n, "missing_count": n, "partial_count": n, "conflict_count": 0, "action": "run sync-bitcoin-core" }` |
| `backbone_conflict` | 409 | `{ "from_height": n, "to_height": n, "first_missing_height": null, "missing_count": 0, "partial_count": 0, "conflict_count": n, "conflict_height": n, "conflict_reason": "...", "hashes": ["..."], "action": "run sync-bitcoin-core" }` |

Unexpected pool checkout, SQL, or response-building invariant failures return HTTP 500 with `code = "internal_error"`, message `internal server error`, and empty details.
The server logs the underlying error; clients should treat this as a server fault, not a query contract response.

Expected 4xx cases:

- `/api/v1/block/:hash`: `invalid_hash`, `not_found`.
- `/api/v1/tree`: `invalid_query`, `unsupported_source`, `range_too_large`, `backbone_unsynced`, `backbone_conflict`.
- `/api/v1/navigator/{target}`: `invalid_query`, `range_too_large`.
- `/api/v1/sources`: no query validation for the base endpoint.
- `/api/v1/competitions`: no query validation.
- `/api/v1/version`: no query validation and no database checkout.

## Bounds

- `/api/v1/tree`: at most 2,016 explicit requested Bitcoin heights.
  `at_height` and a resolved `at_time` request are single-height lookups unless `context=compact` is supplied, in which case the backend chooses a bounded range around the target and emits hidden edges for omitted canonical spans.
  The omitted-height default is a 16-block tip window.
  Responses return at most 500 nodes after deterministic context collapsing.
  Compact mode also caps event candidates, required candidates, and optional orphan candidates before component expansion, then shrinks or skips non-essential context so a valid covered target renders instead of surfacing `range_too_large`.
  Direct explicit ranges can still return `range_too_large` when they request too many non-collapsible nodes.
  When `include_unheighted=true`, `unheighted_from` / `unheighted_to` may span at most 31 UTC calendar days and return at most 250 null-height unknown nodes after filters.
- `/api/v1/navigator/{target}`: page size at most 2,000.
  Stale targets paginate over height-axis cursors; orphan targets paginate over time-axis cursors.
  Every target is presented newest-first, exposes opaque bidirectional cursors (`direction=older|newer`), and supports `anchor_hash` locate mode.
  `orphan` and `orphan-branch` are filtered by `classification` (default strict+weak).

Navigator cursors are target-bound and axis-bound.
Clients must treat them as opaque strings and send them back unchanged.
Clients narrow the query after `range_too_large`.

## Fixture Coverage

`fixtures/api/manifest.json` defines machine-readable fixture coverage and lists every `fixtures/api/*.json` file except `manifest.json` itself (error fixtures use `endpoint_family = "errors"`).
`tests/api_fixture_contract.rs` smoke-checks the manifest, parses every fixture, and verifies the minimal response envelope.
