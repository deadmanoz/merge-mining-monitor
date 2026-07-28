import { loadCompetitions, loadTree, refreshActiveNavigatorTarget, renderUpdated } from "./api-client.js?v=0.2.1";
import * as deltaView from "./delta-view.js?v=0.2.1";
import { applyTreeHighlight, renderTreePanel } from "./controls.js?v=0.2.1";
import { $, DEFAULTS, esc, state } from "./frontend-state.js?v=0.2.1";
import { syncUrl } from "./tree-query-state.js?v=0.2.1";


/// Top-level views, keyed by their `view=` URL value.
///
/// A registry rather than a branch, for two reasons. It is the single place
/// that decides which views exist, so an unbuilt or misspelled `view=` in a
/// shared URL degrades to the default instead of rendering an empty shell. And
/// a view becomes reachable the moment it is registered, so the shell can land
/// before the views that will use it.
///
/// `load` is idempotent unless `force` is set: it fetches only when the view
/// has no data yet or something has invalidated it. `render` paints from
/// whatever `load` left in `state`.
const VIEWS = new Map();

/// Register a view. `label` names its switcher button; the switcher stays
/// hidden while only one view is registered, so registering the second view is
/// what makes the control appear.
function registerView(id, { label, load, render }) {
  VIEWS.set(id, { id, label, load, render });
}

registerView("tree", {
  label: "Tree",
  async load({ force = false } = {}) {
    // The tree is refetched when it has never been built, when a query mutator
    // retargeted it while another view was active, or when the caller forces a
    // refresh. Otherwise switching back to the tree is instant and keeps the
    // camera exactly where the user left it.
    if (!state.tree || state.treeDirty || force) {
      const [applied] = await Promise.all([loadTree(), refreshActiveNavigatorTarget()]);
      // loadTree returns false when the request failed, was superseded, or did
      // not validate. Clearing treeDirty regardless would strand an
      // already-populated state.tree: it is non-null, so the next activation
      // would skip the reload and leave the stale window on screen.
      if (applied) state.treeDirty = false;
    }
  },
  // Deliberately no camera work. Entering the tree restores the stored pan/zoom,
  // and boot's own focus rule (an explicit tree window plus a focus block) is
  // the only thing that recenters. Centering on any selection here would move a
  // bare `?selected=` away from the tip and discard the stored camera on every
  // return from another view.
  render() {
    // Except for a repaint that came due while the panel was hidden: the
    // geometry it was measured against is gone, so it has to be redone now that
    // the panel has a size again.
    if (state.treeRepaintPending) renderTreePanel();
    // Otherwise reapply the shared Source filter. It can be changed from
    // another view, where nothing touches the tree's DOM, so the cached nodes
    // would still be dimmed by the selection in force when the user left. A
    // repaint already carries the current highlight, hence the else.
    else applyTreeHighlight($("#tree-svg"), state.query.sources, state.query.kinds, state.query.classification, state.selectedHash);
  },
});

// The hash a repair has already SETTLED for: the refreshed snapshot carried it,
// or the loaded block detail says there is nothing to carry. Anything else stays
// retryable, because it is a state the backend can still resolve: a failed
// request, or a read model that lags the block detail and returns 200 without
// the row yet. Retiring on a bare successful response would give up on those
// permanently, leaving the advertised focused state missing until a manual
// Refresh; retrying costs one request per user-initiated activation.
let lastSelectionRefetch = null;

