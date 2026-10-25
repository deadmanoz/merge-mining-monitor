export const NAV_COARSE_STRIDE = 100;

function formatNavHeight(height) {
  const h = Number(height);
  return Number.isFinite(h) ? `#${h.toLocaleString("en-US")}` : null;
}

function indexLabel(index, total) {
  const n = Number(index);
  const N = Number(total);
  if (Number.isFinite(n) && Number.isFinite(N) && n > 0 && N > 0 && n <= N) {
    return `${n.toLocaleString("en-US")} of ${N.toLocaleString("en-US")}`;
  }
  return total != null && Number.isFinite(N)
    ? `${N.toLocaleString("en-US")} total`
    : null;
}

function formatOrphanDate(epochSeconds) {
  const epoch = Number(epochSeconds);
  return Number.isFinite(epoch) ? new Date(epoch * 1000).toISOString().slice(0, 10) : null;
}

// A target whose readout follows its current item (branches and mixed signals).
function itemStepperState({ item = null, total = null, hasOlder = false, hasNewer = false }, labelForItem) {
  const currentLabel = item ? labelForItem(item) : null;
  const readout = [currentLabel, indexLabel(item?.index, total)].filter(Boolean).join(" · ");
  return { olderEnabled: !!item && !!hasOlder, newerEnabled: !!item && !!hasNewer, readout };
}

// A target whose readout follows the anchor its selection hydrated (stale, invalid, orphan).
function anchorStepperState(
  { anchor = null, item = null, total = null, hasOlder = false, hasNewer = false },
  labelForAnchor,
) {
  if (!anchor) return { olderEnabled: false, newerEnabled: false, readout: "" };
  return {
    olderEnabled: !!hasOlder,
    newerEnabled: !!hasNewer,
    readout: [labelForAnchor(anchor), indexLabel(item?.index, total)].filter(Boolean).join(" · "),
  };
}

function branchLabel(current) {
  const min = Number(current.position?.min);
  const max = Number(current.position?.max);
  const depth = Number(current.branch?.depth);
  const heightLabel = Number.isFinite(min) && Number.isFinite(max)
    ? min === max ? formatNavHeight(min) : `${formatNavHeight(min)}-${max.toLocaleString("en-US")}`
    : null;
  const depthLabel = Number.isFinite(depth) ? `depth ${depth}` : null;
  return [heightLabel, depthLabel].filter(Boolean).join(" · ");
}

function orphanBranchLabel(current) {
  const depth = Number(current.branch?.depth);
  const depthLabel = Number.isFinite(depth) ? `depth ${depth}` : null;
  return [formatOrphanDate(current.position?.max), depthLabel].filter(Boolean).join(" · ");
}

// A block with a height reads `#height`; an orphan reads its date and the height it steps at,
// or the date alone when it predates every stored canonical block (step height 0).
function mixedSignalLabel(current) {
  const step = Number(current.position?.max);
  if (!current.orphan) return formatNavHeight(step);
  const near = step > 0 ? `near ${formatNavHeight(step)}` : null;
  return [formatOrphanDate(current.orphan.btc_header_time), near].filter(Boolean).join(" · ");
}

const heightAnchorStepper = (slot) => anchorStepperState(slot, (anchor) => formatNavHeight(anchor.btc_height));

export function orphanAnchorFromBlock(payload, hash, classification = null) {
  const block = payload?.block;
  if (!block || block.kind !== "unknown") return null;
  if (Array.isArray(classification)) {
    const cls = block.btc_orphan_class ?? "pending";
    if (!classification.includes(cls)) return null;
  }
  return { btc_header_time: block.header?.time, hash };
}

