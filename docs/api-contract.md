# API Contract

This document defines the release JSON contract for the merge-mining monitor UI.
Fixture examples live in `fixtures/api/`; fixture
coverage is listed by `fixtures/api/manifest.json`.

The current implementation supports Namecoin, RSK, Syscoin, Fractal Bitcoin,
Hathor, and Elastos capture. See `docs/data-model.md` for schema details,
`docs/architecture.md` for current code flow, and
`docs/tree-semantics.md` for implementation notes on deriving `/api/v1/tree`
and orphan navigator responses.

## Compatibility

All JSON responses use `schema_version = "v1"`. This project is pre-production,
so breaking contract changes are allowed before release when they simplify the
shipping API.

The `/api/v1` endpoints return JSON only. RSS feeds, pagination beyond the
documented keyset endpoints, CSV export, and SSE are out of scope. Runtime
health and readiness probes exist outside `/api/v1` and do not use the JSON
envelope.

## Health And Readiness

`GET /health` is a liveness probe. It returns `204 No Content` when the process
can answer HTTP, without checking out a database connection.

`GET /ready` is a readiness probe. It returns `204 No Content` only when the
process can check out a Postgres connection and complete `SELECT 1` inside the
bounded readiness timeout; otherwise it returns `503 Service Unavailable`.

Both probes send `Cache-Control: no-store`.

## Cache Headers

Successful `/api/v1/*` responses send short public cache headers suitable for a
reverse proxy or CDN. The JSON envelope includes a wall-clock `generated_at`, so
conditional JSON requests must be keyed on read-model freshness if they are added
later, not on a body hash.

The static frontend is buildless and not fingerprinted: `index.html` uses a
short edge TTL, hand-authored `www/` assets use moderate TTLs with
revalidation, and only the vendored versioned D3 file is marked immutable.
Error responses and probe responses are not cached.

## Common Envelope

Successful responses include:

- `schema_version`: currently `"v1"`;
- `generated_at`: epoch seconds;
- `query`: normalized query echo for endpoints with query parameters.

Endpoints that take no query parameters omit the `query` field:
`/api/v1/version`, `/api/v1/block/:hash` (a path hash only), and
`/api/v1/sources`.
`/api/v1/tree` and `/api/v1/navigator/{target}` have bounded query contracts
and therefore include a `query` echo.

Field names are snake_case. Hash values are 64 lowercase display/RPC hex
strings. Non-hash byte fields use a `_hex` suffix and are lowercase hex or
JSON `null`. Empty byte arrays are serialized as `null`, not `""`.

Epoch seconds are used for instant fields ending in `_at`, plus `header_time`,
`block.header.time`, and `child_block_time`. Calendar period fields use UTC
dates. Week periods use ISO weeks, Monday through Sunday.

## Version Metadata

`GET /api/v1/version` has no database dependency. It returns the running
application SemVer compiled from the Cargo workspace package version, plus the
release-note data parsed from `RELEASE_NOTES.md` at build time. The release-note
list is unbounded (every section, every item): the About dialog renders it in a
dedicated scrollable, per-release collapsible pane. The `truncated` and
`*_count` fields below are retained as a forward-compatible safety net and are
currently always consistent with `releases` / `items` (no truncation).

Response fields:

- `version`: SemVer string from root `Cargo.toml` `[workspace.package].version`.
- `release_notes.source`: currently `"RELEASE_NOTES.md"`.
- `release_notes.release_count`: total release-note sections present in
  `RELEASE_NOTES.md`.
- `release_notes.truncated`: true when `releases` omits older release-note
  sections; currently always `false`.
- `release_notes.releases`: every release-note section, newest first.
- `release_notes.releases[].version`: section version, for example
  `"0.2.0"`.
- `release_notes.releases[].date`: optional release date from headings like
  `## [0.2.0] - 2026-07-11`.
- `release_notes.releases[].items`: top-level bullet entries with wrapped
  Markdown lines flattened to text.
- `release_notes.releases[].item_count`: total top-level bullet count in that
  release-note section.
- `release_notes.releases[].truncated`: true when `items` omits later entries
  from that section; currently always `false`.

Example:

```json
{
  "schema_version": "v1",
  "generated_at": 1779792000,
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
          "Recover every Lyncoin Bitcoin-merge-mined header through height 260,499 and all 999,407 available SixEleven blocks. Bitcoin Core classified 11 Lyncoin parents and 7 SixEleven parents as canonical; neither chain produced a stale winner.",
          "Keep the recovery limits visible: VCash contributes 68 canonical mappings from archived explorer pages (not the VCash blockchain), while Doichain is a completed zero-row survey after 429,401 AuxPoW commitments produced no Bitcoin block winner.",
          "Make source IDs permanent and retire ID 32. Mazacoin is removed because its consensus source contains no AuxPoW implementation, so it is not a Bitcoin merge-mined source."
        ],
        "item_count": 3,
        "truncated": false
      },
      {
        "version": "0.1.0",
        "date": "2026-07-02",
        "items": [
          "Release the first source distribution of `merge-mining-monitor`.",
          "Include the Rust workspace, Postgres schema baseline, capture/reconciliation pipeline, read API, static frontend, fixtures, provenance manifests, and local operator tooling needed to build, test, and run the monitor."
        ],
        "item_count": 2,
        "truncated": false
      }
    ]
  }
}
```

## Source Codes

Source codes use `<kind>:<chain>` or `<kind>:<chain>:<instance>`.

Syntax:

```text
^[a-z][a-z0-9]*(-[a-z0-9]+)*(:[a-z][a-z0-9]*(-[a-z0-9]+)*){1,2}$
```

Reserved source codes:

- `auxpow:namecoin`
- `auxpow:rsk`
- `auxpow:syscoin`
- `auxpow:fractal`
- `auxpow:hathor`
- `auxpow:elastos`
- `live-chaintip:bitcoin:core`

Reserved historical (recovered) AuxPoW source codes (defined in the Source
Lifecycle Registry with lifecycle `historical`; no live producer):

- `auxpow:argentum`
- `auxpow:bitcoin-vault`
- `auxpow:bitmark`
- `auxpow:coiledcoin`
- `auxpow:crown`
- `auxpow:devcoin`
- `auxpow:emercoin`
- `auxpow:geistgeld`
- `auxpow:groupcoin`
- `auxpow:huntercoin`
- `auxpow:i0coin`
- `auxpow:ixcoin`
- `auxpow:lyncoin`
- `auxpow:myriadcoin`
- `auxpow:sixeleven`
- `auxpow:terracoin`
- `auxpow:unobtanium`
- `auxpow:xaya`
- `auxpow:elcash`

