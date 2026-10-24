import { centerCameraOnHeight, centerCameraOnNode, centerCameraOnWindowMidHeight, goTo, loadBlock, loadNavigatorLatest, loadSources, loadTree, navSelectLabel, reconcileNavFromSelected, refreshActiveNavigatorTarget, refreshNavControls, stepNav } from "./api-client.js";
import { applyStoredRailWidth, applyTreeHighlight, clearStoredTreeTransform, INFO_DIALOGS, openInfoDialog, RAILS, renderInfoDialogs, renderKindControls, renderSignalControls, setRailCollapsed, updateSourceGroupSelectedMarkers, wireRailResize } from "./controls.js";
import { activateModalTab, closeDialog, showDialog, wireModalTabs } from "./dialogs.js";
import { renderDrawer } from "./drawer-renderer.js";
import { $, $all, API_BASE, esc, hydrateFormFromUrl, readForm, refreshRelativeTimes, startRelativeTimeTicker, state, writeForm } from "./frontend-state.js";
import { anyNavTargetBusy, NAV_COARSE_STRIDE, navMenuTargets, resetNavigatorTargetState } from "./nav-targets.js";
import { wireSourceStatusPopover } from "./source-status.js";
import { inputDateTimeToUtc } from "./tree-lookup.js";
import { hasExplicitTreeView, hasManualTreeLookup, hasUnheightedAnchorView, syncUrl, treeWindowError } from "./tree-query-state.js";
import { wireTreeLegend } from "./tree-renderer.js";
import { prepareFocus, registerDeltaView } from "./delta-view.js";
import { registerFindingsView } from "./findings-view.js";
import { activateView, applyViewScopes, registerView, wireViewSwitcher } from "./view-shell.js";

async function reloadAll() {
  readForm();
  syncUrl();
  clearStoredTreeTransform();
  renderDrawer();
  state.treeDirty = true;
  await Promise.all([loadSources(), activateView(state.query.view)]);
  const focus = state.selectedHash
    || (hasUnheightedAnchorView() ? state.query.unheightedAnchor : null);
  if (state.query.view === "tree" && hasExplicitTreeView() && focus) {
    centerCameraOnNode(focus);
  }
}

function refreshBlocked() {
  return anyNavTargetBusy(state) || state.treePointerActive;
}

async function manualRefresh() {
  if (refreshBlocked()) return;
  await Promise.all([loadSources(), activateView(state.query.view, { force: true })]);
  refreshRelativeTimes();
}

async function scheduledRefresh() {
  if (refreshBlocked()) return;
  const pending = [loadSources()];
  if (state.query.view === "tree") {
    pending.push(loadTree(), refreshActiveNavigatorTarget());
  } else {
    state.treeDirty = true;
  }
  await Promise.all(pending);
  refreshRelativeTimes();
}

const REFRESH_INTERVAL_MS = 60_000;

function startRefresh() {
  setInterval(scheduledRefresh, REFRESH_INTERVAL_MS);
  $("#last-updated")?.addEventListener("click", manualRefresh);
}

function wireAboutDialog() {
  const dialog = $("#about-dialog");
  const open = $("#about-button");
  const close = $("#about-close");
  if (!dialog || !open || !close) return;

  const resetScreens = wireAboutScreens(dialog);
  wireModalTabs(dialog);
  wireReleaseNotesAccordion(dialog);

  open.addEventListener("click", () => {
    resetScreens();
    activateModalTab(dialog, "overview");
    showDialog(dialog);
  });
  close.addEventListener("click", () => closeDialog(dialog));
  dialog.addEventListener("click", (event) => {
    if (event.target === dialog) closeDialog(dialog);
  });
}

function setReleaseExpanded(head, expand) {
  head.setAttribute("aria-expanded", String(expand));
  const body = document.getElementById(head.getAttribute("aria-controls"));
  if (body) body.hidden = !expand;
}

