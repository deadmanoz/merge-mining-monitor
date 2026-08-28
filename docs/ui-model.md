# UI Model

> Current shipped state: the live first screen is the Bitcoin header tree and
> the block detail drawer, flanked by collapsible, drag-resizable rails. The
> left Controls rail starts with direct Height and native UTC Date/Time
> tree lookup controls, followed by Classification and Source controls.
> Canonical, stale, and consensus-invalid error blocks are client-side highlights that fade non-matching blocks
> rather than refetch; strict and weak orphan are the server-side orphan
> signal. The detail drawer starts collapsed and auto-expands on selection.
> A topbar switcher selects between the header tree, the Header Time Delta
> distribution, and the Findings feed of evidence-backed editorial stories.

## Primary Layout

The workspace is a three-column analytical screen:

- controls rail on the left;
- the active view in the main canvas;
- detail drawer on the right.

The screen should preserve context while drilling in: selecting a block or
branch opens the drawer and centers the camera on it without replacing the tree.

### Views

Three top-level views share that workspace, selected by a topbar switcher and
by the `view` query parameter (`tree` is the default and is never written to
the URL). Only the rail controls and the main canvas swap; the Source filter
and the block detail drawer are shared, so a selection and a source selection
both survive a view change.

- `tree`: the Bitcoin header tree. Owns the Height / UTC Date/Time lookup, the
  Classification controls, and the tree navigator.
- `delta`: the Header Time Delta distribution over every stale-vs-canonical
  competition, read from `/api/v1/competitions`. Owns a focus window, bin
  width, count scale, and an Era range (`era=<from>-<to>`, written only when
  narrowed).
- `findings`: the evidence-backed editorial feed over the generated findings
  corpus. Owns Category and Status filters and an open-article slug
  (`finding=<slug>`, written only while an article is open). The one layout
  exception: findings collapses the drawer column entirely (drawer state is
  preserved for return), because the article replaces the feed in the canvas
  and has no third pane.

An unregistered `view` value falls back to `tree` and is cleared from the URL,
so a link to a view that does not exist yet degrades rather than rendering an
empty workspace.

The delta view answers one question that a single linear axis cannot: the
middle half of the distribution sits inside about a minute while the extremes
run to weeks. One focus window therefore drives three panels. A linear,
zero-centred histogram shows the core; hatched off-scale gutters at each window
edge carry the counts it excludes, on the same count axis and flat-topped when
they overflow it; and an always-visible symmetric-log strip (linear within
plus or minus ten seconds, decades beyond) shows the whole range, with a
draggable brush for the window and one rug tick per excluded record. A Coverage
tab plots the share within plus or minus T on a log axis, and a Table tab is
the accessible twin, including the off-scale rows.

Deltas the API cannot express (a difference outside a 32-bit second count)
are excluded from every statistic and reported as an unavailable count, never
counted as zero. A selection that the Source or Era filter excludes keeps its
marker on the strip and is named in the outlier panel, but contributes to no
count; navigation never silently resets a filter, and that notice instead offers
to clear only the filters actually hiding the record.

The two views cross-link through the shared selection. In the block detail, a
stale block's Header Time Delta is a link into the distribution, which arrives
focused on that competition: marked on the strip, and either marked on the
histogram bin that holds it or scrolled to and flagged in the outlier panel when
it sits past the window edge. A delta the API could not express is not a link,
because it has no position, no bin and no outlier row. In the other direction,
the outlier panel names the current selection and offers to show it in the tree,
which retargets the tree window on its height rather than merely switching
views. Both directions preserve `selected`, so `?view=delta&selected=<hash>`
restores the view, the selection, the focused state and the open detail panel.

