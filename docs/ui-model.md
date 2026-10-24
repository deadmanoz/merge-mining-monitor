# UI Model

## Product Intent

The monitor presents Bitcoin mining evidence: canonical context, stale and orphan headers, consensus-invalid blocks, available pool attribution and coinbase protocol declarations.
Merge-mined (AuxPoW) child chains embed Bitcoin parent headers in their own blocks, preserving independent records of Bitcoin work that the Bitcoin chain itself discarded.
The workspace opens directly on the data for researchers, mining analysts, and operators who want to examine mining activity, distinguish lost fork races from invalid work, and follow the evidence behind each classification.

- **Multi-source contract**: Different child chains preserve distinct evidence shapes — classic Namecoin-family AuxPoW (`namecoin`, `syscoin`, `terracoin`), Ethereum-style JSON-RPC payloads with uncle context and beneficiary addresses (`rsk`), hybrid 1-in-3 cycle merge mining with full-block child reward pairing (`fractal`), RFC 0006 REST merged mining (`hathor`), 84-byte reconstructed child headers (`elastos`), and display-order chain-merkle proofs without `hashBlock` (`qbit`).
  Lifecycle classes (`live`, `historical`, `partial`, `surveyed`, `catalogued`) describe the evidence held by the monitor rather than whether a native chain is still running, keeping recovery boundaries explicit without overstating coverage.
- **Interpretation from Bitcoin classification**: Child-chain producers record locally verified commitments, while the Bitcoin lineage gate refuses non-Bitcoin SHA-256 parents (such as Bitcoin Cash blocks or shares) before storage.
  A Bitcoin parent is `near` when it fails its own target and `unknown` when it meets it without Bitcoin-chain proof.
  The `live-chaintip:bitcoin:core` classifier turns captured headers into canonical and stale verdicts, same-height competition, branch context, and the `strict_btc_orphan` / `weak_btc_orphan` refinement for Bitcoin work whose absence Core attests.
  Consensus-invalid full-PoW headers (`invalid_block`) are derived from Bitcoin Core or the pinned research catalogue and kept separate from stale and orphan evidence so invalid work is never mistaken for a lost fork race.
- **API contracts over CSV export**: The UI consumes the typed JSON contract in `docs/api-contract.md` and does not infer product meaning from raw CSV rows.
  CSV export, RSS/SSE feeds, and runtime node management are out of scope.

## Primary Layout

The workspace is a three-column analytical screen with collapsible, drag-resizable side rails:

- controls rail on the left;
- the active view in the main canvas;
- detail drawer on the right (starts collapsed and auto-expands on selection).

The screen preserves context while drilling in: selecting a block or branch opens the drawer and centers the camera on it without replacing the tree.

### Views

Three top-level views share the workspace, selected by a topbar switcher and by the `view` query parameter (`tree` is the default and is never written to the URL).
Only the rail controls and main canvas swap; the Source filter and block detail drawer are shared across views.

- `tree`: the Bitcoin header tree.
  Owns the Height / UTC Date/Time lookup, the Classification controls, and the tree navigator.
- `delta`: the Header Time Delta distribution over every stale-vs-canonical competition (`/api/v1/competitions`).
  Owns a focus window, bin width, count scale, and an Era range (`era=<from>-<to>`, written only when narrowed).
- `findings`: the evidence-backed editorial feed over the generated findings corpus.
  Owns Category and Status filters and an open-article slug (`finding=<slug>`, written only while an article is open).
  Findings collapses the drawer column entirely while preserving drawer state for return.

An unregistered `view` value falls back to `tree` and is cleared from the URL.

The delta view uses one focus window to drive three panels because the middle half of the distribution sits inside about a minute while the extremes run to weeks: a linear, zero-centred histogram for the core; hatched off-scale gutters at each window edge carrying excluded counts on the same count axis; and an always-visible symmetric-log strip (linear within $\pm 10\text{ s}$, decades beyond) with a draggable window brush and one rug tick per excluded record.
A Coverage tab plots the share within $\pm T$ on a log axis, and a Table tab is the accessible twin including off-scale rows.
Deltas outside a 32-bit second count (`null` in the API) are excluded from statistics and reported as an unavailable count, never coerced to zero.
A selection excluded by Source or Era filters keeps its strip marker and is named in the outlier panel without contributing to counts; a notice offers to clear only the hiding filters.

The `tree` and `delta` views cross-link through the shared selection (`selected`): in the block detail drawer, an expressible stale Header Time Delta links into the distribution focused on that competition, while the outlier panel in `delta` offers to retarget the tree window on the selected block's height.