function syncToggleAllLabel(toggleAll, body) {
  const heads = body ? $all(".rel-head", body) : [];
  if (!toggleAll || !heads.length) return;
  const anyCollapsed = heads.some((head) => head.getAttribute("aria-expanded") !== "true");
  toggleAll.textContent = anyCollapsed ? "Expand all" : "Collapse all";
}

function wireReleaseNotesAccordion(dialog) {
  const body = $("#about-release-notes-body", dialog);
  const toggleAll = $("#about-notes-toggle-all", dialog);
  if (body) {
    body.addEventListener("click", (event) => {
      const head = event.target.closest(".rel-head");
      if (head && body.contains(head)) {
        setReleaseExpanded(head, head.getAttribute("aria-expanded") !== "true");
        syncToggleAllLabel(toggleAll, body);
      }
    });
  }
  if (toggleAll && body) {
    toggleAll.addEventListener("click", () => {
      const heads = $all(".rel-head", body);
      const expand = heads.some((head) => head.getAttribute("aria-expanded") !== "true");
      heads.forEach((head) => setReleaseExpanded(head, expand));
      syncToggleAllLabel(toggleAll, body);
    });
  }
}

function versionLabel(version) {
  if (!version) return "";
  return version.startsWith("v") ? version : `v${version}`;
}

const RELEASE_CARET = `<svg class="ui-icon rel-caret" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6" /></svg>`;

function releaseTag(release, index, latestReleasedIndex) {
  if (release.version === "Unreleased") {
    return `<span class="rel-tag rel-tag-dev">In development</span>`;
  }
  if (index === latestReleasedIndex) {
    return `<span class="rel-tag rel-tag-latest">Latest</span>`;
  }
  return "";
}

function renderReleaseSection(release, index, source, latestReleasedIndex) {
  const items = release.items || [];
  const open = index === 0;
  const bodyId = `about-rel-${index}`;
  const itemCount = release.item_count || items.length;
  const remaining = Math.max(0, itemCount - items.length);
  const more = remaining
    ? `<p class="rel-more">${remaining} more ${remaining === 1 ? "entry" : "entries"} in ${esc(source)}.</p>`
    : "";
  const date = release.date ? ` <span class="rel-date">(${esc(release.date)})</span>` : "";
  const version = release.version === "Unreleased" ? "Unreleased" : esc(versionLabel(release.version));
  return `<section class="rel">
    <h3 class="rel-h">
      <button class="rel-head" type="button" aria-expanded="${open}" aria-controls="${bodyId}">
        <span class="rel-id"><span class="rel-ver">${version}</span>${date}${releaseTag(release, index, latestReleasedIndex)}</span>
        <span class="rel-meta"><span class="rel-count" title="${itemCount} ${itemCount === 1 ? "entry" : "entries"}">${itemCount}</span>${RELEASE_CARET}</span>
      </button>
    </h3>
    <div class="rel-body" id="${bodyId}"${open ? "" : " hidden"}>
      <ul class="rel-list">${items.map((item) => `<li>${esc(item)}</li>`).join("")}</ul>
      ${more}
    </div>
  </section>`;
}

function renderReleaseNotes(payload) {
  const tablist = $("#about-tablist");
  const tab = $("#about-tab-notes");
  const body = $("#about-release-notes-body");
  const meta = $("#about-notes-meta");
  const foot = $("#about-notes-foot");
  const releases = payload?.release_notes?.releases || [];
  if (!body || !releases.length) return;

  const source = payload?.release_notes?.source || "RELEASE_NOTES.md";
  const releaseCount = payload?.release_notes?.release_count || releases.length;
  const latestReleasedIndex = releases.findIndex((release) => release.version !== "Unreleased");

  body.innerHTML = releases
    .map((release, index) => renderReleaseSection(release, index, source, latestReleasedIndex))
    .join("");

  if (meta) {
    meta.textContent = `${releaseCount} ${releaseCount === 1 ? "release" : "releases"} · ${source}`;
  }
  const hiddenReleases = Math.max(0, releaseCount - releases.length);
  if (foot) {
    const truncated = Boolean(payload?.release_notes?.truncated) && hiddenReleases > 0;
    foot.hidden = !truncated;
    foot.textContent = truncated
      ? `Showing ${releases.length} of ${releaseCount} releases. ${hiddenReleases} older ${hiddenReleases === 1 ? "section" : "sections"} in ${source}.`
      : "";
  }

  syncToggleAllLabel($("#about-notes-toggle-all"), body);
  if (tablist) tablist.hidden = false;
  if (tab) tab.hidden = false;
}