The findings view has two canvas states. The feed lists findings newest-first
as cards grouped under month labels, each carrying a category chip, status
pill, observed date, summary with live citations, affected-chain chips, and
anchor/figure counts. Opening a card replaces the feed with the article
(back control plus newer/older navigation over corpus order); the browser
Back button does not close it, because the app is replaceState-only
throughout. Articles render cited prose paragraphs, structured evidence
panels, typed evidence anchors, and a Sources list. Each evidence panel leads
with compact claim metrics, then renders a validated series chart or event
timeline from the generated corpus, never a committed bitmap. Series charts
support separate labelled lines, fixed-period bars, discrete lollipops,
horizontal comparisons, event markers, and highlighted intervals. Timelines
support named lanes, active intervals, boundary events, and expected-cadence
ticks. Every figure carries a prose equivalent for assistive technology;
line style and mark shape keep comparisons understandable without color, and
the plot becomes horizontally scrollable rather than shrinking labels past
legibility on narrow screens. A `btc-height` anchor jumps to the tree at that height and
carries no `finding=` into the tree URL; a `source` anchor opens the source
detail dialog; `child-height` and `pool` anchors are informational chips. An
unknown `finding=` slug degrades to the feed and clears the parameter, and a
`finding=` supplied with a non-findings `view=` is dropped at hydration. The
Category and Status rail filters are findings-local and not URL-persisted;
the shared Source filter narrows the feed by each finding's affected sources.

Visual form follows the claim. Foundry's RSK and Fractal exit uses separate
weekly series across the full paired window, an exact last-parent marker, and
an explicit zero tail. The Elastos halt uses weekly bars so the partial halt
bucket is not mistaken for a complete week. The Foundry stale cluster uses
one lollipop per stale block so irregular events are not connected into a
false trend. Hathor keeps its long-window difficulty line but labels the
pre-collapse regime, post-collapse floor, and work reduction directly. The
Foundry's RSK and Fractal entry uses paired weekly series across its opening
window, while Terracoin uses an event timeline for sparse full-target wins.

The topbar includes a compact About affordance beside the product name. It
opens a modal with a step-through visual explainer of why merge-mined AuxPoW
chains can reveal Bitcoin stale and orphan evidence, plus a release-notes tab
rendered from the embedded `RELEASE_NOTES.md`.

The default Bitcoin header tree request omits explicit height bounds and lets
the backend resolve a tip-focused canonical window from the local synced
backbone. Manual lookup is explicit but mode-free: leaving Height and UTC
Date/Time empty omits bounds, Height sends `at_height`, and the native UTC
Date/Time picker sends strict `at_time` (`YYYY-MM-DDTHH:MM:SSZ`) for the backend
to resolve to the latest complete canonical Bitcoin block at or before that
timestamp. Manual Height and Date/Time lookups always request compact context
automatically, so users do not choose between exact and compact modes.
`from_height` / `to_height` windows remain direct API support, but the UI no
longer exposes manual range entry or preserves old range URL state.
If the requested or resolved height window has not been synced from Bitcoin Core
yet, the API returns a backbone 409 rather than hydrating headers in the UI
request.

### Tree navigator

A single "Go to" navigator in the tree heading replaces the former co-located
Live tip / Latest stale / Latest stale branch button clusters. The select chooses
the target the stepper walks (Live tip, Latest stale, Latest stale branch, Latest
error block, Latest orphan, or Latest orphan branch); the shared `«` `‹` `›` `»` controls then step
within that target, the inner `‹` `›` by one and the outer `«` `»` by a coarse stride.
One readout shows the active target's position. The target is locked when picked
from the menu and otherwise follows the selection, so clicking a node re-derives
which target the stepper is walking (branch over stale over error block over
orphan branch over orphan by precedence; kinds are mutually exclusive, so an
error block can only ever match its own target).

The select displays the active target (Live tip / Stale / Branch / Error blocks /
Orphan branch / Orphans, "Height" for a height lookup, or "Date/Time" for a
timestamp lookup) rather than the last menu choice, so it always names what is
being stepped; the dropdown lists the six "Latest ..." actions. Picking an
action always re-runs it, even the one already active, so choosing "Latest
stale" after stepping away jumps back to the most recent stale.

