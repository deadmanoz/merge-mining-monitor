import { clearStoredTreeTransform, markTreeSelection, RAILS, renderSourceControls, renderTreePanel, resetTreeToTip, setRailCollapsed } from "./controls.js";
import { renderDrawer } from "./drawer-renderer.js";
import { $, API_BASE, CLASSIFICATION_DEFAULT, CLASSIFICATION_META, classificationParam, CLASSIFICATIONS, readForm, state, writeForm } from "./frontend-state.js";
import { anyNavTargetBusy, applyNavigatorPayload, isNavigatorTarget, navSelectionMatches, navSelectLabelForState, navigatorItemForStep, navigatorItemView, navigatorRoute, navigatorStepperState, selectionTargetForState, setNavigatorBusy, targetState } from "./nav-targets.js";
import { renderSourceStatus } from "./source-status.js";
import { activateAnchorView, activateGeneratedWindow, activateHeightLookup, paramsFor, syncUrl, treePath, treeWindowError } from "./tree-query-state.js";
import { anchorCameraOnTip, renderTree } from "./tree-renderer.js";
import {
  NAV_COARSE_STRIDE,
  orphanAnchorFromBlock,
} from "./windowing.js";


async function fetchJson(key, path) {
  const seq = (state.seq[key] || 0) + 1;
  state.seq[key] = seq;
  try {
    const response = await fetch(path, { headers: { accept: "application/json" } });
    const payload = await response.json().catch(() => ({}));
    if (seq !== state.seq[key]) return null;
    if (!response.ok || payload.error) {
      const error = payload.error || {
        code: `http_${response.status}`,
        message: response.statusText || "HTTP error",
        details: {},
      };
      throw error;
    }
    delete state.errors[key];
    return payload;
  } catch (error) {
    if (seq === state.seq[key]) state.errors[key] = error;
    return null;
  }
}

async function loadSources() {
  // Show a loading state while the (count-aggregating, sometimes slow) /sources
  // query is in flight, but only when no source checkboxes are rendered yet, so
  // a Refresh never blanks an already-populated panel.
  const container = $("#source-controls");
  if (container && !container.querySelector('input[name="source"]')) {
    container.innerHTML = `<div class="loading">Loading sources</div>`;
  }
  const payload = await fetchJson("sources", `${API_BASE}/sources`);
  if (payload) {
    state.sources = payload;
    const sources = payload.sources || [];
    renderSourceControls(sources);
    renderSourceStatus(sources);
  } else {
    renderSourceControls([]);
    renderSourceStatus([], { unavailable: true });
  }
}

function refreshActiveNavigatorTarget() {
  const target = state.nav.target;
  const hash = isNavigatorTarget(target) ? targetState(state, target)?.item?.primary_hash : null;
  return hash ? hydrateNavigatorAnchor(target, hash) : Promise.resolve();
}

function loadStaleBranches() {
  return loadNavigatorLatest("branch");
}

function loadOrphanBranches() {
  return loadNavigatorLatest("orphanBranch");
}

// The label shown in the Go-to select's (hidden, selected) prompt so it names the
// active target being stepped: a navigator target, the live tip, or an exact
// lookup/context window.
function navSelectLabel() {
  return navSelectLabelForState(state);
}

// A compact per-class count summary for the navigator readout, scoped to the
// currently-selected orphan classes (so toggling a class on surfaces its count),
// e.g. "strict 16 · weak 14". Empty until counts have loaded.
function orphanCountsText(counts) {
  if (!counts) return "";
  const selected = state.query.classification?.length
    ? state.query.classification
    : CLASSIFICATION_DEFAULT;
  return CLASSIFICATIONS.filter((cls) => selected.includes(cls))
    .map((cls) => {
      const meta = CLASSIFICATION_META[cls];
      const n = counts[meta.count];
      return n == null ? null : `${meta.count} ${Number(n).toLocaleString("en-US")}`;
    })
    .filter(Boolean)
    .join(" · ");
}