function renderVersionInfo(payload) {
  const version = $("#about-version");
  if (version && payload?.version) {
    version.textContent = versionLabel(payload.version);
    version.hidden = false;
  }
  renderReleaseNotes(payload);
}

async function loadVersionInfo() {
  try {
    const response = await fetch(`${API_BASE}/version`, { headers: { accept: "application/json" } });
    const payload = await response.json().catch(() => null);
    if (response.ok && payload && !payload.error) renderVersionInfo(payload);
  } catch {
    // Version metadata is auxiliary.
  }
}

function wireAboutScreens(dialog) {
  const screens = $all(".explainer-screen", dialog);
  if (!screens.length) return () => {};
  const dots = $all(".explainer-dot", dialog);
  const prev = $(".explainer-prev", dialog);
  const next = $(".explainer-next", dialog);
  const total = screens.length;
  let index = 0;

  function render() {
    screens.forEach((screen, i) => {
      screen.hidden = i !== index;
    });
    dots.forEach((dot, i) => {
      if (i === index) dot.setAttribute("aria-current", "true");
      else dot.removeAttribute("aria-current");
    });
    const active = screens[index];
    active.classList.remove("is-entering");
    void active.offsetWidth;
    active.classList.add("is-entering");
  }

  function go(to) {
    index = (to + total) % total;
    render();
  }

  prev?.addEventListener("click", () => go(index - 1));
  next?.addEventListener("click", () => go(index + 1));
  dots.forEach((dot, i) => dot.addEventListener("click", () => go(i)));
  dialog.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    if (event.target.closest(".modal-tab")) return;
    const overview = $("#about-overview", dialog);
    if (overview && overview.hidden) return;
    event.preventDefault();
    go(event.key === "ArrowLeft" ? index - 1 : index + 1);
  });

  render();
  return () => go(0);
}

function wireInfoDialog(descriptor) {
  const dialog = $(`#${descriptor.id}`);
  const close = $(`#${descriptor.closeId}`);
  if (!dialog || !close) return;
  const container = descriptor.delegateFromDocument ? document : $(descriptor.controlsSelector);
  if (!container) return;

  container.addEventListener("click", (event) => {
    const button = event.target.closest(`[${descriptor.dataAttr}]`);
    if (!button) return;
    event.preventDefault();
    event.stopPropagation();
    openInfoDialog(descriptor, button.dataset[descriptor.datasetKey]);
  });
  close.addEventListener("click", () => closeDialog(dialog));
  dialog.addEventListener("click", (event) => {
    if (event.target === dialog) closeDialog(dialog);
  });
}

function renderNavTargetOptions() {
  const select = $("#nav-goto");
  if (!select) return;
  let prompt = select.querySelector('option[value=""]');
  if (!prompt) {
    prompt = document.createElement("option");
    prompt.value = "";
  }
  prompt.hidden = true;
  prompt.selected = true;
  prompt.textContent = navSelectLabel();
  const options = navMenuTargets().map((target) => {
    const option = document.createElement("option");
    option.value = target.id;
    option.textContent = target.optionLabel;
    return option;
  });
  select.replaceChildren(prompt, ...options);
}

function handleClassificationFilterChange() {
  state.navEpoch += 1;
  resetNavigatorTargetState(state, "orphanBranch");
  if (hasManualTreeLookup()) {
    refreshNavControls();
    loadTree();
  } else if (state.nav.target === "orphanBranch") {
    loadNavigatorLatest("orphanBranch");
  } else if (hasUnheightedAnchorView() || state.nav.target === "orphan") {
    loadNavigatorLatest("orphan");
  } else {
    refreshNavControls();
  }
}