"Live tip" is the height-tip default: choosing it clears any explicit tree view
(a height lookup, a timestamp lookup, or an orphan anchor), deselects the open
block, collapses the drawer, and returns to the backend tip window centered on
the tip. The navigator's Live tip is a full deselect.

"Latest stale" navigates the proven stale blocks (the `stale` parent kind). It
jumps to the most recent proven stale and steps older / newer through bounded
unified navigator pages (`/api/v1/navigator/stale`, newest-first); the readout is
`#<height> · n of N`. Each jump uses the row's server-provided `view`
tree window, selects the stale, and centers the camera. If the row carries
`view_error` instead, the tree panel surfaces that as a Bitcoin Core
coverage/sync issue and does not advance into a rendering-density failure.
Stepping disables at the ends of the index; clicking any stale node hydrates a
keyset anchor with one-row edge probes. The outer `«` / `»` coarse steps request
up to the coarse stride and land on the boundary row.

"Latest stale branch" navigates multi-block stale branches only, backed by
`/api/v1/navigator/stale-branch` (current stales with canonical competitors,
grouped by stale-to-stale previous-header links, one-block branches excluded).
The readout uses branch-level context, for example
`#700,005-700,006 · depth 2 · n of N`.
Branch jumps use the row's server-provided `view` tree window, open the
drawer, select the branch root, and center on it. Rows that cannot safely
advertise a generated tree window carry `view_error` instead, and the tree
panel reports the supplied action. Clicking any branch member hydrates the
matching branch through `anchor_hash`, so interior members resolve too.

"Latest error block" navigates consensus-invalid, full-proof-of-work
blocks, backed by `/api/v1/navigator/error-block`. The readout is the same
height-plus-place form as "Latest stale", for example `#946,213 · 1 of 33`,
because each item is a single block on the height axis. Ordering is newest-first
by height and then by stored hash bytes: more than one error block can share a
height, so the hash tie-break is what makes stepping return every member exactly
once. It accepts no `classification` filter, since consensus invalidity is not a
refinement of `unknown`. Jumps use the row's server-provided
`view` tree window exactly as "Latest stale" does, and rows that cannot
advertise one carry `view_error` instead. Selecting an error block
directly, by clicking its tree node or restoring a `?selected=` link, hydrates
this target through `anchor_hash` so the readout follows the selection and
stepping continues from it.

"Latest orphan branch" navigates multi-block orphan branches only, backed by
`/api/v1/navigator/orphan-branch` (proven prev_hash-linked orphans grouped into
components of depth >= 2, one-block components excluded). It is the orphan twin
of "Latest stale branch" and appears last in the Go to menu. The readout is the newest member's UTC date plus the depth,
for example `2015-07-04 · depth 2 · n of N`, because orphans have no height
ordinal. Each jump anchors the tree on that branch's root hash via
`unheighted_anchor`, which renders the whole component: every member is a tree
node at its own derived `placement_height`, with proven orphan-to-orphan links
shown as solid `orphan` edges and the root attaching to the canonical spine with
a solid `orphan` edge (verified strict root) or a dashed `orphan_approx` edge
(approximate placement). Forked sibling orphans off one parent are placed on
distinct lanes (vertical offset) so competing siblings do not overlap. The index
is classification-filtered and reloads when the orphan-class controls change.
Copy distinguishes orphan branches from proven stale branches: it does not use
"winning", "competition", or "canonical" language for orphan branches.