// Apply the active target's stepper state to the shared navigator: the Go-to
// select's displayed label (which target is being stepped), the << < > >> buttons,
// and the readout (position within that target). Disabled while a jump is in
// flight. The select stays on its hidden prompt so re-picking an option re-fires.
function refreshNavControls() {
  const goto = $("#nav-goto");
  const coarseOlder = $("#nav-coarse-older");
  const older = $("#nav-older");
  const newer = $("#nav-newer");
  const coarseNewer = $("#nav-coarse-newer");
  const readout = $("#nav-readout");
  if (!older || !newer || !coarseOlder || !coarseNewer || !readout) return;
  // Name the active target in the select's prompt (the option it rests on).
  const prompt = goto?.querySelector('option[value=""]');
  if (prompt) prompt.textContent = navSelectLabel();

  let olderEnabled = false;
  let newerEnabled = false;
  let text = "";
  if (isNavigatorTarget(state.nav.target)) {
    const st = navigatorStepperState(state, state.nav.target);
    ({ olderEnabled, newerEnabled, readout: text } = st);
    if (state.nav.target === "orphan") {
      const countsText = orphanCountsText(state.orphan.counts);
      if (text && countsText) text = `${text} · ${countsText}`;
      else if (countsText) text = countsText;
    }
  }
  // tip: no stepping, empty readout.
  const busy = anyNavTargetBusy(state);
  older.disabled = busy || !olderEnabled;
  coarseOlder.disabled = busy || !olderEnabled;
  newer.disabled = busy || !newerEnabled;
  coarseNewer.disabled = busy || !newerEnabled;
  readout.textContent = text;
}

// Re-derive each target's cursor from the selection, then settle the active
// target: keep a navigator-locked target while the selection still belongs to
// it, otherwise recompute by registry precedence. A plain canonical / no-match
// click leaves the active target as-is.
function reconcileNavFromSelected() {
  const matches = navSelectionMatches(state);
  if (state.nav.source === "navigator" && matches[state.nav.target]) return;
  const target = selectionTargetForState(state);
  if (target) state.nav = { target, source: "selection" };
}

// The Go-to menu: jump to the latest of the chosen target (or reset to tip) and
// lock the target as a navigator choice.
function goTo(target) {
  state.navEpoch += 1;
  if (target === "tip") {
    resetTreeToTip();
    return;
  }
  state.nav = { target, source: "navigator" };
  if (isNavigatorTarget(target)) loadNavigatorLatest(target);
}

// The shared << < > >> stepper dispatches to the active target. `direction` is
// "older" or "newer"; `stride` is 1 (inner) or NAV_COARSE_STRIDE (outer).
function stepNav(direction, stride) {
  state.navEpoch += 1;
  if (isNavigatorTarget(state.nav.target)) stepNavigator(state.nav.target, direction, stride);
}

// One-shot camera center on a node, using the tree's stored zoom behavior and
// laid-out positions. Callers await the matching loadTree before centering, so a
// node missing from the rendered layout means the window genuinely omitted the
// target (not a benign interleave): fall back to anchoring the tip and warn,
// rather than leaving a stale camera. Because softRefresh preserves the stored
// transform and this sets it last, once centered the target stays centered across
// later refreshes.
function centerCameraOnNode(hash) {
  const svgEl = $("#tree-svg");
  const elState = svgEl?.__mmmTree;
  if (!elState || !elState.zoom) return;
  const node = elState.byHash?.get(hash);
  if (!node) {
    // The target block is not in the rendered window (e.g. the backend window did
    // not include it). Don't silently leave a stale camera after a jump: anchor on
    // the layout tip as a fresh render would, and report the miss for diagnosis
    // rather than returning invisibly.
    if (anchorCameraOnTip(svgEl)) {
      console.warn(`centerCameraOnNode: "${hash}" absent from rendered window; anchored on tip`);
    }
    return;
  }
  const width = Math.max(520, svgEl.clientWidth || 780);
  const height = Math.max(340, svgEl.clientHeight || 420);
  d3.select(svgEl).call(
    elState.zoom.transform,
    d3.zoomIdentity.translate(width / 2 - node.x, height / 2 - node.y).scale(1),
  );
}

