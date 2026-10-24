import { loadTree, refreshActiveNavigatorTarget, renderUpdated } from "./api-client.js";
import { applyTreeHighlight, renderTreePanel } from "./controls.js";
import { $, DEFAULTS, esc, state } from "./frontend-state.js";
import { syncUrl } from "./tree-query-state.js";

const VIEWS = new Map();

function registerView(id, { label, load, render }) {
  VIEWS.set(id, { id, label, load, render });
}

registerView("tree", {
  label: "Tree",
  async load({ force = false } = {}) {
    if (!state.tree || state.treeDirty || force) {
      const [applied] = await Promise.all([loadTree(), refreshActiveNavigatorTarget()]);
      if (applied) state.treeDirty = false;
    }
  },
  render() {
    if (state.treeRepaintPending) renderTreePanel();
    else applyTreeHighlight($("#tree-svg"));
  },
});

async function activateView(view, { force = false } = {}) {
  const target = applyViewScopes(view);
  state.query.view = target;
  syncUrl();
  renderUpdated();
  const active = VIEWS.get(target);
  if (!active) return;
  await active.load({ force });
  active.render({ force });
}

function applyViewScopes(view) {
  const resolved = VIEWS.has(view) ? view : DEFAULTS.view;
  const workspace = document.querySelector(".workspace");
  if (workspace) workspace.dataset.view = resolved;
  renderViewSwitcher(resolved);
  return resolved;
}

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

const registeredViews = () => [...VIEWS.keys()];

export { activateView, applyViewScopes, registerView, registeredViews, wireViewSwitcher };
