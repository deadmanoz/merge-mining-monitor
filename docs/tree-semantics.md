# Tree And Orphan Projection Semantics

This document is an implementation reference for deriving `/api/v1/tree` and
orphan navigator responses. It explains *how* the backend resolves compact
context, places BTC-orphan components, and reduces dense windows.

These implementation details are not part of the API contract. Clients depend
only on the response fields, error codes, and bounds defined in
`docs/api-contract.md`; the backend may change the procedure as long as the
response stays equivalent. This file keeps those details separate from the
response-format contract.

## Compact Context Resolution

`context=compact` returns the requested height or time target plus nearby blocks
that explain relevant forks. The backend starts with a bounded height range
around the resolved canonical target, then shrinks the range if too many nodes
would be returned or if a missing canonical row in the wider range would
otherwise block the lookup.

The final rendered window always preserves the requested target, the compact
height boundaries, stale event rows, stale branch attachment parents, canonical
competitors, and the branch/root/tip context that intersects that final window.
Canonical rows that do not affect the visible fork structure are not returned as
individual tree nodes: they are validated with lightweight canonical rows and
collapsed into `hidden` edges whose `hidden_count` is the omitted interior count.

Backbone integrity guarantees apply inside the final rendered window, exactly as
for explicit ranges: missing or incomplete canonical interiors return
`backbone_unsynced`, and duplicate canonical heights or broken prev-hash links
return `backbone_conflict`.

Compact mode does not automatically attach BTC orphan branches. Use
`/api/v1/navigator/orphan` to find orphan candidates and
`/api/v1/tree?unheighted_anchor=<hash>` to render the placed orphan fork.

## Anchor-Mode Orphan Placement

`unheighted_anchor` renders the whole proven prev_hash-linked orphan component
that contains the anchor block. Anchor mode reads the canonical context window
from local canonical rows only and does not fetch Bitcoin Core during the
request; run `sync-bitcoin-core` for historical heights before relying on dense
orphan placement.

### Placement height derivation

The backend derives one placement height for the whole component and uses it to
place every member:

- The proven prev_hash path gives exact relative heights (`+1` per link).
- One absolute anchor is pinned: a strict member's exact BIP34 height when
  available, else the root's timestamp-selected DAA-epoch height (which is
  approximate).
- Each member's placement is then `anchor + path-offset`.

Placement is **exact** (non-approx) when any member is a strict orphan and the
strict members agree on the root height. Conflicting strict heights (inconsistent
evidence) degrade the whole component to **approximate** placement. This is the
value surfaced as the per-node `placement_height` / `placement_approx` fields
and as orphan branch height bounds; those fields are layout hints, not stored
block columns, and never appear on `/api/v1/block/:hash`.

### Edge attachment

The canonical `±16` height window around the component is loaded as context. A
proven member-to-member prev_hash link renders as a solid `orphan` edge. The
component root attaches to the canonical spine as:

- a solid `orphan` edge when the root is verified strict and its `prev_hash`
  matches the canonical block at `placement_height - 1`; or
- a dashed `orphan_approx` edge to the nearest in-window canonical otherwise
  (the honest "divergence point, real parent absent" signal).

A depth-2-or-more component carries a `branch` object
`{ "branch_id": "orphan-<root_hash>" }` on every member tree node; a single
orphan (depth 1) stays branch-less (`branch` is `null`).

### Filters and fallback

The `kinds`, `source`, and `min_sources` filters are ignored in anchor mode
(only `classification` decides whether the anchor is eligible), so the canonical
blocks used for placement are never filtered away and the jump always lands.
When no placement height can be derived, or a placement height was derived but
the window holds no canonical block to attach to, anchor mode falls back to a
flat time-ordered strip of the anchor plus its nearest-in-time orphan neighbors
(250-node cap, null window bounds).

## Tree Reduction Procedure

Tree reduction is deterministic: a given range, filter set, and dataset always
yield the same response. The backend produces the collapsed-context fields
(`hidden` edges with `hidden_count`, `window.hidden_linear_block_count`, and the
500-node cap that yields `range_too_large`) with the following procedure:

1. Build the candidate node set for the requested range and filters.
2. Never strip near nodes, unknown nodes, stale branch members, stale canonical
   competitors, or direct parents needed to attach stale roots.
3. Apply `min_sources` to evidence nodes, not canonical context nodes.
4. Include all members of a stale branch when any member satisfies filters.
5. Collapse eligible canonical-only spans by keeping their visible boundaries.
6. Choose the largest collapse span, breaking ties by lower left-boundary
   height and then lexicographic left-boundary hash.
7. Emit one `hidden` edge with `hidden_count` equal to omitted interior nodes.
8. Repeat until the response has at most 500 nodes or no eligible span remains.
9. If required nodes still exceed the cap, return `range_too_large`.

## Where BTC Orphan Classes Appear

The orphan classes (`strict_btc_orphan`, `weak_btc_orphan`,
`btc_stale_excluded`, `pending`) split `kind='unknown'` into more specific states
and appear consistently across endpoints:

- `/api/v1/navigator/orphan`, `/api/v1/navigator/orphan-branch`, and
  `/api/v1/tree?unheighted_anchor=`: the navigation endpoints. They default to
  `strict+weak` and are filterable/countable per class. These replace the old
  browsable unknown bucket.
- `/api/v1/tree` (height-window/tip) and `/api/v1/block/:hash`: add the nullable
  per-row `btc_orphan_class` detail field; `kind` is unchanged.
- `/api/v1/sources`: `counts.unknown` still counts all unknowns;
  `counts.strict_orphan` and `counts.weak_orphan` add the per-source
  strict/weak orphan sub-counts (a refinement WITHIN the unknown bucket). The
  source dialog displays strict/weak; the API response keeps the full set. These
  are parent-level: a source counts the class of every BTC parent it attests, so
  a weak-only chain (e.g. RSK) can show `strict_orphan > 0` when it shares a
  strict-classified parent.