// True when the just-selected block sits near (or past) a viewport edge. A click
// recenters only then, so a block comfortably in view is left where it is and
// routine clicking does not jerk the camera. The "near" band is a fraction of the
// viewport on each side; defaults to true (center) when the node cannot be
// measured (e.g. not in the rendered window).
function clickedNodeNeedsCenter() {
  const svgEl = $("#tree-svg");
  const nodeEl = svgEl?.querySelector('g.tree-node[data-selected="true"]');
  if (!svgEl || !nodeEl) return true;
  const n = nodeEl.getBoundingClientRect();
  const s = svgEl.getBoundingClientRect();
  if (!n.width || !n.height || !s.width || !s.height) return true;
  const padX = s.width * 0.12;
  const padY = s.height * 0.12;
  return n.left < s.left + padX || n.right > s.right - padX
    || n.top < s.top + padY || n.bottom > s.bottom - padY;
}

// Click-to-center: load the block, then recenter ONLY if it is near a viewport
// edge, once the drawer-open reflow has settled. loadBlock's setRailCollapsed
// queues its reflow rAF synchronously, so this rAF (queued after) measures the
// block at the final tree width; when the drawer was already open there is no
// reflow. The captured-epoch + selectedHash guard drops a stale center if a newer
// gesture (another click, a jump, Live tip, or a deselect) supersedes this one
// before the rAF fires, so a deferred center cannot lock a stale camera.
function loadBlockThenCenter(hash) {
  loadBlock(hash);
  const epoch = state.navEpoch;
  requestAnimationFrame(() => {
    if (state.navEpoch !== epoch || state.selectedHash !== hash) return;
    if (clickedNodeNeedsCenter()) centerCameraOnNode(hash);
  });
}

// Center on the block at an entered tree height. Several nodes can share a BTC
// height (a canonical block plus its stale competitor(s)), so pick deterministically
// and canonical-first: the canonical at the EXACT height (the spine), else any block
// at the exact height, else the nearest heighted block; ties break canonical-first
// then by hash order. No-op when there are no heighted nodes or the value is not
// finite. centerCameraOnNode stays safe if the chosen node is absent from byHash.
function centerCameraOnHeight(heightStr) {
  const target = Number(heightStr);
  if (!Number.isFinite(target)) return;
  const heighted = (state.tree?.nodes || []).filter((node) => node.height != null);
  if (!heighted.length) return;
  const exact = heighted.filter((node) => Number(node.height) === target);
  const pool = exact.length ? exact : heighted;
  const best = pool.reduce((current, node) => {
    if (!current) return node;
    const currentCanon = current.kind === "canonical";
    const nodeCanon = node.kind === "canonical";
    if (nodeCanon !== currentCanon) return nodeCanon ? node : current;
    const currentDist = Math.abs(Number(current.height) - target);
    const nodeDist = Math.abs(Number(node.height) - target);
    if (nodeDist !== currentDist) return nodeDist < currentDist ? node : current;
    return String(node.hash) < String(current.hash) ? node : current;
  }, null);
  if (best) centerCameraOnNode(best.hash);
}

// Center on the rendered window's mid-height. Used for a date/time lookup: the
// backend resolves the timestamp to a canonical block and builds the compact
// window AROUND it, but does not echo which block, so the landing sits ~mid-window
// and centering the mid-height lands within a block or two of it. That is ample
// for the "go to roughly this date" intent; exact time landing would need a
// backend target echo. No-op when no bounded window is rendered.
function centerCameraOnWindowMidHeight() {
  const w = state.tree?.window;
  if (!w || w.btc_height_min == null || w.btc_height_max == null) return;
  centerCameraOnHeight(Math.round((w.btc_height_min + w.btc_height_max) / 2));
}

function navigatorParams(target, params = {}) {
  const req = { ...params };
  if (target === "orphan" || target === "orphanBranch") {
    req.classification = classificationParam();
  }
  return paramsFor(req);
}

function navigatorUrl(target, params = {}) {
  const route = navigatorRoute(target);
  if (!route) return null;
  const query = navigatorParams(target, params);
  return `${API_BASE}/navigator/${route}${query ? `?${query}` : ""}`;
}