Reserved partial recovered AuxPoW source codes (lifecycle `partial`; evidence
is ingestible, but the complete child blockchain remains unavailable):

- `auxpow:vcash`

Reserved surveyed AuxPoW source codes (lifecycle `surveyed`; child-chain data
was recovered and reviewed, but no admissible Bitcoin evidence rows were found):

- `auxpow:doichain`

Reserved catalogued (not recovered) AuxPoW source codes (defined in the Source
Lifecycle Registry with lifecycle `catalogued`; chains known to have
BTC-merge-mined but with no recovered chain data, hence no producer, no
poll-cursor, no source_health row, and zero evidence counts):

- `auxpow:jax-network`
- `auxpow:blast`
- `auxpow:fusioncoin`
- `auxpow:jincoin`
- `auxpow:bitcoin-stash`

Source ids are permanent and never reused, so retired ids remain as gaps. Id 32
is the first such gap: Mazacoin was removed after its consensus source showed no
AuxPoW implementation. No Mazacoin row or source code remains in the API
registry.

The `source` query parameter accepts comma-delimited source codes. Response
echoes use `query.sources`, sorted and deduplicated. Normalization trims ASCII
whitespace, rejects empty members as `invalid_query`, rejects unknown enum
members as `invalid_query`, rejects syntactically valid but unregistered source
codes as `unsupported_source`, and sorts the resulting arrays
lexicographically. When no source filter is provided, `query.sources` is an
empty array meaning no source filter.

Malformed source syntax returns `invalid_query`. Well-formed but unregistered
source codes return `unsupported_source`. Source codes are case-sensitive and
must be lowercase.

The `source` registry object has `id`, `code`, `kind`, `chain`, `instance`,
`created_at`, `last_seen_at`, `status`, `sync`, and `counts`. A
nested `source_ref` uses only `id`, `code`, `kind`, `chain`, and `instance`.
`last_seen_at` is a derived API field: for AuxPoW sources it is
`source_health.last_event_seen`; live-chaintip freshness is carried by `sync`
progress and leaves `last_seen_at` null.

Source enums:

- `kind`: `auxpow`, `live-chaintip`
- `status`: `fresh`, `stale`, `not_started`

`source.chain` is non-null in v1 because source-code syntax requires a chain
segment. `live-chaintip:bitcoin:core` is the Bitcoin Core classifier source. If
another instance is ever chosen, update `fixtures/api/sources.json`, every
fixture reference, this document, the manifest, and the source seed migration
together.

Historical `auxpow:<chain>` sources are populated by the operator
`import-dataset` command and then appear through the same block/proof/source
summary projections as live AuxPoW evidence. The API does not expose a separate
dataset row schema for them.

## Response Ordering

Response arrays use stable ordering:

- `tree.nodes`: ascending `height`, then lexicographic `hash`; null heights are
  rendered after height-backed nodes and then sorted by hash. EXCEPTION: in
  anchor-component mode (`unheighted_anchor`) the placed orphan members all have
  null `height` and are instead ordered by their derived `placement_height` then
  stored hash (the layout ranks them by `placement_height`); their root-to-tip
  order is also carried in `branches[].member_hashes`.
- `tree.edges`: by the index of `to_hash` in sorted `tree.nodes`, then
  lexicographic `from_hash`.
- `tree.branches`: ascending `btc_height_min`, then lexicographic `root_hash`.
  Orphan branches sort among stale branches by their placement `btc_height_min`.
- `tree.branches[].member_hashes`: root-to-tip order.
- `block.proofs`: ascending `source.code`, then smallest
  `evidence.contributing_event_ids` value.
- `block.event_details`: `event_confirmed_at`, `source`, `child_height`,
  `child_block_hash`, then `id`.
- `sources.sources`: ascending `id`.

## Core Entities

`merge_mining_event`
: One AuxPoW child-chain block that commits to a Bitcoin parent header.

`rsk_evidence`
: The API view of `rsk_merge_mining_evidence`, exposed as `event_details[].rsk`
  for `auxpow:rsk` rows and `null` for other rows.

`event_pool_attribution`
: Event-level pool match provenance exposed as
  `event_details[].pool_attributions`, split into `btc_parent` and
  `child_block` arrays.

`bitcoin_miner_pool`
: The miner/pool resolved from Bitcoin block coinbase evidence for a Bitcoin
  parent. It does not use child-chain miner evidence.

`child_miner_pool`
: The miner/pool resolved for one child-chain event from that child chain's
  block evidence. Unknown is represented directly when the child event does not
  resolve to one known pool.

`display_miner_pool`
: The best-available miner/pool for display labelling, a presentation-layer
  fallback that never overrides the strict `bitcoin_miner_pool` fact. It equals
  `bitcoin_miner_pool` when the Bitcoin coinbase miner is known. When the strict
  Bitcoin coinbase miner is Unknown (for example an RSK-only stale block whose
  compressed AuxPoW proof carries no recoverable Bitcoin coinbase), it falls
  back to the single distinct known child miner pool across the parent's active
  merge-mining events (the merge-miner of the child block is the miner of the
  Bitcoin parent). Zero or conflicting known child pools resolve to Unknown. It
  is chain-agnostic (any merge-mined child chain can supply it) and is present
  on tree nodes and on the block detail.

`display_miner_basis`
: How `display_miner_pool` was resolved: `bitcoin_coinbase` (strict coinbase
  miner) | `child_inferred` (single distinct known child miner pool) |
  `unknown` (no strict pool and zero or conflicting child pools). Near/unknown
  direct-projected parents are not validated Bitcoin blocks and are never
  child-inferred.

`block`
: The API representation of one Bitcoin parent header. For `near` and
  `unknown`, the API reads directly from active `merge_mining_event` rows. For
  `canonical` and `stale`, later read-model work reads from the reconciled
  block/proof/observation model.

`parent_kind`
: One of `near`, `unknown`, `canonical`, or `stale`.

`near`
: Parent header satisfies the child-chain AuxPoW target but fails Bitcoin
  target.

`unknown`
: Parent header passes Bitcoin target, but Bitcoin-chain proof has not yet
  classified it as canonical or stale.