const TARGETS = Object.freeze({
  stale: Object.freeze({
    id: "stale",
    route: "stale",
    label: "Stale",
    optionLabel: "Latest stale",
    stateKey: "stale",
    selectionPrecedence: 20,
    stepper: heightAnchorStepper,
  }),
  branch: Object.freeze({
    id: "branch",
    route: "stale-branch",
    label: "Branch",
    optionLabel: "Latest stale branch",
    stateKey: "branch",
    selectionPrecedence: 10,
    stepper: (slot) => itemStepperState(slot, branchLabel),
  }),
  invalidBlock: Object.freeze({
    id: "invalidBlock",
    route: "invalid-block",
    label: "Invalid blocks",
    optionLabel: "Latest invalid block",
    stateKey: "invalidBlock",
    selectionPrecedence: 25,
    stepper: heightAnchorStepper,
  }),
  orphan: Object.freeze({
    id: "orphan",
    route: "orphan",
    label: "Orphans",
    optionLabel: "Latest orphan",
    stateKey: "orphan",
    selectionPrecedence: 40,
    stepper: (slot) => anchorStepperState(slot, (anchor) => formatOrphanDate(anchor.btc_header_time)),
  }),
  orphanBranch: Object.freeze({
    id: "orphanBranch",
    route: "orphan-branch",
    label: "Orphan branch",
    optionLabel: "Latest orphan branch",
    stateKey: "orphanBranch",
    selectionPrecedence: 30,
    stepper: (slot) => itemStepperState(slot, orphanBranchLabel),
  }),
  mixedSignal: Object.freeze({
    id: "mixedSignal",
    route: "mixed-signal",
    label: "Mixed signals",
    optionLabel: "Latest mixed signals",
    stateKey: "mixedSignal",
    selectionPrecedence: 50,
    stepper: (slot) => itemStepperState(slot, mixedSignalLabel),
  }),
});

const NAV_MENU_TARGETS = Object.freeze([
  Object.freeze({ id: "tip", label: "Live tip", optionLabel: "Live tip" }),
  TARGETS.stale,
  TARGETS.branch,
  TARGETS.invalidBlock,
  TARGETS.orphan,
  TARGETS.orphanBranch,
  TARGETS.mixedSignal,
]);

export function navMenuTargets() {
  return NAV_MENU_TARGETS.map((target) => ({
    id: target.id,
    label: target.label,
    optionLabel: target.optionLabel,
  }));
}

export function navigatorTarget(targetId) {
  return TARGETS[targetId] || null;
}

export function isNavigatorTarget(targetId) {
  return !!navigatorTarget(targetId);
}

export function navigatorRoute(targetId) {
  return navigatorTarget(targetId)?.route ?? null;
}

export function targetState(state, targetId) {
  const target = navigatorTarget(targetId);
  return target ? state[target.stateKey] : null;
}

export function setNavigatorBusy(state, targetId, busy) {
  const slot = targetState(state, targetId);
  if (slot) slot.busy = !!busy;
}

export function resetNavigatorTargetState(state, targetId) {
  const slot = targetState(state, targetId);
  if (!slot) return;
  slot.item = null;
  slot.total = null;
  slot.hasOlder = false;
  slot.hasNewer = false;
}

export function anyNavTargetBusy(state) {
  return Object.keys(TARGETS).some((targetId) => !!targetState(state, targetId)?.busy);
}

export function applyNavigatorPayload(state, targetId, payload, item) {
  const slot = targetState(state, targetId);
  if (!slot) return null;
  const selected = item ?? payload?.items?.[0] ?? null;
  slot.item = selected;
  slot.total = payload?.total ?? (selected ? slot.total : 0);
  slot.hasOlder = !!payload?.next_cursor;
  slot.hasNewer = !!payload?.prev_cursor;
  if (targetId === "stale" || targetId === "invalidBlock") {
    slot.anchor = selected ? {
      btc_height: selected.position?.max,
      hash: selected.primary_hash,
    } : null;
  } else if (targetId === "orphan") {
    slot.anchor = selected ? {
      btc_header_time: selected.position?.max,
      hash: selected.primary_hash,
    } : null;
    slot.counts = payload?.facets?.orphan_classes ?? slot.counts;
  }
  return selected;
}