function isRedundantTreeLookupCommit(field, value, siblingValue) {
  if (value === "" || siblingValue !== "") return false;
  if (field === "height") {
    return state.query.treeHeight === value
      && state.query.treeTime === ""
      && state.query.unheightedAnchor === "";
  }
  const utc = inputDateTimeToUtc(value);
  return utc !== ""
    && state.query.treeTime === utc
    && state.query.treeHeight === ""
    && state.query.unheightedAnchor === "";
}

function commitTreeLookup({ field }) {
  const form = $("#filters");
  if (!form) return false;
  const input = field === "height" ? form.treeHeight : form.treeTime;
  const sibling = field === "height" ? form.treeTime : form.treeHeight;
  const value = String(input?.value ?? "").trim();
  const siblingValue = String(sibling?.value ?? "").trim();
  if (value === "" && siblingValue !== "") return false;
  if (value === "" && !hasManualTreeLookup()) return false;

  const recenter = field === "height"
    ? () => centerCameraOnHeight(value)
    : () => centerCameraOnWindowMidHeight();

  if (isRedundantTreeLookupCommit(field, value, siblingValue)) {
    state.navEpoch += 1;
    const rendered = !state.errors.tree && (field === "height"
      ? (state.tree?.query?.at_height != null && Number(state.tree.query.at_height) === Number(value))
      : (state.tree?.query?.at_time != null && state.tree.query.at_time === state.query.treeTime));
    if (rendered) recenter();
    else loadTree().then((applied) => { if (applied) recenter(); });
    return true;
  }
  if (value !== "" && sibling) sibling.value = "";
  state.navEpoch += 1;
  readForm({ source: "lookup-commit" });
  writeForm();
  if (!treeWindowError()) syncUrl();
  if (!hasUnheightedAnchorView() && state.orphan.anchor) {
    state.orphan.anchor = null;
    reconcileNavFromSelected();
  }
  refreshNavControls();
  loadTree().then((applied) => { if (applied) recenter(); });
  return true;
}

function wireTreeLookupCommitEvents(form) {
  const height = form.treeHeight;
  const time = form.treeTime;
  const enterCommit = { height: null, time: null };
  const commitFor = (field) => ({ field });
  const recordEnterCommit = (field, input) => {
    enterCommit[field] = { value: input?.value ?? "" };
  };
  const shouldSuppressChange = (field, input) => {
    const commit = enterCommit[field];
    if (!commit) return false;
    enterCommit[field] = null;
    return commit.value === (input?.value ?? "");
  };
  const clearEnterCommitAfterEdit = (field, input) => {
    if (enterCommit[field]?.value !== (input?.value ?? "")) enterCommit[field] = null;
  };
  height?.addEventListener("input", () => clearEnterCommitAfterEdit("height", height));
  time?.addEventListener("input", () => clearEnterCommitAfterEdit("time", time));
  height?.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    event.stopImmediatePropagation();
    if (commitTreeLookup(commitFor("height"))) recordEnterCommit("height", height);
  });
  time?.addEventListener("keydown", (event) => {
    if (event.key !== "Enter") return;
    event.preventDefault();
    event.stopImmediatePropagation();
    if (commitTreeLookup(commitFor("time"))) recordEnterCommit("time", time);
  });
  height?.addEventListener("change", (event) => {
    event.stopPropagation();
    if (shouldSuppressChange("height", height)) return;
    commitTreeLookup(commitFor("height"));
  });
  time?.addEventListener("change", (event) => {
    event.stopPropagation();
    if (shouldSuppressChange("time", time)) return;
    commitTreeLookup(commitFor("time"));
  });
}