`canonical`
: Parent header is on the active Bitcoin chain.

`stale`
: Parent header is Bitcoin-valid but not on the active chain.

`stale_branch`
: One or more stale blocks linked by parent-child edges, with branch depth,
  member hashes, and canonical competitor references.

## Pool Objects

Known pools expose identity only:

```json
{"id": 7, "slug": "antpool", "name": "AntPool", "known": true}
```

Unknown pools use:

```json
{"id": null, "slug": null, "name": "Unknown", "known": false}
```

Known slugs must match lowercase letters, digits, and hyphens. RSK
miner-address attribution remains visible in `event_details[].rsk.pool_identity`
and is not treated as identical to Namecoin parent coinbase attribution.

Event attribution rows expose `namespace`, `match_kind`, `matched_value`,
`pool`, nullable `pool_identity`, `source`, `confidence`, and `details`. Known
attributions use the same pool object as the legacy `pools` field;
unresolved observed identifiers use the Unknown pool object with
`pool_identity: null`.

## Counter Semantics

`source_summary.sources` is the sorted unique source-code array that contributes
active evidence for the parent header in the response. `/api/v1/tree` may also
emit canonical context nodes with `sources: []` when source filters remove all
evidence but the node is still needed to connect visible structure; those nodes
are context, not evidence, and do not satisfy `min_sources`.

`source_summary.distinct_sources` is `COUNT(DISTINCT source_id)` across active
evidence for that parent header. Direct `near` and `unknown` projections compute
that distinct source count from non-revoked `merge_mining_event` rows, so two
Namecoin events for one parent still produce `distinct_sources = 1`. Canonical
and stale projections count non-revoked `attestation_proof.source_id` values
and add the synthetic Bitcoin source when the `block` row is Core-attested or
live-observed.

`source_summary.auxpow_chain_count` counts distinct AuxPoW child chains among
active evidence. Namecoin plus RSK for the same parent gives `2`.

`source_summary.live_observed` is true when the parent header is represented by
live Bitcoin Core evidence. Core-only backbone rows carry that provenance on
the canonical `block` row and expose it through `block.source_summary`.

`source_summary.pow_validates_btc_target` describes the Bitcoin parent header,
not the child-chain target. Child-chain target validation stays on
`event_details[].pow_validates_child_target`.

## Endpoint List

### `/api/v1/tree`

Purpose: project the windowed Bitcoin header tree from the synced Bitcoin Core
backbone and active merge-mining evidence.

Query:

```text
GET /api/v1/tree?at_height&at_time&context=compact&from_height&to_height&kinds=near,unknown,canonical,stale&classification=strict_btc_orphan,weak_btc_orphan&source=auxpow:namecoin,auxpow:rsk,auxpow:syscoin,auxpow:fractal,auxpow:hathor&include_near=true&include_unheighted=true&unheighted_from=2026-05-01&unheighted_to=2026-05-26&min_sources=1
```

`at_height`, `at_time`, `from_height` / `to_height`, and `unheighted_anchor` are
mutually exclusive tree lookup modes. `at_height` selects one exact Bitcoin
height and resolves to the existing single-height tree window `H..H`. `at_time`
is a UTC RFC3339 timestamp in `YYYY-MM-DDTHH:MM:SSZ` form. It resolves to the
complete canonical block with the nearest `btc_header_time` at or before the
timestamp, using deterministic ordering
`btc_header_time DESC, btc_height DESC, btc_header_hash ASC`, then renders that
row's height as a single-height tree. If no complete canonical block exists at
or before the timestamp, the response is an empty non-tip lookup window with
`empty_reason = "no_complete_canonical_at_or_before_time"`.

`context` defaults to `"exact"` for direct API callers. `context=compact` is
accepted only with `at_height` or `at_time`; it returns the requested target plus
nearby blocks that explain relevant forks. It does not accept raw
caller-supplied ranges: `context=compact` with `from_height` / `to_height`,
`unheighted_anchor`, `include_unheighted=true`, `unheighted_from`, or
`unheighted_to` is `invalid_query`. The manual UI always uses compact context
for Height and Date/Time lookups; its shared links store only frontend
`tree_height` / `tree_time` parameters and map them to backend `at_height` /
`at_time` plus `context=compact`.

Compact context represents uneventful canonical runs as `hidden` edges while
preserving the requested target, stale rows, canonical competitors, and
branch/root/tip context. Backbone integrity guarantees apply inside the rendered
window exactly as for explicit ranges (`backbone_unsynced` / `backbone_conflict`).
Compact mode does not automatically attach BTC orphan branches: use
`/api/v1/navigator/orphan` to find orphan candidates and
`/api/v1/tree?unheighted_anchor=<hash>` to render the placed orphan fork. The
adaptive-shrinking mechanics are described in `docs/tree-semantics.md`.

`from_height` and `to_height` are legacy/internal API range bounds and remain
optional as a pair. Supplying exactly one is `invalid_query`. They are available
for direct API callers, but the frontend no longer exposes or preserves range
state from manual controls. The manual UI writes `tree_height` and `tree_time`
parameters that map to backend `at_height` and `at_time`. Backend-generated
navigator links may use the frontend-only marker `tree_window=generated` with
`tree_from` / `tree_to`; the frontend then calls this endpoint with bounded
`from_height` / `to_height`.
When all lookup modes are omitted, the backend defaults to a tip-focused 16-block
canonical Bitcoin window ending at the highest complete covered canonical
backbone window, not raw `MAX(block.btc_height)`.
The request path does not fetch Bitcoin Core. Every canonical height in the
resolved tree window must already be present as a single `kind = 'canonical'`
`block` row with `btc_coinbase_status = 'complete'`, normally populated by
`sync-bitcoin-core`. This requirement is independent of `kinds` and
`classification` filters: stale and orphan-focused projections still use
canonical rows as the displayed Bitcoin spine and branch attachment context. If
the window is missing rows or has incomplete coinbase evidence, the endpoint
returns HTTP 409 `backbone_unsynced`. If duplicate canonical rows or broken
prev-hash links are present, it returns HTTP 409 `backbone_conflict`. If the DB
has no complete canonical backbone height, the response is an empty tree with
nullable window bounds and
`empty_reason = "no_canonical_tip"`.
Existing deployments can have canonical rows that predate Core coinbase
persistence, or rows where a best-effort coinbase fetch failed. Those rows are
treated as incomplete backbone rows until `sync-bitcoin-core` repairs the
affected heights. A failed coinbase fetch does not stop later hash-linked
heights in the requested sync page from being written, but the
contiguous-complete cursor remains pinned before the failed height. Clients
should show the 409 action rather than retrying the tree request in a loop.
`first_missing_height` in `backbone_unsynced` means the first not-fully-covered
height in the requested window; that includes absent canonical rows and rows
with incomplete or failed Core coinbase evidence.