"Latest orphan" navigates the unheighted BTC orphans, backed by
`/api/v1/navigator/orphan` (newest-first, keyset-paginated). Orphans are the
Bitcoin-Core-gated refinement of `unknown` blocks, filtered by the Classification
control's strict and weak orphan toggles. Excluded and pending refinements remain
backend states but are not exposed in the left rail. Orphans have no stored Bitcoin height, so
navigating to one switches the tree to the anchor-centered `unheighted_anchor`
mode, where the backend dangles the orphan as a fork off the canonical chain at
its derived placement height within a `±16` canonical window (a strict orphan at
its validated BIP34 height with a solid fork edge to its real predecessor; a weak
orphan at its timestamp-selected epoch height with a distinct
dashed `~`-marked edge). The orphan node is coloured by its orphan class and the
legend shows only the strict and weak orphan signal. When no placement and no canonical context can be
resolved, it falls back to a flat left-to-right time strip of the orphan and its
nearest-in-time neighbors. The readout is the anchor's date, the 1-based newest-first place in the
filtered index (`n of N`), and the per-class counts for the selected
classes. The inner `‹` `›` step one orphan older / newer; the
outer `«` `»` page a coarse stride (100) older / newer and land on the page
boundary. The older / newer directions disable at the extremes of the index.
Toggling the orphan-class filter re-drives the navigator; if the new filter has
no orphans, the view falls back to "Live tip". Selecting an in-filter orphan node
re-anchors the view on it; the anchor view persists when the open block is
deselected, and only "Live tip" leaves it.

## Filters

Classification:

- `canonical`
- `stale`
- `strict orphan`
- `weak orphan`

The Classification rail combines the visible parent-kind highlights with the
visible BTC-orphan signal. Each row has a compact helper affordance explaining
the criterion and interpretation: canonical means active-chain Bitcoin context,
stale means Bitcoin-valid but off the active chain, strict orphan means
Core-absent Bitcoin-valid work with BIP34 height and epoch nBits agreement, and
weak orphan means Core-absent Bitcoin-valid work with timestamp-epoch nBits
agreement.

Source (live section):

- Bitcoin
- Namecoin
- RSK
- Syscoin
- Fractal Bitcoin
- Hathor
- Elastos

Recovered datasets, recovered subsets, recovered surveys, and catalogued chains
join the rail in their own collapsible sections, described below.

The Source rail displays chain-level names, not raw source codes. Bitcoin is
listed first because it supplies the parent-chain classification context that
makes child-chain evidence interpretable. Each source row has a compact help
affordance that opens the chain/source detail modal; the modal may expose the
raw source code for provenance, but the rail label stays chain-oriented. Known
chain modals should use curated product copy with inception dates and
source-specific caveats, such as RSK uncle evidence and Fractal Bitcoin's
partial merge-mined coverage, authored in `data/sources/chain_profiles.json`.

The rail separates source rows into **Live sources**, **Recovered datasets**,
**Recovered subsets**, **Recovered surveys**, and **Catalogued (not recovered)**
using the Source Lifecycle Registry metadata.
Live sources include active producers plus Bitcoin Core classification context.
Recovered datasets have complete in-scope coverage without a live producer:
Lyncoin covers every Bitcoin-era height through 260,499 (the later Flex era has
no Bitcoin parent), while SixEleven covers all 999,407 blocks through its
available tip. Recovered subsets have filterable rows but incomplete child-chain
coverage, as with VCash's 68 archived explorer mappings. Recovered surveys are
completed recoveries with nothing to filter, as with Doichain's zero Bitcoin
block winners. Catalogued sources have not been recovered at all.

Surveyed and catalogued rows are disabled but retain an active info button.
Source sections are collapsible and non-live sections default closed; a
collapsed section summary shows when it contains active source selections.
The topbar source-status bead and popover summarize only operational live
sources (`live` and `bitcoin-core-backbone` sync modes); non-live lifecycle
classes do not carry aliveness status there.

