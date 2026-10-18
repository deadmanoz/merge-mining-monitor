// Findings view: editorial feed and article canvas over the generated findings corpus.

import { markUpdated } from "./api-client.js?v=0.9.3";
import { INFO_DIALOGS, openInfoDialog } from "./controls.js?v=0.9.3";
import { FINDINGS } from "./findings.generated.js?v=0.9.3";
import { CATEGORIES, STATUSES, renderArticle, renderFeed } from "./findings-render.js?v=0.9.3";
import { $, matchesSourceFilter, state } from "./frontend-state.js?v=0.9.3";
import { showInTree } from "./tree-jump.js?v=0.9.3";
import { syncUrl } from "./tree-query-state.js?v=0.9.3";

let mounted = false;

function registerFindingsView(registerView) {
  registerView("findings", {
    label: "Findings",
    async load({ force = false } = {}) {
      if (!state.findings || force) {
        state.findings = FINDINGS;
        markUpdated("findings");
      }
    },
    render() {
      mount();
      paint();
    },
  });
}

function visibleFindings() {
  const ui = state.findingsUi;
  return state.findings.filter(
    (f) =>
      !ui.hideCategories.includes(f.category)
      && !ui.hideStatuses.includes(f.status)
      && matchesSourceFilter(f.affected_sources, state.query.sources),
  );
}

// Paints the article named by `state.query.finding`, or falls back to the feed.
function paint() {
  const scroller = $("#findings-scroll");
  if (!scroller) return;
  const slug = state.query.finding;
  if (slug) {
    const index = state.findings.findIndex((f) => f.slug === slug);
    if (index === -1) {
      state.query.finding = "";
      syncUrl();
    } else {
      scroller.innerHTML = renderArticle(state.findings[index], index, state.findings.length);
      scroller.scrollTop = 0;
      return;
    }
  }
  scroller.innerHTML = renderFeed(visibleFindings(), state.findings.length);
  scroller.scrollTop = state.findingsUi.feedScroll;
}

function openFinding(slug) {
  const scroller = $("#findings-scroll");
  if (scroller && !state.query.finding) state.findingsUi.feedScroll = scroller.scrollTop;
  state.query.finding = slug;
  syncUrl();
  paint();
}

function closeArticle() {
  state.query.finding = "";
  syncUrl();
  paint();
}

function stepArticle(delta) {
  const index = state.findings.findIndex((f) => f.slug === state.query.finding);
  if (index === -1) return;
  const next = state.findings[index + delta];
  if (next) openFinding(next.slug);
}

function dispatchAnchor(button) {
  const kind = button.dataset.anchorKind;
  const value = button.dataset.anchorValue;
  if (kind === "btc-height") {
    showInTree(Number(value));
  } else if (kind === "source") {
    const descriptor = INFO_DIALOGS.find((d) => d.id === "source-dialog");
    if (descriptor) openInfoDialog(descriptor, value);
  }
}

function filterGroup(legend, entries, hidden) {
  const rows = Object.entries(entries)
    .map(([value, meta]) => {
      const dot = meta.color ? `<span class="finding-cat-dot" style="--cat: ${meta.color}"></span>` : "";
      return `<label class="finding-filter-row">
        <input type="checkbox" name="finding-${legend.toLowerCase()}" value="${value}"
          ${hidden.includes(value) ? "" : "checked"} />${dot}<span>${meta.label}</span>
      </label>`;
    })
    .join("");
  return `<fieldset><legend>${legend}</legend><div class="stack-sm">${rows}</div></fieldset>`;
}

function mount() {
  if (mounted) return;
  mounted = true;

  $("#findings-controls").innerHTML =
    filterGroup("Category", CATEGORIES, state.findingsUi.hideCategories)
    + filterGroup("Status", STATUSES, state.findingsUi.hideStatuses);
  $("#findings-main").innerHTML =
    '<section class="findings-card" aria-label="Findings"><div id="findings-scroll" class="findings-scroll"></div></section>';

  $("#findings-controls").addEventListener("change", (event) => {
    const box = event.target.closest("input[type=checkbox]");
    if (!box) return;
    const list = box.name === "finding-category"
      ? state.findingsUi.hideCategories
      : state.findingsUi.hideStatuses;
    const at = list.indexOf(box.value);
    if (box.checked && at !== -1) list.splice(at, 1);
    if (!box.checked && at === -1) list.push(box.value);
    state.findingsUi.feedScroll = 0;
    paint();
  });

  $("#findings-main").addEventListener("click", (event) => {
    if (event.target.closest("a")) return;
    const card = event.target.closest("[data-finding]");
    if (card) {
      openFinding(card.dataset.finding);
      return;
    }
    const action = event.target.closest("[data-action]");
    if (action) {
      if (action.dataset.action === "findings-back") closeArticle();
      if (action.dataset.action === "findings-newer") stepArticle(-1);
      if (action.dataset.action === "findings-older") stepArticle(1);
      return;
    }
    const anchor = event.target.closest("[data-anchor-kind]");
    if (anchor) dispatchAnchor(anchor);
  });
}

export { registerFindingsView };
