import { loadTree, refreshActiveNavigatorTarget } from "./api-client.js?v=0.2.1";
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
  render() {},
});

/// The one activation path: initial boot, the switcher, cross-links, and
/// manual refresh all go through here, so no route can leave a view unloaded.
async function activateView(view, { force = false } = {}) {
  const target = applyViewScopes(view);
  state.query.view = target;
  syncUrl();
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