The findings view has two canvas states (`replaceState`-only):
- **Feed**: newest-first cards grouped under month labels with category chip, status pill, observed date, cited summary, affected-chain chips, and anchor/figure counts.
- **Article**: cited prose, structured evidence panels (leading with claim metrics followed by a validated series chart or event timeline rendered from the generated corpus rather than a bitmap, plus an assistive prose equivalent), typed evidence anchors (`btc-height` jumps to the tree, `source` opens the source modal, `child-height` and `pool` are informational chips), and a Sources list. Unknown `finding=` slugs degrade to the feed; `finding=` with a non-findings `view=` is dropped at hydration.
  Category and Status filters are findings-local; the shared Source filter narrows the feed by affected sources.

The topbar includes an About control that opens a modal with a four-step visual explainer of chain placement, child proofs, proof of work versus consensus validity, and classification from Core context plus reviewed invalid-block evidence.
The modal also has a release-notes tab rendered from embedded `RELEASE_NOTES.md`.

In the `tree` view, leaving Height and UTC Date/Time empty omits bounds so the backend resolves the default tip-focused canonical window.
Entering Height sends `at_height` and entering UTC Date/Time sends `at_time` (`YYYY-MM-DDTHH:MM:SSZ`), both automatically requesting `context=compact`.
Manual range entry (`from_height`/`to_height`) is not exposed in the UI.
Unsynced height windows surface the backend HTTP 409 `backbone_unsynced` / `backbone_conflict` state rather than hydrating headers on demand.

### Tree navigator

A single "Go to" navigator in the tree heading combines a target selector with shared `«` `‹` `›` `»` steppers (`‹` `›` step by 1; `«` `»` step by a coarse stride) and a position readout.
The target locks when picked from the menu and otherwise follows node selection by precedence: branch > stale > invalid block > orphan branch > orphan.
The select displays the active target (`Live tip`, `Stale`, `Branch`, `Invalid blocks`, `Orphan branch`, `Orphans`, `Height`, or `Date/Time`), while its menu lists the six `Latest ...` actions (re-picking the active action jumps back to the newest item).

| Menu Action | API Target / Mode | Readout Format | Selection & Stepping Behavior |
|---|---|---|---|
| `Live tip` | `/api/v1/tree` (default tip window) | Tip height | Clears explicit height/time/anchor state, deselects the open block, collapses the drawer, and centers on the chain tip. |
| `Latest stale` | `/api/v1/navigator/stale` (`tree_window`) | `#<height> · n of N` | Jumps to the row's `view` window, selects the stale block, and centers the camera (or surfaces `view_error`). Clicking any stale node hydrates its keyset anchor. |
| `Latest stale branch` | `/api/v1/navigator/stale-branch` (`tree_window`, `depth >= 2`) | `#<min>-<max> · depth <d> · n of N` | Opens the drawer, selects the branch root, and centers on it (or surfaces `view_error`). Clicking any branch member hydrates the owning branch via `anchor_hash`. |
| `Latest invalid block` | `/api/v1/navigator/invalid-block` (`tree_window`) | `#<height> · n of N` | Steps newest-first by height then stored hash bytes (no `classification` filter). Clicking or restoring `?selected=` on an invalid block hydrates via `anchor_hash`. |
| `Latest orphan` | `/api/v1/navigator/orphan` (`unheighted_anchor`) | `<YYYY-MM-DD> · n of N` + per-class counts | Filtered by active strict/weak orphan toggles; switches tree to `unheighted_anchor` mode (`±16` canonical context or flat time strip fallback). Coarse stride is 100. Toggling orphan classes re-drives the navigator (falling back to `Live tip` if empty). |
| `Latest orphan branch` | `/api/v1/navigator/orphan-branch` (`unheighted_anchor`, `depth >= 2`) | `<YYYY-MM-DD> · depth <d> · n of N` | Anchors the tree on the component's `root_hash` via `unheighted_anchor` and renders all linked members. Reloads when orphan-class controls change. |

## Filters

Classification:

- `canonical`
- `stale`
- `invalid block`
- `strict orphan`
- `weak orphan`

The Classification rail combines the visible parent-kind highlights with the visible BTC-orphan signal.
Each row has a compact helper affordance explaining the criterion and interpretation: canonical means active-chain Bitcoin context, stale means Bitcoin evidence placed on a branch off the active chain, invalid block means full-PoW evidence with an established consensus failure, strict orphan means Core-absent Bitcoin work with BIP34 height and epoch nBits agreement, and weak orphan means Core-absent Bitcoin work with timestamp-epoch nBits agreement.

Relationships:

- `Mixed signals`

The Relationships rail highlights blocks whose tree node `relationship.mixed` is true: their coinbase tags, payout addresses, child identities and declared accounts name two or more pools.
It is a client-side highlight like the parent kinds, kept in the URL as `mixed=1`.

Source (live section):

