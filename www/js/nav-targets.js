import {
  branchStepperState,
  orphanBranchStepperState,
  orphanStepperState,
  staleStepperState,
} from "./windowing.js";

const TARGETS = Object.freeze({
  stale: Object.freeze({
    id: "stale",
    route: "stale",
    label: "Stale",
    optionLabel: "Latest stale",
    stateKey: "stale",
    selectionPrecedence: 20,
  }),
  branch: Object.freeze({
    id: "branch",
    route: "stale-branch",
    label: "Branch",
    optionLabel: "Latest stale branch",
    stateKey: "branch",
    selectionPrecedence: 10,
  }),
  orphan: Object.freeze({
    id: "orphan",
    route: "orphan",
    label: "Orphans",
    optionLabel: "Latest orphan",
    stateKey: "orphan",
    selectionPrecedence: 40,
  }),
  orphanBranch: Object.freeze({
    id: "orphanBranch",
    route: "orphan-branch",
    label: "Orphan branch",
    optionLabel: "Latest orphan branch",
    stateKey: "orphanBranch",
    selectionPrecedence: 30,
  }),
});

const NAV_MENU_TARGETS = Object.freeze([
  Object.freeze({ id: "tip", label: "Live tip", optionLabel: "Live tip" }),
  TARGETS.stale,
  TARGETS.branch,
  TARGETS.orphan,
  TARGETS.orphanBranch,
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
  slot.loaded = false;
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
  slot.loaded = !!payload;
  if (targetId === "stale") {
    slot.anchor = selected ? {
      btc_height: selected.position?.max,
      hash: selected.primary_hash,
      cursor: selected.cursor,
    } : null;
  } else if (targetId === "orphan") {
    slot.anchor = selected ? {
      btc_header_time: selected.position?.max,
      hash: selected.primary_hash,
      cursor: selected.cursor,
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
  if (item.kind === "stale" && item.position?.axis === "height") {
    return {
      mode: "height",
      view: {
        treeHeight: String(item.position.max),
        treeTime: "",
        unheightedAnchor: "",
        treeLookupContext: "compact",
      },
      centerHash: item.primary_hash,
    };
  }
  return null;
}

export function navigatorStepperState(state, targetId) {
  const slot = targetState(state, targetId);
  if (!slot) return { olderEnabled: false, newerEnabled: false, readout: "" };
  if (targetId === "stale") {
    return staleStepperState({
      anchor: slot.anchor,
      total: slot.total,
      hasOlder: slot.hasOlder,
      hasNewer: slot.hasNewer,
    });
  }
  if (targetId === "orphan") {
    return orphanStepperState({
      anchor: slot.anchor,
      total: slot.total,
      hasOlder: slot.hasOlder,
      hasNewer: slot.hasNewer,
    });
  }
  if (targetId === "branch") {
    return branchStepperState({
      item: slot.item,
      total: slot.total,
      hasOlder: slot.hasOlder,
      hasNewer: slot.hasNewer,
    });
  }
  return orphanBranchStepperState({
    item: slot.item,
    total: slot.total,
    hasOlder: slot.hasOlder,
    hasNewer: slot.hasNewer,
  });
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