async function loadNavigatorLatest(target) {
  const epoch = state.navEpoch;
  state.nav = { target, source: "navigator" };
  setNavigatorBusy(state, target, true);
  refreshNavControls();
  const payload = await fetchJson(`${target}Navigator`, navigatorUrl(target, { limit: 1 }));
  setNavigatorBusy(state, target, false);
  if (epoch !== state.navEpoch) { refreshNavControls(); return; }
  if (!payload) {
    refreshNavControls();
    return;
  }
  const item = payload.items?.[0] ?? null;
  if (!item) {
    applyNavigatorPayload(state, target, payload, null);
    resetTreeToTip();
    return;
  }
  jumpToNavigatorItem(target, item, payload);
}

async function stepNavigator(target, direction, stride) {
  const slot = targetState(state, target);
  const cursor = slot?.item?.cursor;
  if (!cursor || slot.busy) return;
  const epoch = state.navEpoch;
  setNavigatorBusy(state, target, true);
  refreshNavControls();
  const payload = await fetchJson(
    `${target}Navigator`,
    navigatorUrl(target, { cursor, direction, limit: Math.max(1, Number(stride) || 1) }),
  );
  setNavigatorBusy(state, target, false);
  if (epoch !== state.navEpoch || !payload) {
    refreshNavControls();
    return;
  }
  const item = navigatorItemForStep(payload, direction);
  if (!item) {
    if (direction === "older") slot.hasOlder = false;
    else slot.hasNewer = false;
    if (payload.total != null) slot.total = payload.total;
    refreshNavControls();
    return;
  }
  jumpToNavigatorItem(target, item, payload);
}

async function jumpToNavigatorAnchor(target, hash) {
  const epoch = state.navEpoch;
  state.nav = { target, source: "navigator" };
  setNavigatorBusy(state, target, true);
  refreshNavControls();
  const payload = await fetchJson(
    `${target}NavigatorAnchor`,
    navigatorUrl(target, { anchor_hash: hash, limit: 1 }),
  );
  setNavigatorBusy(state, target, false);
  // Superseded by a newer gesture: the user moved on, so drop this result.
  if (epoch !== state.navEpoch) {
    refreshNavControls();
    return;
  }
  // The anchor fetch failed (null) or carried no navigable item, but this click is
  // still the current gesture. Fall back to plain block detail so selecting a node
  // ALWAYS opens the drawer rather than silently no-opping.
  const item = payload?.items?.[0] ?? null;
  if (!item) {
    if (payload) applyNavigatorPayload(state, target, payload, null);
    loadBlockThenCenter(hash);
    return;
  }
  jumpToNavigatorItem(target, item, payload);
}

async function hydrateNavigatorAnchor(target, hash) {
  const payload = await fetchJson(
    `${target}NavigatorHydrate`,
    navigatorUrl(target, { anchor_hash: hash, limit: 1 }),
  );
  if (state.selectedHash !== hash || !payload) return;
  const item = payload.items?.[0] ?? null;
  applyNavigatorPayload(state, target, payload, item);
  reconcileNavFromSelected();
  refreshNavControls();
}