export function navigatorItemForStep(payload, direction) {
  const rows = Array.isArray(payload?.items) ? payload.items : [];
  if (!rows.length) return null;
  return direction === "older" ? rows[rows.length - 1] : rows[0];
}

export function navigatorItemView(item) {
  if (!item) return null;
  if (!item.view && item.view_error) {
    return {
      mode: "unavailable",
      error: item.view_error,
      anchorHeight: item.view_error.target_height ?? null,
      centerHash: item.primary_hash,
    };
  }
  if (item.view?.mode === "tree_window") {
    return {
      mode: "window",
      view: {
        treeWindow: "generated",
        treeFrom: String(item.view.tree_from),
        treeTo: String(item.view.tree_to),
        treeTargetHeight: String(item.view.target_height),
        treeHeight: "",
        treeTime: "",
        unheightedAnchor: "",
      },
      centerHash: item.view.center_hash ?? item.primary_hash,
    };
  }
  if (item.view?.mode === "unheighted_anchor") {
    return {
      mode: "anchor",
      view: { unheightedAnchor: item.view.anchor_hash },
      centerHash: item.view.center_hash ?? item.view.anchor_hash ?? item.primary_hash,
    };
  }
  if ((item.kind === "stale" || item.kind === "invalid-block") && item.position?.axis === "height") {
    return {
      mode: "height",
      view: {
        treeHeight: String(item.position.max),
        treeTime: "",
        unheightedAnchor: "",
      },
      centerHash: item.primary_hash,
    };
  }
  return null;
}

// Each target reads its own slot; the stepper functions ignore the slot fields they do not use.
export function navigatorStepperState(state, targetId) {
  const slot = targetState(state, targetId);
  if (!slot) return { olderEnabled: false, newerEnabled: false, readout: "" };
  return navigatorTarget(targetId).stepper(slot);
}

export function navSelectLabelForState(state) {
  if (isNavigatorTarget(state.nav.target)) return navigatorTarget(state.nav.target).label;
  if (state.query?.unheightedAnchor) return "Orphans";
  if (state.query?.treeHeight) return "Height";
  if (state.query?.treeTime) return "Date/Time";
  return "Live tip";
}

function selectedTreeNode(state) {
  const hash = state.selectedHash;
  if (!hash) return null;
  return state.tree?.nodes?.find((entry) => entry.hash === hash) ?? null;
}

function itemMatchesSelection(state, targetId) {
  const hash = state.selectedHash;
  const item = targetState(state, targetId)?.item;
  if (!hash || !item) return false;
  if (item.primary_hash === hash) return true;
  const branch = item.branch;
  if (!branch) return false;
  const nodeBranchId = selectedTreeNode(state)?.branch?.branch_id
    ?? state.selectedBlock?.stale_branch?.branch_id
    ?? null;
  if (nodeBranchId && branch.branch_id === nodeBranchId) return true;
  return branch.root_hash === hash || (branch.tip_hashes || []).includes(hash);
}

export function navSelectionMatches(state) {
  const matches = { tip: !state.selectedHash };
  for (const targetId of Object.keys(TARGETS)) {
    matches[targetId] = itemMatchesSelection(state, targetId)
      || (targetId === "stale"
        && state.stale.anchor?.hash === state.selectedHash)
      || (targetId === "orphan"
        && state.orphan.anchor?.hash === state.selectedHash
        && !itemMatchesSelection(state, "orphanBranch"));
  }
  return matches;
}

export function selectionTargetForState(state) {
  const ordered = Object.values(TARGETS)
    .slice()
    .sort((a, b) => a.selectionPrecedence - b.selectionPrecedence);
  for (const target of ordered) {
    if (itemMatchesSelection(state, target.id)) return target.id;
  }
  if (state.stale.anchor?.hash === state.selectedHash) return "stale";
  if (state.orphan.anchor?.hash === state.selectedHash) return "orphan";
  return null;
}