`unheighted_anchor` selects the anchor-centered BTC-orphan view (the orphan-block
navigator landing): a 64-character hex hash of a PoW-valid `unknown` block whose
`btc_orphan_class` is in the active `classification` set. It is mutually
exclusive with `at_height`, `at_time`, `from_height` / `to_height`, and with the
`include_unheighted` date-window mode (supplying either combination is
`invalid_query`), and an anchor hash that is not an existing PoW-valid orphan in
that set is `not_found` (so the navigator never lands on a pending or excluded
block in the default strict+weak view). Anchor mode renders the whole proven
prev_hash-linked orphan component that contains the anchor, placed against the
canonical `±16` context window read from local canonical rows only (it does not
fetch Bitcoin Core during the request). The `kinds`, `source`, and `min_sources`
filters are ignored (only `classification` decides eligibility), so the
canonical blocks used for placement are never filtered away and the jump always
lands. Run
`sync-bitcoin-core` for historical heights before relying on dense orphan
placement. The query echo reports `window_mode = "unheighted_anchor"` and echoes
`unheighted_anchor`; the `window` reports the `±16` placement bounds in
`btc_height_min` / `btc_height_max` (null `tip_height`,
`defaulted_to_tip = false`, `empty_reason = null`). When no placement height can
be derived, or a placement height was derived but the window holds no canonical
block to attach to, anchor mode falls back to a flat time-ordered strip of the
anchor plus its nearest-in-time orphan neighbors (250-node cap, null window
bounds). The placement-height derivation and `orphan` / `orphan_approx` edge
attachment rules are described in `docs/tree-semantics.md`.

`classification` is a comma-delimited filter over the orphan classes
(`strict_btc_orphan`, `weak_btc_orphan`, `btc_stale_excluded`, `pending`),
defaulting to `strict_btc_orphan,weak_btc_orphan`. It is a SEPARATE parameter from
`kinds` (the structural parent kinds): the orphan classes are the derived
refinement of `kind='unknown'` (see `block.btc_orphan_class`) and are never
smuggled through `kinds`. It scopes the unknown population in anchor mode and in
the `include_unheighted` date-window mode; the height-window and tip modes select
no time-located unknown nodes, so it is inert there. An unknown member is
`invalid_query`.

Response fields:

- `query`, including `include_near`, `include_unheighted`, and any
  `unheighted_from` / `unheighted_to` bounds or `unheighted_anchor`. The query
  echo includes `from_height`, `to_height`, `at_height`, `at_time`, and
  `window_mode = "tip"` for omitted lookup bounds, `"explicit"` for legacy
  caller-supplied range bounds, `"height"` for exact-height lookup, `"time"` for
  datetime lookup, and `"unheighted_anchor"` for anchor mode. It also echoes
  `context = "exact"` or `"compact"`;
- `window` with nullable `btc_height_min`, nullable `btc_height_max`, nullable
  `tip_height`, `defaulted_to_tip`, nullable `empty_reason`,
  and `hidden_linear_block_count`;
- `nodes[]` sorted by height then hash (anchor-component orphan members, all
  null-height, by `placement_height` then hash);
- `edges[]` including `hidden` edges for collapsed canonical context;
- `branches[]` for stale-branch summaries;
- `legend` with `kinds` and `edge_kinds`.

Each tree node has:

- `id`: response-local integer layout ID;
- `hash`, `height`, `kind`, `prev_id`, `prev_hash`;
- `btc_orphan_class`: the derived refinement of `kind='unknown'`
  (`strict_btc_orphan` / `weak_btc_orphan` / `btc_stale_excluded`), or `null` for
  canonical/stale nodes and for pending/never-Core-checked unknowns. `kind` stays
  the structural evidence state; `btc_orphan_class` is the refinement the UI
  renders. It is a per-node detail field, not a navigable bucket;
- `pool`;
- `source_summary`;
- `child_chain_evidence[]`, grouped by active AuxPoW `source` and
  `child_chain`, with event count and child-height min/max for child chains
  that referred to this Bitcoin header;
- `proof_state` with `has_auxpow_evidence` and `has_live_observation`;
- `competition`, null except for stale nodes with a paired competitor;
- `branch`, null for non-stale/non-orphan-branch nodes and `{ "branch_id": "<id>" }`
  for stale members and for multi-block orphan component members;
- `placement_height` (optional) and `placement_approx` (optional): the orphan
  fork placement, present on placed orphan nodes in anchor mode only (both
  omitted elsewhere). `placement_height` is the derived layout height the orphan
  dangles at (its stored `btc_height` stays `null`); `placement_approx` is `true`
  when that height is an approximation rather than a validated strict BIP34
  height, and the UI prefixes the label with `~`. They are layout hints, not
  stored block columns, and are not exposed on `/api/v1/block/:hash`. See
  `docs/tree-semantics.md` for how placement is derived;

`height` is `null` for direct-projected near and unknown nodes because current
AuxPoW producers do not prove Bitcoin parent height. The height window selects
height-backed canonical/stale/context nodes
only. Null-height nodes are
excluded unless `include_unheighted = true`; in that mode
`unheighted_from` and `unheighted_to` are required UTC date bounds applied to
the parent header time carried by active direct evidence rows, and the
`classification` filter (default strict+weak) further scopes the unheighted
unknown rows to the requested orphan classes. A block-backed unknown is filtered
by its `block.btc_orphan_class`; a direct-evidence unknown with no `block`
read-model row carries no class and is treated as `pending`, so it appears only
when `classification` includes `pending`. These bounds are
inclusive, capped to 31 UTC calendar days, and the endpoint returns
`range_too_large` if more than 250 unheighted nodes survive filters. Null-height
nodes sort after height-backed nodes, then by hash.

Clients must derive hidden context from `edges[]`. A hidden
edge may make `node.prev_hash` differ from the visible `prev_id` node's hash.
`edges[]` is the drawn-edge graph: a node may carry `prev_id`/`prev_hash` for a
visible predecessor without a matching `edges[]` entry when that transition has
no drawable edge kind (an unknown/near child, or a canonical child with a
non-canonical predecessor).