async function jumpToNavigatorItem(target, item, payload) {
  const jump = navigatorItemView(item);
  if (!jump) return;
  if (jump.mode === "unavailable") {
    state.nav = { target, source: "navigator" };
    applyNavigatorPayload(state, target, payload, item);
    state.treeLoadSeq += 1;
    state.seq.tree = (state.seq.tree || 0) + 1;
    state.tree = null;
    state.errors.tree = navigationErrorForTree(jump.error);
    renderTreePanel();
    refreshNavControls();
    return;
  }
  let commitView = jump.view;
  const epoch = state.navEpoch;
  state.nav = { target, source: "navigator" };
  setNavigatorBusy(state, target, true);
  refreshNavControls();
  // Expand the drawer BEFORE loading/centering. Otherwise loadBlock expands it
  // after we center, which reflows the grid, narrows the tree SVG, and re-renders
  // reusing the now-wrong wide-width transform, knocking the centered target
  // off-screen. Opening it first lays the tree out at its final width; loadBlock's
  // later expand is then a no-op.
  setRailCollapsed(RAILS.drawer, false);
  // Load the target view WITHOUT committing query/URL yet.
  // If a newer gesture supersedes this jump mid-load, loadTree's epoch gate drops
  // the response and nothing else was mutated, so no canceled view leaks into the
  // query/URL.
  const applied = await loadTree(commitView);
  setNavigatorBusy(state, target, false);
  // Abort the commit if this jump did not win: a newer nav gesture superseded it,
  // or its view load was overtaken (e.g. by a softRefresh) so the displayed tree
  // is NOT this window. Either way, do not commit the URL or center/select.
  if (epoch !== state.navEpoch || !applied) { refreshNavControls(); return; }
  // Won: now commit the view to query + form + URL, then clear the prior camera
  // and center on the target.
  applyNavigatorPayload(state, target, payload, item);
  if (jump.mode === "height" && commitView?.treeHeight) {
    activateHeightLookup(commitView.treeHeight, { context: commitView.treeLookupContext });
  } else if (jump.mode === "anchor" && commitView?.unheightedAnchor) {
    activateAnchorView(commitView.unheightedAnchor);
  } else if (jump.mode === "window" && commitView?.treeWindow === "generated") {
    activateGeneratedWindow({
      treeFrom: commitView.treeFrom,
      treeTo: commitView.treeTo,
      targetHeight: commitView.treeTargetHeight,
    });
  } else {
    return;
  }
  writeForm();
  readForm({ source: jump.mode === "window" ? "generated-window" : "form" });
  syncUrl();
  clearStoredTreeTransform();
  centerCameraOnNode(jump.centerHash);
  loadBlock(jump.centerHash);
}

function navigationErrorForTree(error = {}) {
  return {
    code: error.code || "target_backbone_unsynced",
    message: error.message || "Bitcoin Core backbone is not synced for this navigation target",
    details: {
      target_height: error.target_height ?? null,
      action: error.action || "run sync-bitcoin-core",
    },
  };
}

function loadStalesLatest() {
  return loadNavigatorLatest("stale");
}

function loadOrphansLatest() {
  return loadNavigatorLatest("orphan");
}

// Tree node click dispatch (renderTree passes only the hash). An orphan-class
// unknown node re-anchors the orphan view; any other kind opens block detail.
function selectTreeNode(hash) {
  state.navEpoch += 1;
  const node = state.tree?.nodes?.find((entry) => entry.hash === hash);
  const branchId = node?.branch?.branch_id || "";
  if (branchId.startsWith("orphan-")) {
    jumpToNavigatorAnchor("orphanBranch", hash);
  } else if (node && node.kind === "unknown") {
    jumpToNavigatorAnchor("orphan", hash);
  } else {
    loadBlockThenCenter(hash);
  }
}

// Load the tree for `view` (a jump's target window/anchor) or, by default, the
// committed state.query. A jump passes a view so it can render its target without
// mutating state.query/URL until it wins. Returns true only when THIS call applied
// its own payload, so a jump commits (URL/center/select) only when its view won;
// false means superseded (by a newer nav gesture or by another tree fetch sharing
// the "tree" seq key, e.g. a softRefresh) or errored.
async function loadTree(view) {
  // Claim ownership of the shared tree state for this load. A newer loadTree (a
  // jump, softRefresh, or a validation-only filter edit that never fetches) bumps
  // this, so a stale load below leaves state.tree / state.errors.tree untouched.
  const myLoad = ++state.treeLoadSeq;
  // Reject an out-of-range window before fetching so the user gets instant
  // feedback and the backend is never asked to build an oversized tree. Only the
  // default (state.query) path can carry a user-entered window; jump views are
  // always pre-bounded, so they skip this.
  if (!view) {
    const windowError = treeWindowError();
    if (windowError) {
      // Invalidate any in-flight "tree" FETCH so its fetchJson sees a seq mismatch
      // and returns without its success path clearing this validation error (the
      // validation does not fetch, so it would not otherwise bump the fetch seq).
      // treeLoadSeq above guards loadTree's own branches; this guards fetchJson's.
      state.seq.tree = (state.seq.tree || 0) + 1;
      state.errors.tree = { code: "invalid_range", message: windowError };
      state.tree = null;
      renderTreePanel();
      return false;
    }
  }
  delete state.errors.tree;
  $("#tree-error").textContent = "Loading tree";
  $("#tree-error").dataset.state = "loading";
  // Capture the nav epoch too: a nav gesture (a node click, a deselect) can
  // supersede a jump WITHOUT starting a competing tree load, so treeLoadSeq alone
  // would not catch it. Non-nav loads (initial, softRefresh, filter edits that
  // bump the epoch just before calling this) match.
  const epoch = state.navEpoch;
  let payload = await fetchJson("tree", treePath(view));
  // A newer loadTree started while this was in flight: it owns the shared tree and
  // error state now, so leave both untouched (do not clear its validation error or
  // apply this stale tree). Do NOT renderTreePanel here either: painting the stale
  // state.tree against a just-cleared (null) transform would re-anchor on old
  // geometry and lock elState.transform, so the owning load's render would preserve
  // the stale camera. The owning load paints when it settles.
  if (myLoad !== state.treeLoadSeq) {
    return false;
  }
  if (epoch !== state.navEpoch) {
    // Superseded only by a nav gesture (no newer tree load). Discard this
    // response, including any request error fetchJson recorded for it, so a
    // canceled load cannot paint a stale error over the current view.
    delete state.errors.tree;
    renderTreePanel();
    return false;
  }
  if (payload) {
    state.tree = payload;
    markUpdated();
    renderTreePanel();
    return true;
  }
  // payload null: a fetch error (its error is already recorded and current). This
  // call did not win, so a jump must not commit.
  renderTreePanel();
  return false;
}