function wireEvents() {
  const filters = $("#filters");
  wireTreeLookupCommitEvents(filters);
  filters.addEventListener("change", (event) => {
    if (event.target && (event.target.name === "treeHeight" || event.target.name === "treeTime")) return;
    readForm({ source: "filter-change" });
    syncUrl();
    if (event.target && ["source", "kind", "mixed"].includes(event.target.name)) {
      if (event.target.name === "source") updateSourceGroupSelectedMarkers();
      if (state.query.view === "tree") {
        applyTreeHighlight($("#tree-svg"));
      } else {
        activateView(state.query.view);
      }
    } else if (event.target && event.target.name === "classification") {
      handleClassificationFilterChange();
    }
  });
  startRefresh();
  wireViewSwitcher();
  wireAboutDialog();
  INFO_DIALOGS.forEach(wireInfoDialog);
  wireModalTabs($("#source-dialog"));
  wireSourceStatusPopover();
  renderNavTargetOptions();
  $("#nav-goto")?.addEventListener("change", (event) => {
    // Reset to the hidden prompt so re-picking the same action still fires.
    const target = event.target.value;
    event.target.value = "";
    if (target) goTo(target);
  });
  $("#nav-coarse-older")?.addEventListener("click", () => stepNav("older", NAV_COARSE_STRIDE));
  $("#nav-older")?.addEventListener("click", () => stepNav("older", 1));
  $("#nav-newer")?.addEventListener("click", () => stepNav("newer", 1));
  $("#nav-coarse-newer")?.addEventListener("click", () => stepNav("newer", NAV_COARSE_STRIDE));
  wireTreeLegend();
  $("#theme-button").addEventListener("click", () => {
    const next = document.documentElement.dataset.theme === "dark" ? "" : "dark";
    document.documentElement.dataset.theme = next;
    localStorage.setItem("mmm-theme", next);
  });
  $("#drawer-collapse").addEventListener("click", () => setRailCollapsed(RAILS.drawer, true));
  $("#drawer-reopen").addEventListener("click", () => setRailCollapsed(RAILS.drawer, false));
  $("#filters-collapse").addEventListener("click", () => setRailCollapsed(RAILS.filters, true));
  $("#filters-reopen").addEventListener("click", () => setRailCollapsed(RAILS.filters, false));
  document.addEventListener("click", (event) => {
    if (!event.target.closest('[data-action="delta"]')) return;
    prepareFocus();
    activateView("delta");
  });
  document.addEventListener("click", (event) => {
    const topic = event.target.closest("[data-scroll-topic]")?.dataset.scrollTopic;
    if (!topic) return;
    $(`#drawer .detail-section[data-topic="${topic}"]`)?.scrollIntoView({ block: "start", behavior: "smooth" });
  });
  document.addEventListener("click", async (event) => {
    const copy = event.target.closest("[data-copy]");
    if (!copy) return;
    if (!navigator.clipboard?.writeText) return;
    await navigator.clipboard.writeText(copy.dataset.copy);
    copy.dataset.copied = "true";
    copy.setAttribute("aria-label", "Copied");
    copy.title = "Copied";
    window.setTimeout(() => {
      delete copy.dataset.copied;
      copy.setAttribute("aria-label", "Copy value");
      copy.title = "Copy value";
    }, 900);
  });
}

async function initApp() {
  document.documentElement.dataset.theme = localStorage.getItem("mmm-theme") || "";
  hydrateFormFromUrl();
  registerDeltaView(registerView);
  registerFindingsView(registerView);
  applyViewScopes(state.query.view);
  renderKindControls();
  renderSignalControls();
  renderInfoDialogs();
  writeForm();
  wireEvents();
  applyStoredRailWidth(RAILS.filters);
  applyStoredRailWidth(RAILS.drawer);
  wireRailResize(RAILS.filters);
  wireRailResize(RAILS.drawer);
  loadVersionInfo();
  setRailCollapsed(RAILS.filters, localStorage.getItem(RAILS.filters.collapsedKey) === "1", {
    persist: false,
    rerender: false,
  });
  const drawerStored = localStorage.getItem(RAILS.drawer.collapsedKey);
  setRailCollapsed(RAILS.drawer, state.selectedHash ? false : drawerStored !== "0", {
    persist: false,
    rerender: false,
  });
  renderDrawer();
  startRelativeTimeTicker();
  await reloadAll();
  if (state.selectedHash) loadBlock(state.selectedHash);
}

export {
  initApp,
};