Edge fields are `from_hash`, `to_hash`, and `edge_kind`. Allowed edge kinds:

| `edge_kind` | Meaning | Extra fields |
|---|---|---|
| `canonical` | visible canonical parent-child edge | none |
| `stale_entry` | visible edge entering a stale branch root | none |
| `stale` | visible stale-member to stale-member edge | none |
| `hidden` | collapsed canonical context | `hidden_count`, greater than zero |
| `orphan` | anchor-mode fork edge from a verified strict root to its canonical predecessor (`placement_height - 1`), or a proven orphan-to-orphan prev_hash link inside a multi-block component | none |
| `orphan_approx` | anchor-mode fork edge for an approximate placement (weak/excluded/pending root, or a strict root whose predecessor is absent/mismatched), to the nearest in-window canonical | none |

Branch IDs use `stale-<btc_height_min>-<root_hash>` for stale branches and
`orphan-<root_hash>` for orphan branches. Orphan branch `root_hash` is the stable
component identity (the oldest member whose prev_hash is not in the component);
orphans have no stable height, so the branch id contains no height segment.
`tree.branches` for orphan branches use the same `TreeBranch` object
(`branch_id`, `root_hash`, `tip_hash`, `member_hashes`, `btc_height_min`,
`btc_height_max`) with height bounds set to the placement heights and
`canonical_competitor_hashes` empty (orphans have no competition).

Tree reduction is deterministic: a given range, filter set, and dataset always
yield the same response. Only the fields in the response are guaranteed:
collapsed canonical context is represented by `hidden` edges carrying
`hidden_count` and summarized by `window.hidden_linear_block_count`; clients
should read `edges[]` to find hidden context; and the response returns at most
500 nodes, yielding `range_too_large` (HTTP 422) when the required nodes cannot
fit within that cap. The specific reduction procedure the backend uses is
documented as an implementation note in `docs/tree-semantics.md`, not part of
the API contract.

### `/api/v1/block/:hash`

Purpose: fetch details for a selected parent header.

The hash path parameter may be uppercase or mixed-case when valid hex. The API
normalizes valid hashes to lowercase before lookup. Invalid hash syntax returns
`invalid_hash`.

Response fields:

- `block` with `hash`, nullable `height`, `kind`, nullable `btc_orphan_class`
  (the derived refinement of `kind='unknown'`: `strict_btc_orphan` /
  `weak_btc_orphan` / `btc_stale_excluded`, else `null`), nullable
  `coinbase_tag` (for Core-attested canonical rows with stored Core coinbase
  evidence, extracted from `block.btc_coinbase_script`; otherwise extracted
  from the representative Bitcoin coinbase script in
  `commitment.parent_coinbase_script_hex`; else `null`), `header`,
  `bitcoin_miner_pool`, `display_miner_pool` + `display_miner_basis` (the
  best-available display miner; see the glossary), and `source_summary`.
  Direct-projected near/unknown blocks (no read-model row) carry
  `btc_orphan_class: null`;
- `proofs`;
- `event_details`;
- `competition`;
- `stale_branch`;
- `commitment`.

Core-only backbone rows represent Core provenance through the canonical `block`
row and `block.source_summary`; there is no separate per-header observation
payload.

`commitment` is the parent-level AuxPoW merge-mining commitment, or `null`
(matching `competition` / `stale_branch`) when the block has no recognized
AuxPoW-format event. When present it carries `format`
(`namecoin-aux` / `rsk-opaque` / `hathor-rfc0006`, chosen by family priority:
any Namecoin-family event wins, else RSK, else Hathor), `parent_coinbase_txid`,
`parent_coinbase_script_hex`, and `marker`. `marker` is non-null only for a
Namecoin-family parent whose coinbase scriptSig yields a `0xfabe6d6d` marker; it
carries `magic_present`, `aux_merkle_root`, `merkle_size`, and `merkle_nonce`.
`aux_merkle_root` is a hash-like field in the standard reversed/display order
(the reverse of its raw scriptSig bytes), like every other API hash. RSK and
Hathor are never scanned for the marker, so their commitment is format-only with
a null marker and null coinbase fields. The marker is decoded in Rust from the
already-stored parent coinbase bytes; no `aux_target` value is surfaced (only the
`event_details[].pow_validates_child_target` check), and that value remains a
documented follow-up.

For Core-attested canonical blocks with stored Bitcoin Core coinbase evidence,
`block.coinbase_tag` is derived from the `block.btc_coinbase_script` on the
Bitcoin block row, even when child-chain event details exist. This Core path
prefers the BTC pool resolver's matched coinbase tag and then falls back to the
normalized printable run with surrounding `/` delimiters trimmed. When no Core
tag is available, event-backed blocks derive `block.coinbase_tag` from the same
commitment representative row as `commitment.parent_coinbase_script_hex`, not
from an arbitrary contributing event. The raw extraction keeps printable ASCII
bytes (`0x20..=0x7e`), splits on every other byte, trims kept runs of four or
more characters, and joins multiple runs with a single space. RSK and Hathor
commitments therefore report `coinbase_tag: null` because their representative
commitment intentionally has no recoverable Bitcoin coinbase script.

`event_details[]` additionally carry `chain_id` (the reference AuxPoW chain id;
cite-or-null, Namecoin = 1, `null` for not-yet-cited chains and non-Namecoin
families) and `slot_index` (this chain's `nChainIndex` slot in the parent's aux
merkle tree, decoded from the stored CAuxPow blob). Both are Namecoin-family-only
and otherwise `null`; `slot_index` is additionally gated on the blob's embedded
parent header matching the event's `btc_parent_header_hash`, so a
parseable-but-mismatched blob never surfaces a foreign slot.

`event_details[].aux_proof` is the decoded CAuxPow merkle proof (the human
breakdown the UI renders in place of the opaque proof-byte hex): the redundant
`hash_block` (`CAuxPow::hashBlock`, conventionally all-zero and not the real
parent hash) plus `coinbase_branch` (coinbase txid up to the parent transaction
merkle root) and `blockchain_branch` (aux block hash up to the marker's
`aux_merkle_root`), each with an `index` (side-mask; the `blockchain_branch`
index is the slot) and display-order `siblings`. Same gate
as `slot_index` (Namecoin-family, parent-header match), `null` otherwise. The
raw `aux_merkle_proof_hex` stays in the payload for programmatic use.