// Stamp the top-right freshness indicator with the current local time. Called on
// every successful tree load (initial, refresh, auto-refresh, Live tip, height
// change), so it reflects when the view last received data.
function markUpdated() {
  const el = $("#last-updated");
  if (!el) return;
  const time = new Date().toLocaleTimeString([], {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
  el.textContent = `Updated ${time}`;
}

function staleAnchorFromBlock(payload, hash) {
  const block = payload?.block;
  if (!block || block.kind !== "stale") return null;
  const height = Number(block.height);
  if (!Number.isInteger(height)) return null;
  const staleHash = payload?.competition?.stale_hash || block.hash || hash;
  if (staleHash !== hash) return null;
  return { btc_height: height, hash };
}

function selectedNodeBranchTarget(hash) {
  const node = state.tree?.nodes?.find((entry) => entry.hash === hash);
  const branchId = node?.branch?.branch_id
    ?? state.selectedBlock?.stale_branch?.branch_id
    ?? "";
  if (branchId.startsWith("stale-")) return "branch";
  if (branchId.startsWith("orphan-")) return "orphanBranch";
  return null;
}

async function loadBlock(hash) {
  state.selectedHash = hash;
  state.selectedBlock = null;
  // Selecting any block re-derives the active target. Navigator cursors are
  // hydrated below through the unified anchor_hash mode.
  reconcileNavFromSelected();
  refreshNavControls();
  // Selecting a block always reveals the detail panel if it was collapsed.
  setRailCollapsed(RAILS.drawer, false);
  $("#drawer").innerHTML = `<div class="loading">Loading block</div>`;
  syncUrl();
  markTreeSelection();
  const payload = await fetchJson("block", `${API_BASE}/block/${encodeURIComponent(hash)}`);
  if (payload) state.selectedBlock = payload;
  const staleAnchor = staleAnchorFromBlock(payload, hash);
  const anchor = orphanAnchorFromBlock(payload, hash, state.query.classification);
  reconcileNavFromSelected();
  refreshNavControls();
  renderDrawer();

  const branchTarget = selectedNodeBranchTarget(hash);
  if (branchTarget) hydrateNavigatorAnchor(branchTarget, hash);
  else if (staleAnchor) hydrateNavigatorAnchor("stale", hash);
  if (anchor && branchTarget !== "orphanBranch") hydrateNavigatorAnchor("orphan", hash);
}

export {
  loadSources,
  refreshActiveNavigatorTarget,
  loadOrphanBranches,
  navSelectLabel,
  refreshNavControls,
  reconcileNavFromSelected,
  goTo,
  stepNav,
  centerCameraOnNode,
  centerCameraOnHeight,
  centerCameraOnWindowMidHeight,
  loadOrphansLatest,
  selectTreeNode,
  loadTree,
  loadBlock,
};