The topbar source-status popover separates capture progress from evidence
freshness. Status pills show the sync state only (`Live`, `Catching up`,
`Stale`, `Error`, `Not started`, `Unknown`); the source mode remains in the
tooltip. **Height** is the source's current indexed height; live AuxPoW rows
show `progress / target` when an observed target height exists, while Bitcoin
Core backbone rows keep the target height in the tooltip. **Cursor Updated** is
the time the cursor/backbone row last advanced or was seeded. **Latest Evidence** is the newest AuxPoW evidence
timestamp seen for that source; Bitcoin Core backbone rows mark this as `N/A`
because they observe chain-tip progress rather than AuxPoW evidence. A stale
cursor means progress has not advanced recently; it is not the same claim as
stale evidence. When a source reports an error, the error code and associated
height are shown directly in the Cursor Updated cell so operators do not need
to hover the status pill to see the failure reason.

### Planned filter dimensions

These dimensions are modelled by the API contract but not yet exposed as rail
controls:

- Pool: known pools, the unknown-pool bucket, or a selected pool.
- Proof state: AuxPoW evidence, derived AuxPoW proof where available, live
  Bitcoin observation, combinations of the three, or none (a near or unknown
  parent row with no derived proof yet).
- Stale branch depth: one-block branch versus multi-block branch.

## Visual Semantics

Canonical, stale, and the visible strict/weak orphan signal must be visually
distinct. Near, pending, and excluded remain backend refinements but are not
shown in the left rail or default legend.

Each Bitcoin header renders as one uniform rounded block. Parent kind is carried
by fill color alone, with a matching legend entry; there is no shape or size
encoding and no per-kind text label on the block. The block shows the Bitcoin
height and the best-available miner (`display_miner_pool`) inside it. That label
is the strict Bitcoin coinbase miner when known, and otherwise the chain-agnostic
child-inferred miner: an RSK-only stale block whose compressed AuxPoW proof has
no recoverable Bitcoin coinbase still labels with its merge-miner rather than
"unknown miner". The drawer keeps `Bitcoin miner` strict (Unknown for that case)
and adds an `Inferred miner` row when `display_miner_basis` is `child_inferred`,
so the inference is never mistaken for a coinbase fact. Merge-mined child-chain
evidence from
`child_chain_evidence` is summarized as a single count badge in the block's
top-right corner: the number of distinct AuxPoW child chains that merge-mined the
header. That count reflects all evidence and does not change with the Source or
Kind highlight. Selecting a block expands an inline per-chain breakdown (each
chain with its child-block height or range, and an event count when more than one
child block commits) anchored to the block as a non-reflowing overlay, with the
full detail in the drawer.

Canonical/stale Classification rows and Source are client-side highlights rather
than server filters. With the default (both visible kinds, no source) every
kind-matched block is prominent; selecting a strict subset fades the blocks that
do not match (the intersection of both), along with the edges between two faded
blocks. Strict/weak orphan rows are server-side navigator filters. This makes
"which headers did this chain or signal touch" legible without changing source
counts.
Explicit focus gestures center the focal block in the viewport (the same
`width/2, height/2` point the navigator stale/orphan jumps already use): clicking
a block recenters on it only when it sits near a viewport edge (a block
comfortably in view is left where it is, so routine clicking does not jerk the
camera), entering a tree height centers on that height's canonical
block, entering a date/time centers on the resolved window's mid-height (an
approximate "go to roughly this date" landing, since the backend does not echo
the exact resolved block), and Live tip and the initial load center on the chain
tip (nothing is newer than the tip, so the right half of the canvas is
intentionally empty). Re-entering the active height or date recenters without a
refetch. Refresh preserves the current pan/zoom, and source/kind highlight
toggles never move the camera.

Multi-block stale branches should read as horizontal branch rows: a stale block
that builds on another stale block advances by Bitcoin height along the same
branch row, while same-height canonical competitors remain on the main spine.

