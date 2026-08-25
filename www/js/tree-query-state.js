import { API_BASE, CLASSIFICATION_DEFAULT, classificationParam, DEFAULTS, KINDS, sameSet, state, VISIBLE_KIND_CONTROLS } from "./frontend-state.js?v=0.7.0";
import { inputDateTimeToUtc } from "./tree-lookup.js?v=0.7.0";


function hasExactHeightLookup() {
  return state.query.treeHeight !== "";
}

function hasExactTimeLookup() {
  return state.query.treeTime !== "";
}

function hasUnheightedAnchorView() {
  return !!state.query.unheightedAnchor;
}

function hasGeneratedTreeWindow() {
  return state.query.treeWindow === "generated"
    && state.query.treeFrom !== ""
    && state.query.treeTo !== "";
}

function hasManualTreeLookup() {
  return hasExactHeightLookup() || hasExactTimeLookup();
}

function hasExplicitTreeView() {
  return hasManualTreeLookup() || hasUnheightedAnchorView() || hasGeneratedTreeWindow();
}

function clearGeneratedWindowState() {
  state.treeDirty = true;
  state.query.treeWindow = "";
  state.query.treeFrom = "";
  state.query.treeTo = "";
  state.query.treeTargetHeight = "";
}

function activateHeightLookup(height, { context = "compact" } = {}) {
  state.treeDirty = true;
  state.query.treeHeight = String(height);
  state.query.treeTime = "";
  state.query.treeLookupContext = context;
  state.query.unheightedAnchor = "";
  clearGeneratedWindowState();
}

function activateAnchorView(hash) {
  state.treeDirty = true;
  state.query.unheightedAnchor = hash;
  state.query.treeHeight = "";
  state.query.treeTime = "";
  state.query.treeLookupContext = "compact";
  clearGeneratedWindowState();
}

function activateGeneratedWindow({ treeFrom, treeTo, targetHeight } = {}) {
  state.treeDirty = true;
  state.query.treeWindow = "generated";
  state.query.treeFrom = String(treeFrom);
  state.query.treeTo = String(treeTo);
  state.query.treeTargetHeight = targetHeight == null ? "" : String(targetHeight);
  state.query.treeHeight = "";
  state.query.treeTime = "";
  state.query.treeLookupContext = "compact";
  state.query.unheightedAnchor = "";
}

function clearTreeViewModes() {
  state.treeDirty = true;
  state.query.treeHeight = "";
  state.query.treeTime = "";
  state.query.treeLookupContext = "compact";
  state.query.unheightedAnchor = "";
  clearGeneratedWindowState();
}

function syncUrl() {
  const params = new URLSearchParams();
  const q = state.query;
  // Only non-default views are written, so every existing tree URL round-trips
  // byte-identically to before the view parameter existed.
  if (q.view && q.view !== DEFAULTS.view) params.set("view", q.view);
  // Owned by the delta view; written here so one function builds the URL.
  if (q.era) params.set("era", q.era);
  // Owned by the findings view. Serialized ONLY while findings is active: the
  // shared activation path has no view-exit hook, so an unconditional write
  // would leak `?view=delta&finding=<slug>` after switching views. The
  // in-memory slug survives the switch so returning to findings restores the
  // open article.
  if (q.view === "findings" && q.finding) params.set("finding", q.finding);
  if (hasUnheightedAnchorView()) {
    params.set("unheighted_anchor", q.unheightedAnchor);
  } else if (hasGeneratedTreeWindow()) {
    params.set("tree_window", "generated");
    params.set("tree_from", q.treeFrom);
    params.set("tree_to", q.treeTo);
    if (q.treeTargetHeight !== "") params.set("tree_height", q.treeTargetHeight);
  } else if (hasExactHeightLookup()) {
    params.set("tree_height", q.treeHeight);
  } else if (hasExactTimeLookup()) {
    params.set("tree_time", q.treeTime);
  }
  if (!sameSet(q.kinds, VISIBLE_KIND_CONTROLS)) params.set("kinds", q.kinds.join(","));
  if (!sameSet(q.classification, CLASSIFICATION_DEFAULT)) {
    params.set("classification", q.classification.join(","));
  }
  if (q.sources.length) params.set("sources", q.sources.join(","));
  if (state.selectedHash) params.set("selected", state.selectedHash);
  const url = `${window.location.pathname}?${params.toString()}`;
  window.history.replaceState(null, "", url);
}

function paramsFor(base) {
  const params = new URLSearchParams();
  Object.entries(base).forEach(([key, value]) => {
    if (value !== undefined && value !== null && value !== "") params.set(key, value);
  });
  return params;
}

function treePath(view) {
  const v = view || state.query;
  const base = { kinds: KINDS.join(",") };
  if (v.unheightedAnchor) {
    base.unheighted_anchor = v.unheightedAnchor;
    base.classification = classificationParam();
  } else if (v.treeWindow === "generated" && v.treeFrom !== "" && v.treeTo !== "") {
    base.from_height = v.treeFrom;
    base.to_height = v.treeTo;
  } else if (v.treeHeight != null && v.treeHeight !== "") {
    base.at_height = v.treeHeight;
    if (v.treeLookupContext !== "exact") {
      base.context = "compact";
      base.classification = classificationParam();
    }
  } else if (v.treeTime != null && v.treeTime !== "") {
    base.at_time = v.treeTime;
    if (v.treeLookupContext !== "exact") {
      base.context = "compact";
      base.classification = classificationParam();
    }
  }
  return `${API_BASE}/tree?${paramsFor(base)}`;
}

function treeWindowError() {
  if (state.query.treeHeight !== "") {
    const height = Number(state.query.treeHeight);
    if (!Number.isInteger(height) || height < 0) return "Height must be a non-negative whole number.";
    return null;
  }
  if (state.query.treeTime !== "") {
    if (!inputDateTimeToUtc(state.query.treeTime)) return "Date/time must be a valid UTC timestamp.";
    return null;
  }
  return null;
}

export {
  hasUnheightedAnchorView,
  hasManualTreeLookup,
  hasExplicitTreeView,
  activateHeightLookup,
  activateAnchorView,
  activateGeneratedWindow,
  clearTreeViewModes,
  syncUrl,
  paramsFor,
  treePath,
  treeWindowError,
};