For direct `near` and `unknown` projections, group active non-revoked
`merge_mining_event` rows by `btc_parent_header_hash`. Current kind precedence
is canonical/stale read model first, then any `unknown` event, then any `near`
event.

`proofs[].evidence.contributing_event_ids` is the durable proof evidence for
AuxPoW-derived proofs. `/api/v1/block/:hash` hydrates those IDs into sibling
`event_details[]` for UI convenience. Canonical and stale block fixtures must
have exact set equality between proof IDs and hydrated event detail IDs.

`proofs[].source` is a `source_ref` object. `event_details[].source` is a
source-code string.

Competition fields:

- `btc_height`;
- `stale_hash`;
- `canonical_hash`;
- `stale_bitcoin_miner_pool`;
- `canonical_bitcoin_miner_pool`;
- `header_time_delta_s`;
- `propagation_delta_s`.

`propagation_delta_s` is nullable while the underlying observation data is
absent.

Stale branch positions are `root`, `interior`, `tip`, and `root_and_tip`.
`root_and_tip` requires `depth = 1`; `root` has no parent and at least one child;
`tip` has a parent and no children.

### `/api/v1/navigator/{target}`

```
GET /api/v1/navigator/{target}?limit&cursor&direction&anchor_hash&classification
```

Purpose: one bounded navigation API for the header-tree controls. The four
targets are:

| Target | Axis | Item scope | View |
|---|---|---|---|
| `stale` | `height` | proven stale blocks | `tree_window` |
| `stale-branch` | `height` | current multi-block stale branches | `tree_window` |
| `orphan` | `time` | BTC-orphan blocks | `unheighted_anchor` |
| `orphan-branch` | `time` | multi-block BTC-orphan branches | `unheighted_anchor` |

All targets use the same modes:

- latest mode: omit `cursor`, `direction`, and `anchor_hash`; returns the newest
  item(s) for the target.
- page mode: pass an opaque response `cursor` plus `direction=older|newer`;
  returns the next page on that side of the cursor.
- anchor mode: pass `anchor_hash=<64 lowercase display hex>`; returns the item
  containing that hash. For branch targets, interior member hashes resolve to
  their owning branch, not just the root.

Query parameters:

- `limit`: optional page size, default `1`, maximum `2000`; `limit=0` is
  `invalid_query`.
- `cursor`: opaque hex-encoded server cursor from a previous navigator item or
  `next_cursor` / `prev_cursor`. It is target-bound and axis-bound; a cursor from
  another target is `invalid_query`.
- `direction`: required with `cursor`, and only valid with `cursor`.
- `anchor_hash`: mutually exclusive with `cursor`.
- `classification`: accepted only by `orphan` and `orphan-branch`, defaulting to
  `strict_btc_orphan,weak_btc_orphan`. The valid classes are
  `strict_btc_orphan`, `weak_btc_orphan`, `btc_stale_excluded`, and `pending`.

Supplying any other parameter, mixing `cursor` with `anchor_hash`, supplying only
one of `cursor` / `direction`, or passing `classification` to a stale target is
`invalid_query`.

Response fields:

- `query`: normalized target, mode, cursor, direction, anchor hash,
  classification, and limit.
- `target`: the target string from the path.
- `items[]`: target-specific navigator items, newest-first regardless of whether
  the page was reached by an older or newer cursor.
- `total`: the global count for the active target and classification filter.
- `facets`: `{}` except for `orphan`, where it includes
  `orphan_classes: { strict, weak, excluded, pending }`, the full per-class
  breakdown over PoW-valid unknown rows independent of the active filter.
- `next_cursor`: opaque cursor for the next older page, or `null` at the oldest
  edge.
- `prev_cursor`: opaque cursor for the next newer page, or `null` at the newest
  edge.

Each `items[]` entry has this format:

- `id`: stable target-local id.
- `kind`: one of the target strings.
- `primary_hash`: display/RPC hash for the block or branch root.
- `label`: display label for the UI.
- `position`: `{ axis, min, max }`; stale targets use Bitcoin height, orphan
  targets use `btc_header_time`.
- `cursor`: opaque item cursor. Clients must send it back as-is.
- `branch`: `null` for single-block targets, or
  `{ branch_id, root_hash, tip_hashes, depth }` for branch targets.
- `orphan`: `null` except for the `orphan` target, where it is
  `{ btc_orphan_class }` (`null` only when `pending` is in the active filter).
- `view`: either `null`, `{ mode: "tree_window", target_height, tree_from,
  tree_to, select_hash, center_hash }`, or `{ mode: "unheighted_anchor",
  anchor_hash, select_hash, center_hash }`.
- `view_error`: `null` unless the backend cannot advertise a safe tree-window
  view, in which case it is `{ code, target_height, message, action }`.

Ordering is stable and target-specific:

- `stale`: `btc_height` descending, then stored stale hash bytes ascending.
- `stale-branch`: `btc_height_max` descending, then `btc_height_min` descending,
  then stored root hash bytes ascending.
- `orphan`: `btc_header_time` descending, then stored header hash bytes
  descending.
- `orphan-branch`: `btc_header_time_max` descending, then
  `btc_header_time_min` descending, then stored root hash bytes ascending.

Single-block stale items round-trip to `/api/v1/block/:hash` and to tree node
hashes. Stale branch items include only current components whose stale members
have a derivable canonical competitor, with `depth >= 2`; one-block stale
branches remain single `stale` items and block detail. Orphan branch items
include only proven prev_hash-linked orphan components with `depth >= 2`;
one-block orphan components remain single `orphan` items. Branch endpoints
intentionally omit member lists, pool detail, and placement heights; the tree
and `/api/v1/block/:hash` endpoints expose those when a branch is selected.

The orphan classes (`btc_orphan_class`) appear across the navigator, tree,
block, and sources endpoints as documented per endpoint above. See
`docs/tree-semantics.md` for where BTC orphan classes appear.

### `/api/v1/sources`

Purpose: drive source filters and health panels.

Response fields:

- `sources[]` sorted by `id`.

`status` describes evidence freshness only. Values are `fresh`, `stale`, or
`not_started`; `not_started` requires derived `last_seen_at = null`.

`sync` describes source capture progress separately from evidence freshness:

- `sync.mode`: `live`, `bitcoin-core-backbone`, `historical`, `partial`,
  `surveyed`, `catalogued`, or `unknown`.
- `sync.state`: `live`, `catching_up`, `stale`, `error`, `not_started`,
  `historical`, `partial`, `surveyed`, `catalogued`, or `unknown`.
- `sync.progress_height`: the generalized progress height for live AuxPoW
  producers or the Bitcoin Core contiguous backbone, else JSON `null`.
- `sync.progress_updated_at`: Unix seconds for the progress row's last seed or
  advancement time, else JSON `null`.
- `sync.target_height`: the observed AuxPoW chain tip for live AuxPoW sources
  or the current Bitcoin Core backbone target tip height, else JSON `null`.
- `sync.latest_evidence_at`: latest AuxPoW evidence time for live AuxPoW
  sources, else JSON `null`.
- `sync.error_code`: latest Bitcoin Core backbone error code, else JSON `null`.
- `sync.error_height`: Bitcoin height associated with `sync.error_code`, else
  JSON `null`.

`progress_height` and `progress_updated_at` are both present or both null. Live
AuxPoW sources use a capture-specific 1-hour cursor-age window first: a missing
cursor is `not_started`, an old cursor is `stale`, a fresh cursor below a known
`target_height` is `catching_up`, and a fresh cursor with no target or at or
above target is `live`. A fresh AuxPoW source can transiently report
`catching_up` over its configured reorg window after the initial tip-anchored
seed. Historical, partial, surveyed, and catalogued sources report their
lifecycle token as both mode and state with null progress fields.
Bitcoin Core live-chaintip sources report
`bitcoin-core-backbone` from `bitcoin_core_sync_state`: no row is `not_started`,
`last_error_code` is `error`, a row older than one hour is `stale`, a fresh row
with no target or no real contiguous progress is `not_started`, a fresh row
below target is `catching_up`, and a fresh row at or beyond target is `live`.
`poll_cursor.updated_at` is NOT a producer heartbeat; it is the time that cursor
row was seeded or advanced. A stale capture state means the cursor has not
advanced within the window; a caught-up AuxPoW source with no recent cursor
advance can age into `stale`, and that does not, by itself, prove that the
poller is down. `latest_evidence_at` is separate from cursor progress: it is
the newest AuxPoW evidence timestamp observed for that source and can lag or
lead the cursor update time depending on source behavior and ingest history.
Future live non-AuxPoW source kinds must define their own sync semantics before
they should be registered.

Per-source counts (`counts.events`, the distinct-parent `near`/`unknown`/
`canonical`/`stale`, and the strict/weak orphan sub-counts `strict_orphan`/
`weak_orphan`) are served from the precomputed `source_health` table
(O(sources)), not recomputed per request. This endpoint FAILS CLOSED (the shared
`internal_error` envelope, HTTP 500) rather than returning zeros when the
`source_health` table has not yet been built: the operator must run
`reconcile-read-model --rebuild-source-health` after first creating the
`source_health` table (the baseline defaults `source_health_ready` to false so the
endpoint fails closed until the rebuild populates the counters) and after bulk
backfills before `/sources` is trusted. The
same internal-error guard fires if any active unknown parent fails its Bitcoin
target (a corrupt invariant).

## Nullability By Parent Kind

| Field | near | unknown | canonical | stale |
|---|---|---|---|---|
| `block.hash` | required | required | required | required |
| `block.height` | null (direct-projected) | null (direct-projected) | required | required |
| `block.kind` | required | required | required | required |
| `block.coinbase_tag` | null or printable tag | null or printable tag | null or printable tag | null or printable tag |
| `block.header` | required | required | required | required |
| `block.bitcoin_miner_pool` | required | required | required | required |
| `block.display_miner_pool` | required (Unknown) | required | required | required |
| `block.display_miner_basis` | `unknown` | one of `bitcoin_coinbase` / `child_inferred` / `unknown` | one of `bitcoin_coinbase` / `child_inferred` / `unknown` | one of `bitcoin_coinbase` / `child_inferred` / `unknown` |
| `block.source_summary.sources` | sorted non-empty array | sorted non-empty array | sorted non-empty array for evidence; may be empty for `/tree` canonical context | sorted non-empty array |
| `block.source_summary.distinct_sources` | required | required | required | required |
| `block.source_summary.auxpow_chain_count` | required | required | required | required |
| `block.source_summary.live_observed` | false | false unless observed | required | required |
| `block.source_summary.pow_validates_btc_target` | false | true | true | true |
| `proofs` | empty array | empty array until proof derivation | array | array |
| `event_details` | non-empty array | non-empty array | array | array |
| `competition` | null | null | null | required when paired |
| `stale_branch` | null | null | null | required |

The `unknown` invariant depends on producer behavior: rows with failing Bitcoin
target are `near`, not `unknown`. The first non-AuxPoW producer migration must
preserve `btc_parent_kind != 'unknown' OR pow_validates_btc_target`, and the
read API must fail loudly if it observes an active `unknown` event with
`pow_validates_btc_target = false`.

## Event Details

Every event detail includes source, child block identity, parent hash,
event-parent kind, generic AuxPoW fields, `child_miner_pool`, pool identity
objects, target flags, event lifecycle fields, and `pool_attributions`. Rows sort by
`event_confirmed_at`, then `source`, then `child_height`, then
`child_block_hash`, then `id`.

`event_details[].pool_attributions` is always present with `btc_parent` and
`child_block` arrays. Each array contains zero or more provenance objects:

- `namespace`
- `match_kind`
- `matched_value`
- `pool`
- `pool_identity`
- `source`
- `confidence`
- `details`

The current source labels are `btc_pool_snapshot`,
`btc_pool_snapshot_legacy_child_script`, `child_payout_registry`,
`child_coinbase_output`, `rsk_miner_registry`, and `rsk_rpc_miner`.
`child_payout_registry` marks a Namecoin/Syscoin child payout address
or Fractal/Hathor child reward address resolved through `pool_identity`;
`child_coinbase_output` marks an observed chain-native child payout/reward
address that remains unresolved. `confidence` is one of `high`, `medium`, or
`low`.

`event_details[].child_miner_pool` is the child-chain miner fact for that one
event. It is derived from child-chain evidence and is not inferred by clients
from `pool_attributions.child_block`.

For `source = "auxpow:rsk"`, `event_details[].rsk` is required and includes:

- `block_hash`
- `height`
- `is_uncle`
- `uncle_index`
- `uncle_referencing_height`
- `miner_address`
- `pool_identity`
- `merge_mining_hash`
- `merkle_proof_hex`
- `coinbase_tail_hex`
- `proof_format`

`miner_address` and `pool_identity.identifier` use lowercase 40-character hex
without `0x`. `proof_format` is currently `rskj_rpc_opaque`. `uncle_index` and
`uncle_referencing_height` are both non-null iff `is_uncle = true`.
`child_height` equals `rsk.height`; for uncle rows, `uncle_referencing_height`
is the canonical RSK block height that referenced the uncle. The current RSK
sidecar stores this value in the producer-facing `uncle_parent_height` column,
but the API uses the clearer wire name.

RSK rows set Namecoin-only generic fields to `null`: parent coinbase txid,
parent coinbase script, parent coinbase outputs, child coinbase txid, child
coinbase script, `aux_merkle_proof_hex`, and `pow_validates_child_target`.
Fractal rows use the generic AuxPoW fields, but their child coinbase fields are
only populated after the paired full child block has been captured or replayed.
The raw CAuxPoW proof still comes from `getblockheader <hash> false true`; the
child coinbase txid/script/output fields and any `fractal_reward_address`
attribution come from full `getblock <hash> 0` bytes whose transaction merkle
root matched the child header.

Hathor rows use the generic parent evidence fields and expose reward identity
only through `event_details[].pool_attributions.child_block`. Standard HTR
outputs decoded from the Hathor sidecar's persisted `funds_graph` bytes appear
as `hathor_reward_address`; child coinbase fields and `aux_merkle_proof_hex`
remain `null`.

## Event Detail Field Availability

| Field family | Namecoin/Syscoin | Fractal Bitcoin | RSK | Hathor |
|---|---|---|---|---|
| Identity and source | populated from `merge_mining_event` | populated from `merge_mining_event` | populated from `merge_mining_event` | populated from `merge_mining_event` |
| Parent coinbase fields | populated when parsed | populated when parsed | `null` | populated when reconstructed |
| Child coinbase fields | populated when parsed | populated when full child block capture/replay has run | `null` | `null` |
| `aux_merkle_proof_hex` | raw Namecoin-family AuxPoW bytes | raw Fractal CAuxPow bytes | `null` | `null` |
| `aux_proof` | decoded branches when the blob parses, else `null` | decoded branches when the blob parses, else `null` | `null` | `null` |
| `chain_id` / `slot_index` | `chain_id` cited-or-null (Namecoin = 1); `slot_index` when the blob parses and its parent matches | same rules | `null` | `null` |
| `rsk` | `null` | `null` | required object | `null` |
| `child_miner_pool` | populated with a known pool or Unknown | populated with a known pool or Unknown | populated with a known pool or Unknown | populated with a known pool or Unknown |
| `pool_attributions` | always present, BTC parent matches, legacy child script tags, and chain-native child payout addresses when matched or observed | always present, BTC parent matches and `fractal_reward_address` rows when matched or observed | always present, child `rsk_miner_address` when observed | always present, BTC parent matches and `hathor_reward_address` rows when observed |
| `pow_validates_btc_target` | populated | populated | populated | populated |
| `pow_validates_child_target` | populated | populated | `null` | `null` |
| `difficulty_epoch_ok` | `null` | `null` | `null` | `null` |
| Lifecycle fields | populated | populated | populated | populated |

## Proof Lifecycle

AuxPoW proofs are derived from contributing `merge_mining_event` rows:

- `discovered_at`: earliest contributing event `discovered_at`;
- `confirmed_at`: earliest current contributing event `confirmed_at`;
- `revoked_at`: `null` while any contributing event remains active, otherwise
  the maximum contributing `revoked_at`;
- `revocation_reason`: null while live, optional summary when all events are
  revoked;
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

Unexpected pool checkout, SQL, or response-building invariant failures return
HTTP 500 with `code = "internal_error"`, message `internal server error`, and
empty details. The server logs the underlying error; clients should treat this
as a server fault, not a query contract response.

Expected 4xx cases:

- `/api/v1/block/:hash`: `invalid_hash`, `not_found`.
- `/api/v1/tree`: `invalid_query`, `unsupported_source`, `range_too_large`,
  `backbone_unsynced`, `backbone_conflict`.
- `/api/v1/navigator/{target}`: `invalid_query`, `range_too_large`.
- `/api/v1/sources`: no query validation for the base endpoint.
- `/api/v1/version`: no query validation and no database checkout.

## Bounds

- `/api/v1/tree`: at most 2,016 explicit requested Bitcoin heights. `at_height`
  and a resolved `at_time` request are single-height lookups unless
  `context=compact` is supplied, in which case the backend chooses a bounded
  range around the target and emits hidden edges for omitted canonical spans.
  The omitted-height default is a 16-block tip window. Responses return at most
  500 nodes after deterministic context collapsing. Compact mode also caps
  event candidates, required candidates, and optional orphan candidates before
  component expansion, then shrinks or skips non-essential context so a valid
  covered target renders instead of surfacing
  `range_too_large`. Direct explicit ranges can still return `range_too_large`
  when they request too many non-collapsible nodes. When
  `include_unheighted=true`, `unheighted_from` / `unheighted_to` may span at
  most 31 UTC calendar days and return at most 250 null-height near/unknown
  nodes after filters.
- `/api/v1/navigator/{target}`: page size at most 2,000. Stale targets paginate
  over height-axis cursors; orphan targets paginate over time-axis cursors.
  Every target is presented newest-first, exposes opaque bidirectional cursors
  (`direction=older|newer`), and supports `anchor_hash` locate mode. `orphan`
  and `orphan-branch` are filtered by `classification` (default strict+weak).

Navigator cursors are target-bound and axis-bound. Clients must treat them as
opaque strings and send them back unchanged. Clients narrow the query after
`range_too_large`.

## Fixture Coverage

`fixtures/api/manifest.json` defines machine-readable fixture coverage. It
lists every `fixtures/api/*.json` file except
`manifest.json` itself. Error fixtures use `endpoint_family = "errors"`.

`tests/api_fixture_contract.rs` smoke-checks the manifest, parses every fixture,
and verifies the minimal response envelope. Endpoint behavior is covered by the
endpoint and route tests rather than by field-by-field fixture assertions.