- Bitcoin
- Namecoin
- RSK
- Syscoin
- Fractal Bitcoin
- Hathor
- Elastos
- Qbit
- Terracoin

Recovered datasets, recovered subsets, recovered surveys, and catalogued chains join the rail in their own collapsible sections, described below.

The Source rail displays chain-level names, not raw source codes.
Bitcoin is listed first because it supplies the parent-chain classification context that makes child-chain evidence interpretable.
Each source row has a compact help affordance that opens the chain/source detail modal; the modal may expose the raw source code for provenance, but the rail label stays chain-oriented.
Known chain modals use curated product copy with inception dates and source-specific caveats (such as RSK uncle evidence and Fractal Bitcoin's 1-in-3 cycle hybrid merge-mined coverage, where skipped non-merge-mined blocks are expected rather than missing evidence), authored in `data/sources/chain_profiles.json`.

The rail separates source rows into **Live sources**, **Recovered datasets**, **Recovered subsets**, **Recovered surveys**, and **Catalogued (not recovered)** using the Source Lifecycle Registry metadata.
Live sources include live producers plus Bitcoin Core classification context.
Recovered datasets have complete in-scope coverage without a live producer: Lyncoin covers every Bitcoin-era height through 260,499 (the later Flex era has no Bitcoin parent), while SixEleven covers all 999,407 blocks through its available tip.
Recovered subsets have filterable rows but incomplete child-chain coverage, as with VCash's 68 archived explorer mappings.
Recovered surveys are completed recoveries with nothing to filter, as with Doichain's zero Bitcoin block winners.
Catalogued sources have not been recovered at all.

Surveyed and catalogued rows are disabled but retain an active info button.
Source sections are collapsible and non-live sections default closed; a collapsed section summary shows when it contains active source selections.
The topbar source-status bead and popover summarize only operational live sources (`live` and `bitcoin-core-backbone` sync modes); non-live lifecycle classes do not carry aliveness status there.

The topbar source-status popover separates capture progress from evidence freshness.
Status pills show the sync state only (`Live`, `Waiting`, `Catching up`, `Stale`, `Error`, `Not started`, `Unknown`); hovering, focusing or tapping a pill explains that state, and the Height, Cursor Updated and Latest Evidence headers carry `?` help that opens the same way. **Height** is the source's current indexed height; live AuxPoW rows show `progress / target` when an observed target height exists, while Bitcoin Core backbone rows keep the target height in the tooltip. **Cursor Updated** is the time the cursor/backbone row last advanced or was seeded. **Latest Evidence** is the newest AuxPoW evidence timestamp seen for that source; Bitcoin Core backbone rows mark this as `N/A` because they observe chain-tip progress rather than AuxPoW evidence.
An old cursor timestamp means progress has not advanced recently; runtime heartbeats and successful cycles establish liveness separately. Sources without runtime rows use the legacy cursor-age rule. The source's Stale status is distinct from a stale Bitcoin block.
When a source reports an error, the error code and associated height are shown directly in the Cursor Updated cell so operators do not need to hover the status pill to see the failure reason.

### Planned filter dimensions

These dimensions are modelled by the API contract but not yet exposed as rail controls:

- Pool: known pools, the unknown-pool bucket, or a selected pool.
- Proof state: AuxPoW evidence, derived AuxPoW proof where available, live Bitcoin observation, combinations of the three, or none (a near or unknown parent row with no derived proof yet).
- Stale branch depth: one-block branch versus multi-block branch.

## Visual Semantics

Canonical, stale, invalid-block, and the visible strict/weak orphan signal are visually distinct by fill color alone on uniform rounded blocks (no shape/size encoding or per-kind text label on the block).
Pending and excluded remain backend refinements outside the left rail and default legend; a near parent is not a tree node, and its page lists the child blocks that name it.

Each block shows its Bitcoin height and best-available miner (`display_miner_pool`): the strict `bitcoin_miner_pool` when known, or the chain-agnostic `child_inferred` miner when the Bitcoin coinbase miner is Unknown (for example an RSK-only stale block).
The drawer keeps `Bitcoin miner` strict and adds an `Inferred miner` row when `display_miner_basis` is `child_inferred`.
A top-right badge shows the count of distinct AuxPoW child chains in `child_chain_evidence` (unaffected by Source/Kind highlights), and a grey top-left badge the count of CORE and exSat declarations in `protocol_commitments`; selecting a block expands a non-reflowing overlay with per-chain height/event summaries and, below them, each declaration with its associated pool (else its declared account).
A block whose `relationship.mixed` is true gets an amber (`--signal-mixed`) outline ring just outside its fill, and its overlay lists the pools its signals name, the reference pool first.

Canonical, stale, invalid-block, Mixed signals, and Source selections are client-side highlights that fade non-matching blocks and edges without changing source counts or moving the camera; strict/weak orphan rows are server-side navigator filters.
Explicit focus gestures center on `width/2, height/2`: clicking a block recenters only when near a viewport edge, height lookup centers on that canonical block, date/time lookup centers on the window's mid-height, and Live tip / initial load center on the chain tip.
A silent 60s timer refreshes sources always and the tree when active; clicking `Updated HH:MM:SS` force-reloads while preserving pan/zoom.

Multi-block stale branches render as horizontal branch rows advancing by Bitcoin height alongside the canonical spine, with `stale_entry` and `stale` edges rolled up under "stale branch" in the legend.
In `unheighted_anchor` mode, multi-block orphan branches advance by derived `placement_height` with solid `orphan` edges between members, distinct vertical lanes for forked siblings, and a solid `orphan` (verified strict root) or dashed `orphan_approx` edge attaching the root to the canonical spine.
Copy for orphan branches never uses "winning", "competition", or "canonical" language.
Missing pool attribution renders as an explicit unknown state, never omitted.

## Detail Drawer

The drawer is consolidated around the AuxPoW record (no standalone "AuxPoW Proofs" section, and no field labelled "Confirmed"):

- **Parent block (Bitcoin)**: header identity, `kind`, `Orphan class` (`block.btc_orphan_class` for unknowns) or `Consensus rejection` (`block.invalid_block_reason` for invalid blocks), `coinbase_tag` when present, strict `Bitcoin miner` plus optional `Inferred miner`, Bitcoin block time, and collapsible raw header.
  A mixed block shows a "Mixed signals · N pools" chip beside `Bitcoin miner`, linking to the next section.
- **Relationship signals**: every pool the parent names (`relationship_signals`), as a summary sentence and one collapsed entry per pool: the reference pool first, then each other known pool, then "No known pool".
  Each entry shows its pool and signal count, and expands to one row per Bitcoin coinbase tag, payout address, child reward address or coinbase tag, and declared CORE or exSat account, with its value.
  Entries for a pool other than the reference are accented as "differs"; the help text says a signal is an association, not proof of who mined the block.
- **Merge-mining commitment**: decoded marker (`aux_merkle_root`, `merkle_size`, `merkle_nonce`) and parent coinbase txid for `namecoin-aux` / `qbit-aux`, or a format-only `rsk-opaque` / `hathor-rfc0006` entry.
- **Sources & capture**: source summary, distinct source count, and AuxPoW chain count.
- **Auxiliary blocks**: hydrated `event_details[]` with authenticated child height/hash/header/time/`nBits` (or explicit unavailable state), `slot_index` / `chain_id`, parent/aux PoW target checks, RSK sidecar fields (`proof_format = "rskj_rpc_opaque"`, miner address, pool identity, canonical-vs-uncle state, and opaque proof byte presence), and a collapsed AuxPoW proof breakdown (`aux_proof` branches, or byte-count-plus-copy fallback when `aux_merkle_proof_hex` does not decode).
- **Competition & branch context**: competition and stale-branch panels.
  For an `invalid_block`, the competition panel is replaced by an explicit note that the block never raced and has no canonical competitor.
- **Protocol commitments**: last, each CORE and exSat declaration with its decode status, declared account and associated pool, or the reason none are listed.

Child block rows label their resolved child pool `Child block pool`: it can come from a child reward or payout address or from a child coinbase tag, and it is not the Bitcoin block's miner.

Mapped `invalid_block_reason` tokens render in plain language with an `(i)` dialog explaining the violated rule; unmapped tokens render as raw text without a help button.
Drawer block hashes link to chain explorers (mempool.space for Bitcoin, Namebrow.se for Namecoin, Rootstock Explorer for RSK, Syscoin Blockbook for Syscoin) alongside the local copy button.

## Stale Branch Language

Use branch language for multi-block stale cases:

- branch;
- member;
- root;
- tip or tips;
- depth;
- winning block;
- winning miner / pool.

Avoid wording that implies every stale event is a single isolated block.

## States

Loading states should preserve the selected filters and visible layout.

Empty states should name the active filter or range that removed all results.
For example, an empty orphan view should say there are no orphans in the selected window rather than saying no data exists.

Error states should show the API error code and message.
Range errors should prompt the user to narrow the range or choose coarser granularity.

## Source runtime diagnostics

Sources distinguishes Waiting for Bitcoin Core from Catching up through local recovery. The rail and aggregate badge include Waiting. The source popover and Capture tab show a safe reason, dependency, last successful cycle, next retry and heartbeat beside cursor and evidence timestamps. Unknown reason codes use a neutral label rather than raw error text. Unresolved capture or backbone errors remain visible above runtime status; an expired heartbeat becomes Stale. Sources without runtime rows retain legacy progress-based status during rollout.
