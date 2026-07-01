// Pure helpers for the stale stepper, so the windowing, index math, and
// disabled/readout logic stay out of the DOM handler.

// Coarse stride for the consolidated navigator's outer (<< / >>) buttons. The
// inner (< / >) buttons step by 1.
export const NAV_COARSE_STRIDE = 100;

// A block height as a readout token: "#256,558", no dangling "h" prefix.
function formatNavHeight(height) {
  const h = Number(height);
  if (!Number.isFinite(h)) return null;
  return `#${h.toLocaleString("en-US")}`;
}

function totalLabel(total) {
  return total != null && Number.isFinite(Number(total))
    ? `${Number(total).toLocaleString("en-US")} total`
    : null;
}

function itemStepperState({ item = null, total = null, hasOlder = false, hasNewer = false } = {}, labelForItem) {
  const currentLabel = item ? labelForItem(item) : null;
  const readout = [currentLabel, totalLabel(total)].filter(Boolean).join(" · ");
  return { olderEnabled: !!item && !!hasOlder, newerEnabled: !!item && !!hasNewer, readout };
}

export function branchStepperState({ item = null, total = null, hasOlder = false, hasNewer = false } = {}) {
  return itemStepperState({ item, total, hasOlder, hasNewer }, (current) => {
    const min = Number(current.position?.min);
    const max = Number(current.position?.max);
    const depth = Number(current.branch?.depth);
    const heightLabel = Number.isFinite(min) && Number.isFinite(max)
      ? min === max ? formatNavHeight(min) : `${formatNavHeight(min)}-${max.toLocaleString("en-US")}`
      : null;
    const depthLabel = Number.isFinite(depth) ? `depth ${depth}` : null;
    return [heightLabel, depthLabel].filter(Boolean).join(" · ") || null;
  });
}

// Button-enable + readout for the orphan-branch target (the orphan twin of the
// stale-branch stepper). Orphan branches have NULL btc_height (only approximate
// per-member placement), so the readout is the newest member's UTC date plus the
// depth and total, never a height range or client-side ordinal.
export function orphanBranchStepperState({ item = null, total = null, hasOlder = false, hasNewer = false } = {}) {
  return itemStepperState({ item, total, hasOlder, hasNewer }, (current) => {
    const date = formatOrphanDate(current.position?.max);
    const depth = Number(current.branch?.depth);
    const depthLabel = Number.isFinite(depth) ? `depth ${depth}` : null;
    return [date, depthLabel].filter(Boolean).join(" · ") || null;
  });
}

// --- BTC-orphan navigator ---

// The orphan stepper anchor { btc_header_time, hash } derived from a /block
// envelope, or null when the block is absent, not an unknown, or its orphan class
// is outside the active classification filter (orphans are the classified
// refinement of kind='unknown'). The /block envelope nests the block under
// `block` (block.kind, block.header.time, block.btc_orphan_class). When
// `classification` is an array, a block whose class
// (a null class counts as "pending") is not in it is NOT a valid anchor, so a
// deep-linked pending/excluded unknown under the default strict+weak filter does
// not switch the UI to the Orphans target around a row that is not in that
// navigator. Omitting `classification` keeps the unfiltered behavior.
// Pure so it is tested against the real fixture.
export function orphanAnchorFromBlock(payload, hash, classification = null) {
  const block = payload?.block;
  if (!block || block.kind !== "unknown") return null;
  if (Array.isArray(classification)) {
    const cls = block.btc_orphan_class ?? "pending";
    if (!classification.includes(cls)) return null;
  }
  return { btc_header_time: block.header?.time, hash };
}

// A UTC calendar date "YYYY-MM-DD" from a btc_header_time epoch (seconds).
function formatOrphanDate(epochSeconds) {
  const epoch = Number(epochSeconds);
  if (!Number.isFinite(epoch)) return null;
  return new Date(epoch * 1000).toISOString().slice(0, 10);
}

// Button-enable + readout for the orphan target. Keyset pagination has no cheap
// ordinal, so the readout is the anchor's date plus the global total, never an
// "n of N".
export function orphanStepperState({ anchor = null, total = null, hasOlder = false, hasNewer = false } = {}) {
  if (!anchor) {
    return { olderEnabled: false, newerEnabled: false, readout: "" };
  }
  const date = formatOrphanDate(anchor.btc_header_time);
  // A null/undefined total means "count not yet known" (Number(null) === 0, so
  // guard explicitly); a real zero-count index has no anchor and returns above.
  const count = total != null && Number.isFinite(Number(total))
    ? Number(total).toLocaleString("en-US")
    : null;
  const readout = [date, count].filter(Boolean).join(" · ");
  return { olderEnabled: !!hasOlder, newerEnabled: !!hasNewer, readout };
}

// --- Stale navigator ---

export function staleStepperState({ anchor = null, total = null, hasOlder = false, hasNewer = false } = {}) {
  if (!anchor) {
    return { olderEnabled: false, newerEnabled: false, readout: "" };
  }
  const readout = [formatNavHeight(anchor.btc_height), totalLabel(total)].filter(Boolean).join(" · ");
  return { olderEnabled: !!hasOlder, newerEnabled: !!hasNewer, readout };
}

// --- Tree node label + fill (pure, Node-tested) ---

// The big block label. Heighted nodes show their height; a fork-placed anchor
// orphan (null btc_height, derived placement_height) shows its placement height,
// prefixed with "~" when the placement is approximate (weak/excluded/pending);
// any other null-height node shows a short hash. Never renders the literal
// "null".
export function nodeLabel(node) {
  if (node?.height != null) return String(node.height);
  if (node?.placement_height != null) {
    return node.placement_approx ? `~${node.placement_height}` : String(node.placement_height);
  }
  const hash = typeof node?.hash === "string" ? node.hash : "";
  return hash ? hash.slice(0, 12) : "?";
}

// SVG rect fill CSS variable for a tree node. Unknown (orphan) nodes are coloured
// by their orphan class via the --orphan-* vars; a null/unrecognised class is the
// pending colour. Every other kind uses its structural --<kind> var. The HTML
// legend swatch classes (fill-<class>) are separate styling, not used here.
const ORPHAN_FILL_VAR = {
  strict_btc_orphan: "var(--orphan-strict)",
  weak_btc_orphan: "var(--orphan-weak)",
  btc_stale_excluded: "var(--orphan-excluded)",
};

export function nodeFillVar(node) {
  const kind = node?.kind || "unknown";
  if (kind === "unknown") {
    return ORPHAN_FILL_VAR[node?.btc_orphan_class] || "var(--orphan-pending)";
  }
  return `var(--${kind})`;
}