Multi-block orphan branches should read similarly, but on the unheighted anchor
view: an orphan that builds on another orphan advances by derived placement
height along the same branch row, with proven orphan-to-orphan links shown as
solid `orphan` edges. Forked sibling orphans off one parent are placed on
distinct lanes (vertical offset) so competing siblings do not overlap. The root
attaches to the canonical spine with a solid `orphan` edge when the root is
verified strict and its predecessor matches the canonical at `placement_height - 1`,
or a dashed `orphan_approx` edge otherwise. Copy must not use "winning",
"competition", or "canonical" language for orphan branches.

The tree edge legend uses user-facing categories rather than raw API enum names:
both stale-entry and stale-member edge styles roll up under "stale branch".

Missing pool attribution should appear as an explicit unknown state. It should
not disappear from counts or filters.

RSK event details need separate presentation for canonical RSK blocks and RSK
uncles. RSK miner-address attribution should be labelled separately from
Namecoin coinbase or payout-address attribution.

Revoked proof or event state is part of the detail model even though the first
fixtures only use active rows.

## Detail Drawer

The drawer is consolidated around the AuxPoW record. Its sections are:

- Parent block (Bitcoin): header identity, classification (`kind`, plus an Orphan
  class row sourced from `block.btc_orphan_class` for unknown blocks, or a
  Consensus rejection row sourced from `block.error_block_reason` for error
  blocks), raw
  coinbase tag when `block.coinbase_tag` is non-null (Core block evidence for
  Core-attested canonical blocks first, event commitment fallback when no Core
  tag exists), pool attribution, the real Bitcoin block time, and a collapsible
  raw header;
- Merge-mining commitment: the decoded AuxPoW marker (`aux_merkle_root`,
  `merkle_size`, `merkle_nonce`) and parent coinbase txid for Namecoin-family
  parents, or a format-only `rsk-opaque` / `hathor-rfc0006` entry with no marker;
- Sources & capture: the source summary, distinct source count, and AuxPoW chain
  count;
- Auxiliary blocks: the hydrated per-child events with child height, hash,
  header, time, and `nBits` shown when the source authenticated each value, plus
  an explicit unavailable state otherwise; `slot_index` / `chain_id`; the
  `parent_target` / `aux_target` PoW checks; RSK sidecar evidence for RSK rows;
  and a collapsed AuxPoW proof section that decodes `aux_proof` into its
  `coinbase_branch` / `blockchain_branch` sibling hashes (the raw
  `aux_merkle_proof_hex` bytes are only shown as a compact byte-count-plus-copy
  fallback when the blob does not decode, never as an inline hex wall);
- competition context;
- stale-branch context.

The standalone "AuxPoW Proofs" section has been removed (its fields were already
in the event and source sections, and its proof bytes now live inside each
auxiliary block). No drawer field reads as "Confirmed".

The Consensus rejection row renders the rule in prose, not as the catalogue's
raw `rejection_reason` token, with an `(i)` control opening a consensus-rule
dialog that states the rule and what the block did wrong. The mapped vocabulary
is deliberately not exhaustive: the research classifier can emit a token the
frontend has not mapped, and such a token renders as its raw value with **no**
help control rather than opening an empty or mislabelled dialog. For an error
block the competition section is replaced by an explicit note that the block
never raced and so has no canonical competitor, so its absence reads as meaning
rather than a missing panel.

For RSK rows, show `proof_format`, miner address, optional pool identity,
canonical-vs-uncle state, and opaque proof byte presence. Do not imply decoded
RSK proof meaning while `proof_format = "rskj_rpc_opaque"`.

Block hashes in the detail drawer should link to reputable chain-specific
explorers without replacing the local copy affordance: Bitcoin parent headers
use mempool.space, Namecoin child blocks use Namebrow.se, RSK child blocks use
Rootstock Explorer, and Syscoin child blocks use Syscoin Blockbook.

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
For example, an empty near view should say there are no near misses in the
selected window rather than saying no data exists.

Error states should show the API error code and message. Range errors should
prompt the user to narrow the range or choose coarser granularity.