/// True when the selection is missing from the cached snapshot and worth
/// refetching for. The tree refreshes on a timer while competitions load once,
/// so a stale block discovered after the first load would otherwise cross-link
/// into a view that cannot mark, bin or list it.
///
/// The block detail has three states here and they are not interchangeable. Not
/// loaded yet is UNKNOWN, and must be treated as worth trying: it arrives from
/// its own request, so a selection crossed over before it lands would otherwise
/// read as having no competition, skip the repair, and have nothing to retry
/// once it resolved. Loaded and carrying no competition is a definite no, and
/// skipping it matters because activateView also runs on every Source-filter
/// change: a canonical selection would otherwise refetch the whole unpaginated
/// endpoint each time.
function snapshotPredatesSelection() {
  const hash = state.selectedHash;
  if (!hash || hash === lastSelectionRefetch) return false;
  const detail = state.selectedBlock;
  if (detail && detail.block?.hash === hash && !detail.competition) return false;
  return !state.competitions?.some((row) => row.stale_hash === hash);
}

/// Whether there is nothing left for a repair to achieve for `hash`: the
/// snapshot now holds its competition, or the block detail has loaded and
/// conclusively has none.
function repairSettled(hash) {
  if (state.competitions?.some((row) => row.stale_hash === hash)) return true;
  const detail = state.selectedBlock;
  return Boolean(detail && detail.block?.hash === hash && !detail.competition);
}

registerView("delta", {
  label: "Distribution",
  async load({ force = false } = {}) {
    // Competitions change on the order of weeks, so this is a load-once view:
    // the auto-refresh timer never comes here, and only an explicit Refresh
    // (which forces) refetches.
    if (state.competitions && !force && !snapshotPredatesSelection()) return;
    const hash = state.selectedHash;
    await loadCompetitions();
    lastSelectionRefetch = hash && repairSettled(hash) ? hash : null;
  },
  render() {
    // Mounting builds the rail fieldsets and the main panel; it needs the data,
    // so it cannot run before the first load.
    deltaView.mount();
    deltaView.render();
  },
});

/// The one activation path: initial boot, the switcher, cross-links, and
/// manual refresh all go through here, so no route can leave a view unloaded.
async function activateView(view, { force = false } = {}) {
  const target = applyViewScopes(view);
  state.query.view = target;
  syncUrl();
  // The indicator is shared, so it has to follow the view: each carries its own
  // last-received time, and the incoming view's is the honest one to show while
  // its load runs.
  renderUpdated();
  const active = VIEWS.get(target);
  if (!active) return;
  await active.load({ force });
  active.render({ force });
}

/// Stamp the active view on the workspace. The `[data-view-scope]` rules in
/// layout.css do the actual showing and hiding, so no per-control JS.
///
/// Safe to call before `activateView` and independently of it: boot calls it
/// synchronously so the scope is set before first paint. Until `data-view` is
/// stamped, neither scope rule matches and both view panels occupy the same
/// grid slot. It resolves an unregistered view to the default itself, so
/// callers never have to consult the registry.
function applyViewScopes(view) {
  const resolved = VIEWS.has(view) ? view : DEFAULTS.view;
  const workspace = document.querySelector(".workspace");
  if (workspace) workspace.dataset.view = resolved;
  renderViewSwitcher(resolved);
  return resolved;
}

/// One button per registered view, hidden entirely while only one exists.
function renderViewSwitcher(active) {
  const host = $("#view-switcher");
  if (!host) return;
  host.hidden = VIEWS.size < 2;
  if (host.hidden) {
    host.replaceChildren();
    return;
  }
  host.innerHTML = [...VIEWS.values()]
    .map(
      (view) =>
        `<button class="view-tab" type="button" role="tab" data-view="${esc(view.id)}"`
        + ` aria-selected="${view.id === active ? "true" : "false"}">${esc(view.label)}</button>`,
    )
    .join("");
}

function wireViewSwitcher() {
  $("#view-switcher")?.addEventListener("click", (event) => {
    const button = event.target.closest("[data-view]");
    if (!button || button.dataset.view === state.query.view) return;
    activateView(button.dataset.view);
  });
}

/// Test seam: which views are currently reachable.
const registeredViews = () => [...VIEWS.keys()];

export { activateView, applyViewScopes, registerView, registeredViews, wireViewSwitcher };
