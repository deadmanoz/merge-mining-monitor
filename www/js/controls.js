import { loadTree, reconcileNavFromSelected, refreshNavControls, selectTreeNode } from "./api-client.js";
import { showDialog } from "./dialogs.js";
import { auxpowHelpFor, consensusRuleHelpFor, errorSummary, renderDrawer } from "./drawer-renderer.js";
import { $, $all, CLASSIFICATION_DEFAULT, compareSourcesForDisplay, DELTA_HELP, EDGE_KINDS, esc, kindHelpFor, KINDS, kvRows, matchesSourceFilter, readForm, SOURCE_GROUPS, sourceChain, sourceDisplayName, sourceGroupKey, sourceMeta, state, VISIBLE_KIND_CONTROLS, writeForm } from "./frontend-state.js";
import { collectCitedReferenceIds, formatCitedText, renderSourceDialog, renderSourcesSection, sourceTagline } from "./source-dialog.js";
import { renderSourceRailStatus } from "./source-status.js";
import { clearTreeViewModes, syncUrl } from "./tree-query-state.js";
import { drawSelectionOverlay, renderTree, renderTreeLegend } from "./tree-renderer.js";


const UI_ICONS = {
  help: '<circle cx="12" cy="12" r="10" /><path d="M9.09 9a3 3 0 1 1 5.82 1c0 2-3 2-3 4" /><path d="M12 17h.01" />',
  close: '<path d="M18 6 6 18" /><path d="m6 6 12 12" />',
};

// Info dialog descriptors driving shell markup and delegated openers.
const INFO_DIALOGS = [
  {
    id: "kind-dialog",
    className: "about-dialog kind-dialog",
    titleId: "kind-dialog-title",
    title: "Classification",
    kickerId: "kind-dialog-kicker",
    bodyId: "kind-dialog-body",
    bodyClassName: "about-dialog-body kind-dialog-body",
    closeId: "kind-dialog-close",
    closeLabel: "Close classification dialog",
    controlsSelector: "#kind-controls",
    dataAttr: "data-kind-info",
    datasetKey: "kindInfo",
    resolve: (key) => kindHelpFor(key),
    entityTitle: (help) => help.name,
    entityKicker: (help) => help.meta,
    renderBody: (_help, key) => renderKindDialog(key),
  },
  {
    id: "source-dialog",
    className: "about-dialog source-dialog",
    titleId: "source-dialog-title",
    title: "Source",
    kickerId: "source-dialog-kicker",
    bodyId: "source-dialog-body",
    bodyClassName: "about-dialog-body source-dialog-body",
    closeId: "source-dialog-close",
    closeLabel: "Close source dialog",
    controlsSelector: "#source-controls",
    dataAttr: "data-source-info",
    datasetKey: "sourceInfo",
    resolve: (key) => sourceByCode(key),
    entityTitle: (source) => sourceDisplayName(source),
    entityKicker: (source) => sourceTagline(source) || sourceMeta(source),
    renderBody: (source) => renderSourceDialog(source),
  },
  {
    id: "delta-dialog",
    className: "about-dialog kind-dialog",
    titleId: "delta-dialog-title",
    title: "Header time delta",
    kickerId: "delta-dialog-kicker",
    bodyId: "delta-dialog-body",
    bodyClassName: "about-dialog-body kind-dialog-body",
    closeId: "delta-dialog-close",
    closeLabel: "Close header time delta dialog",
    delegateFromDocument: true,
    dataAttr: "data-delta-info",
    datasetKey: "deltaInfo",
    resolve: (key) => DELTA_HELP[key],
    entityTitle: (help) => help.name,
    entityKicker: (help) => help.meta,
    renderBody: (help) => help.body.map((paragraph) => `<p>${esc(paragraph)}</p>`).join(""),
  },
  {
    id: "auxpow-dialog",
    className: "about-dialog kind-dialog",
    titleId: "auxpow-dialog-title",
    title: "AuxPoW",
    kickerId: "auxpow-dialog-kicker",
    bodyId: "auxpow-dialog-body",
    bodyClassName: "about-dialog-body kind-dialog-body",
    closeId: "auxpow-dialog-close",
    closeLabel: "Close AuxPoW help dialog",
    delegateFromDocument: true,
    dataAttr: "data-auxpow-info",
    datasetKey: "auxpowInfo",
    resolve: (key) => auxpowHelpFor(key),
    entityTitle: (help) => help.name,
    entityKicker: (help) => help.meta,
    renderBody: (_help, key) => renderAuxpowDialog(key),
  },
  {
    id: "consensus-rule-dialog",
    className: "about-dialog kind-dialog",
    titleId: "consensus-rule-dialog-title",
    title: "Consensus rule",
    kickerId: "consensus-rule-dialog-kicker",
    bodyId: "consensus-rule-dialog-body",
    bodyClassName: "about-dialog-body kind-dialog-body",
    closeId: "consensus-rule-dialog-close",
    closeLabel: "Close consensus rule dialog",
    delegateFromDocument: true,
    dataAttr: "data-consensus-rule-info",
    datasetKey: "consensusRuleInfo",
    resolve: (key) => consensusRuleHelpFor(key) || { name: "Consensus rule", meta: "", body: [] },
    entityTitle: (help) => help.name,
    entityKicker: (help) => help.meta,
    renderBody: (help) => help.body.map((paragraph) => `<p>${esc(paragraph)}</p>`).join(""),
  },
  {
    id: "sources-about-dialog",
    className: "about-dialog kind-dialog",
    titleId: "sources-about-dialog-title",
    title: "About sources",
    kickerId: "sources-about-dialog-kicker",
    bodyId: "sources-about-dialog-body",
    bodyClassName: "about-dialog-body kind-dialog-body",
    closeId: "sources-about-dialog-close",
    closeLabel: "Close about sources dialog",
    delegateFromDocument: true,
    dataAttr: "data-sources-about",
    datasetKey: "sourcesAbout",
    resolve: () => ({}),
    entityTitle: () => "About sources",
    entityKicker: () => "How this monitor classifies the sources it shows",
    renderBody: () => renderSourcesAboutDialog(),
  },
];

function renderAttrs(attrs) {
  return Object.entries(attrs)
    .filter(([, value]) => value !== undefined && value !== null && value !== false)
    .map(([name, value]) => value === true ? ` ${name}` : ` ${name}="${esc(value)}"`)
    .join("");
}

function renderIcon(name) {
  return `<svg class="ui-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${UI_ICONS[name] || ""}</svg>`;
}

function renderIconButton({ id, className = "icon-button", data = {}, ariaLabel, title, icon }) {
  const dataAttrs = Object.fromEntries(Object.entries(data).map(([name, value]) => [`data-${name}`, value]));
  return `<button${renderAttrs({
    id,
    class: className,
    type: "button",
    ...dataAttrs,
    "aria-label": ariaLabel,
    title,
  })}>${renderIcon(icon)}</button>`;
}

function renderCheckboxOption({ name, value, checked, swatchClass, label, meta, infoButton }) {
  const checkedAttr = checked ? " checked" : "";
  const info = infoButton ? renderIconButton(infoButton) : "";
  return `<div class="kind-option">
  <label class="kind-choice">
    <input type="checkbox" name="${esc(name)}" value="${esc(value)}"${checkedAttr} />
    <span class="kind-copy">
      <span class="kind-name"><span class="kind-swatch ${esc(swatchClass)}" aria-hidden="true"></span>${esc(label)}</span>
      <span class="kind-meta">${esc(meta)}</span>
    </span>
  </label>
  ${info}
</div>`;
}

function renderKindControlRows() {
  const kindRows = VISIBLE_KIND_CONTROLS.map((kind) => {
    const help = kindHelpFor(kind);
    return renderCheckboxOption({
      name: "kind",
      value: kind,
      checked: state.query.kinds.includes(kind),
      swatchClass: `fill-${kind}`,
      label: help.name,
      meta: help.meta,
      infoButton: {
        className: "icon-button kind-info-button",
        data: { "kind-info": kind },
        ariaLabel: `About ${help.name.toLowerCase()} parent kind`,
        title: `About ${help.name.toLowerCase()}`,
        icon: "help",
      },
    });
  });
  const classificationRows = CLASSIFICATION_DEFAULT.map((classification) => {
    const help = kindHelpFor(classification);
    return renderCheckboxOption({
      name: "classification",
      value: classification,
      checked: state.query.classification.includes(classification),
      swatchClass: `fill-${classification}`,
      label: help.name,
      meta: help.meta,
      infoButton: {
        className: "icon-button kind-info-button",
        data: { "kind-info": classification },
        ariaLabel: `About ${help.name.toLowerCase()}`,
        title: `About ${help.name.toLowerCase()}`,
        icon: "help",
      },
    });
  });
  return [...kindRows, ...classificationRows].join("");
}

function renderKindControls() {
  const container = $("#kind-controls");
  if (!container) return;
  container.innerHTML = renderKindControlRows();
}

function renderSignalControls() {
  const container = $("#signal-controls");
  if (!container) return;
  container.innerHTML = renderCheckboxOption({
    name: "mixed",
    value: "1",
    checked: state.query.mixed,
    swatchClass: "ring-signal-mixed",
    label: "Mixed signals",
    meta: "Blocks whose tags, payouts and accounts name two or more pools",
    infoButton: {
      className: "icon-button kind-info-button",
      data: { "auxpow-info": "relationship_signals" },
      ariaLabel: "About relationship signals",
      title: "About relationship signals",
      icon: "help",
    },
  });
}

function renderInfoDialogShell(descriptor) {
  return `<dialog id="${esc(descriptor.id)}" class="${esc(descriptor.className)}" aria-labelledby="${esc(descriptor.titleId)}" aria-describedby="${esc(descriptor.bodyId)}">
  <div class="about-dialog-shell">
    <header class="about-dialog-header">
      <div>
        <h2 id="${esc(descriptor.titleId)}">${esc(descriptor.title)}</h2>
        <p id="${esc(descriptor.kickerId)}"></p>
      </div>
      ${renderIconButton({
        id: descriptor.closeId,
        className: "icon-button",
        ariaLabel: descriptor.closeLabel,
        title: "Close",
        icon: "close",
      })}
    </header>
    <div id="${esc(descriptor.bodyId)}" class="${esc(descriptor.bodyClassName)}"></div>
  </div>
</dialog>`;
}

function renderInfoDialogShells() {
  return INFO_DIALOGS.map(renderInfoDialogShell).join("");
}

function renderInfoDialogs() {
  const container = $("#info-dialogs");
  if (!container) return;
  container.innerHTML = renderInfoDialogShells();
}

function renderSourceControlRows(entries) {
  return entries.map((source) => {
    const groupKey = sourceGroupKey(source);
    const disabled = groupKey === "catalogued" || groupKey === "surveyed";
    const checked = state.query.sources.includes(source.code) ? "checked" : "";
    const name = sourceDisplayName(source);
    const status = renderSourceRailStatus(source);
    return `<div class="source-option${disabled ? " source-option-disabled" : ""}">
    <label class="source-choice">
      <input type="checkbox" name="source" value="${esc(source.code)}" ${checked}${disabled ? " disabled" : ""} />
      <span class="source-copy">
        <span class="source-name"><span class="source-name-text">${esc(name)}</span>${status}</span>
        <span class="source-meta">${esc(sourceMeta(source))}</span>
      </span>
    </label>
    ${renderIconButton({
      className: "icon-button source-info-button",
      data: { "source-info": source.code },
      ariaLabel: `About ${name} source`,
      title: `About ${name}`,
      icon: "help",
    })}
  </div>`;
  }).join("");
}

function renderSourceControls(sources) {
  const container = $("#source-controls");
  if (!container) return;
  if (!sources.length) {
    container.innerHTML = `<div class="empty">No source registry</div>`;
    return;
  }
  const grouped = sources
    .slice()
    .sort(compareSourcesForDisplay)
    .reduce((groups, source) => {
      const key = sourceGroupKey(source);
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(source);
      return groups;
    }, new Map());

  container.innerHTML = SOURCE_GROUPS
    .map((group) => [group, grouped.get(group.key) || []])
    .filter(([, entries]) => entries.length)
    .map(([group, entries]) => {
      const headingId = `source-group-${group.key}`;
      const hasSelectedSource = entries.some((source) => state.query.sources.includes(source.code));
      const isOpen = Object.prototype.hasOwnProperty.call(state.sourceGroupOpen, group.key)
        ? state.sourceGroupOpen[group.key]
        : group.defaultOpen || hasSelectedSource;
      const rows = renderSourceControlRows(entries);
      return `<details class="source-group" data-source-group="${esc(group.key)}" aria-labelledby="${headingId}" ${isOpen ? "open" : ""}>
        <summary class="source-group-summary">
          <span class="source-group-heading">
            <span id="${headingId}" class="source-group-title">${esc(group.title)}</span>
            <span class="source-group-heading-actions">
              <span class="source-group-count">${entries.length.toLocaleString("en-US")}</span>
              <span class="source-group-chevron" aria-hidden="true"></span>
            </span>
          </span>
        </summary>
        <div class="source-group-meta">${esc(group.meta)}</div>
        <div class="source-group-list">${rows}</div>
      </details>`;
    })
    .join("");
  $all("details[data-source-group]", container).forEach((details) => {
    details.addEventListener("toggle", () => {
      state.sourceGroupOpen[details.dataset.sourceGroup] = details.open;
    });
  });
  updateSourceGroupSelectedMarkers(container);
}

function updateSourceGroupSelectedMarkers(container = $("#source-controls")) {
  if (!container) return;
  $all("details[data-source-group]", container).forEach((details) => {
    const selectedCount = $all('input[name="source"]:checked', details).length;
    let marker = $(".source-group-selected-count", details);
    if (selectedCount === 0) {
      if (marker) marker.remove();
      return;
    }
    const actions = $(".source-group-heading-actions", details);
    if (!actions) return;
    if (!marker) {
      marker = document.createElement("span");
      marker.className = "source-group-selected-count";
      const total = $(".source-group-count", actions);
      actions.insertBefore(marker, total || actions.firstChild);
    }
    marker.textContent = `${selectedCount.toLocaleString("en-US")} selected`;
  });
}

function sourceByCode(code) {
  return (state.sources?.sources || []).find((source) => source.code === code) || {
    code,
    chain: sourceChain(code),
    kind: String(code || "").split(":")[0] || null,
  };
}

function renderKindDialog(kind) {
  const help = kindHelpFor(kind);
  const refs = help.references || [];
  const cited = collectCitedReferenceIds(help.criteria, help.interpretation, ...help.notes);
  const sourceIds = cited.length ? cited : refs.map((ref) => ref.id);
  const rows = [
    ["Criteria", formatCitedText(help.criteria, refs)],
    ["How To Read It", formatCitedText(help.interpretation, refs)],
  ];
  return [
    ...help.notes.map((note) => `<p>${formatCitedText(note, refs)}</p>`),
    kvRows(rows),
    renderSourcesSection(refs, sourceIds),
  ].join("");
}

function renderSourcesAboutDialog() {
  const classes = kvRows([
    ["Bitcoin Core parent chain", "The live Bitcoin Core node that classifies every recovered parent header. It is the classification authority, not a merge-mined producer."],
    ["Live AuxPoW producer", "A merge-mined chain this monitor polls continuously for new Bitcoin parent-header evidence."],
    ["Recovered dataset", "A historical merge-mined chain whose Bitcoin evidence has been recovered and ingested."],
    ["Recovered subset", "Filterable Bitcoin evidence recovered without the complete child chain. Coverage is limited to the rows described by the source."],
    ["Recovered survey", "The child chain was recovered and checked, but no canonical or stale Bitcoin block winner was found. There are no rows to filter."],
    ["Catalogued (not recovered)", "A chain known to have been merge-mined with Bitcoin, but with no recovered chain data in this monitor. Listed for completeness and greyed in the rail, since there is nothing to filter on yet."],
  ]);
  return [
    `<p>Each source has one of these relationships to this monitor. The rail's Live sources group holds Bitcoin Core and the live AuxPoW producers; each other class has its own group.</p>`,
    classes,
    `<p>A source's <strong>Chain status</strong> row describes the native chain's own state, separately from this monitor's lifecycle and evidence coverage. Active means the chain is currently producing blocks; Zombie means it still produces blocks but at negligible, sub-Bitcoin difficulty; Dormant means it is inactive or its state is uncertain; Dead means it is verified stopped, abandoned, migrated or forked away, or unreachable; Unknown means the current native-chain status has not been established. A live monitor evidence path and a native Active status are separate facts: historical sources can provide recovered evidence without a poller, and an Active chain can lack current monitor evidence.</p>`,
  ].join("");
}

function openInfoDialog(descriptor, key) {
  const dialog = $(`#${descriptor.id}`);
  if (!dialog) return;
  const entity = descriptor.resolve(key);
  const title = $(`#${descriptor.titleId}`);
  const kicker = $(`#${descriptor.kickerId}`);
  const body = $(`#${descriptor.bodyId}`);
  if (title) title.textContent = descriptor.entityTitle(entity);
  if (kicker) kicker.textContent = descriptor.entityKicker(entity);
  if (body) body.innerHTML = descriptor.renderBody(entity, key);
  showDialog(dialog);
}

function renderAuxpowDialog(topic) {
  const help = auxpowHelpFor(topic);
  return help.body.map((paragraph) => `<p>${esc(paragraph)}</p>`).join("");
}

function resetTreeToTip() {
  state.navEpoch += 1;
  state.nav = { target: "tip", source: "navigator" };
  clearTreeViewModes();
  state.selectedHash = null;
  state.selectedBlock = null;
  state.stale.anchor = null;
  state.invalidBlock.anchor = null;
  state.orphan.anchor = null;
  reconcileNavFromSelected();
  writeForm();
  readForm();
  syncUrl();
  renderDrawer();
  markTreeSelection();
  setRailCollapsed(RAILS.drawer, true, { rerender: false });
  refreshNavControls();
  clearStoredTreeTransform();
  loadTree();
}

function renderTreePanel() {
  // Defer painting when the tree panel is hidden so we don't measure 0 width.
  if (!$("#tree-svg")?.clientWidth) {
    state.treeRepaintPending = true;
    return;
  }
  state.treeRepaintPending = false;
  const error = state.errors.tree;
  const status = $("#tree-error");
  if (error) {
    status.dataset.state = "error";
    status.innerHTML = errorSummary(error, "Tree");
    renderTree({ nodes: [], edges: [], window: null, legend: { edge_kinds: EDGE_KINDS } }, $("#tree-svg"), {
      selectedHash: state.selectedHash,
      onSelect: selectTreeNode,
      onBackgroundClick: clearTreeSelection,
    });
    return;
  }
  status.dataset.state = "ok";
  status.textContent = "";
  renderTree(state.tree || { nodes: [], edges: [], window: null, legend: { edge_kinds: EDGE_KINDS } }, $("#tree-svg"), {
    selectedHash: state.selectedHash,
    onSelect: selectTreeNode,
    onBackgroundClick: clearTreeSelection,
  });
  renderTreeLegend(state.tree?.legend);
  updateHeightPlaceholders(state.tree?.window);
}

function updateHeightPlaceholders(window) {
  const form = $("#filters");
  if (!form) return;
  const min = window && window.btc_height_min != null ? String(window.btc_height_min) : "";
  const max = window && window.btc_height_max != null ? String(window.btc_height_max) : "";
  if (form.treeHeight) form.treeHeight.placeholder = max || min;
}

function clearSelection({
  clearOrphanAnchor = false,
  resetSelectionNavigator = false,
} = {}) {
  state.navEpoch += 1;
  state.selectedHash = null;
  state.selectedBlock = null;
  if (clearOrphanAnchor) state.orphan.anchor = null;
  if (resetSelectionNavigator && state.nav.source === "selection") {
    state.nav = { target: "tip", source: "selection" };
  }
  reconcileNavFromSelected();
  refreshNavControls();
  renderDrawer();
  markTreeSelection();
  setRailCollapsed(RAILS.drawer, true);
  syncUrl();
}

function clearTreeSelection() {
  clearSelection({ clearOrphanAnchor: true });
}

function markTreeSelection() {
  const svgEl = $("#tree-svg");
  if (!svgEl) return;
  d3.select(svgEl)
    .selectAll("g.tree-node")
    .attr("data-selected", (node) => node.hash === state.selectedHash);
  const overlay = d3.select(svgEl).select(".tree-overlay-layer");
  const elState = svgEl.__mmmTree;
  if (elState && !overlay.empty()) {
    drawSelectionOverlay(overlay, state.selectedHash, elState.byHash);
  }
  applyTreeHighlight(svgEl);
}

function nodeMatchesSources(node, sources) {
  const nodeSources = node?.source_summary?.sources || [];
  return matchesSourceFilter(nodeSources, sources)
    || matchesSourceFilter(
      (node?.child_chain_evidence || []).map((item) => item.source),
      sources,
    );
}

function nodeMatchesHeaderSignal(node, kinds, classifications) {
  if (node?.kind === "unknown") {
    const cls = node.btc_orphan_class || "pending";
    return Array.isArray(classifications) && classifications.includes(cls);
  }
  return Array.isArray(kinds) && kinds.includes(node?.kind);
}

function nodeMatchesHighlight(node, { sources, kinds, classification, mixed }) {
  return nodeMatchesSources(node, sources)
    && nodeMatchesHeaderSignal(node, kinds, classification)
    && (!mixed || Boolean(node?.relationship?.mixed));
}

function highlightActive({ sources, kinds, classification, mixed }) {
  const sourceActive = Array.isArray(sources) && sources.length > 0;
  const kindActive = Array.isArray(kinds) && kinds.length > 0 && kinds.length < KINDS.length;
  const classificationActive = Array.isArray(classification) && classification.length > 0;
  return sourceActive || kindActive || classificationActive || mixed;
}

function applyTreeHighlight(svgEl) {
  if (!svgEl) return;
  const { query: highlight, selectedHash } = state;
  const active = highlightActive(highlight);
  const byHash = svgEl.__mmmTree?.byHash;
  const root = d3.select(svgEl);
  root.selectAll("g.tree-node")
    .classed("tree-node--dim", (node) => active && node.hash !== selectedHash && !nodeMatchesHighlight(node, highlight));
  const edgeDimmed = (edge) => {
    if (!active || !byHash) return false;
    const from = byHash.get(edge.from_hash);
    const to = byHash.get(edge.to_hash);
    return !(nodeMatchesHighlight(from, highlight) || nodeMatchesHighlight(to, highlight));
  };
  root.selectAll("path.tree-edge").classed("tree-edge--dim", edgeDimmed);
  root.selectAll("g.tree-edge-label").classed("tree-edge--dim", edgeDimmed);
}

function clearStoredTreeTransform() {
  const svgEl = $("#tree-svg");
  if (svgEl?.__mmmTree) svgEl.__mmmTree.transform = null;
}

const RAILS = {
  drawer: {
    widthKey: "mmm-drawer-width",
    collapsedKey: "mmm-drawer-collapsed",
    widthVar: "--drawer-width",
    attr: "drawerCollapsed",
    railSel: ".detail-drawer",
    handleSel: "#drawer-resize",
    collapseBtnSel: "#drawer-collapse",
    edge: "left",
    min: 300,
  },
  filters: {
    widthKey: "mmm-filters-width",
    collapsedKey: "mmm-filters-collapsed",
    widthVar: "--filters-width",
    attr: "filtersCollapsed",
    railSel: ".filter-rail",
    handleSel: "#filters-resize",
    collapseBtnSel: "#filters-collapse",
    edge: "right",
    min: 210,
  },
};

function setRailCollapsed(cfg, collapsed, { persist = true, rerender = true } = {}) {
  const workspace = $(".workspace");
  if (!workspace) return;
  const was = workspace.dataset[cfg.attr] === "true";
  workspace.dataset[cfg.attr] = collapsed ? "true" : "false";
  $(cfg.collapseBtnSel)?.setAttribute("aria-expanded", collapsed ? "false" : "true");
  if (persist) localStorage.setItem(cfg.collapsedKey, collapsed ? "1" : "0");
  if (rerender && was !== collapsed) rerenderTreeAfterLayout();
}

function clampRailWidth(cfg, width) {
  const max = Math.min(760, Math.round(window.innerWidth * 0.55));
  return Math.max(cfg.min, Math.min(max, width));
}

function applyStoredRailWidth(cfg) {
  const workspace = $(".workspace");
  if (!workspace) return;
  const stored = parseInt(localStorage.getItem(cfg.widthKey) || "", 10);
  if (Number.isFinite(stored)) {
    workspace.style.setProperty(cfg.widthVar, `${clampRailWidth(cfg, stored)}px`);
  }
}

function rerenderTreeAfterLayout() {
  window.requestAnimationFrame(() => renderTreePanel());
}

function wireRailResize(cfg) {
  const handle = $(cfg.handleSel);
  const workspace = $(".workspace");
  const rail = $(cfg.railSel);
  if (!handle || !workspace || !rail) return;
  let startX = 0;
  let startWidth = 0;
  const onMove = (event) => {
    const delta = cfg.edge === "left" ? startX - event.clientX : event.clientX - startX;
    workspace.style.setProperty(cfg.widthVar, `${clampRailWidth(cfg, startWidth + delta)}px`);
  };
  const onUp = (event) => {
    handle.releasePointerCapture?.(event.pointerId);
    window.removeEventListener("pointermove", onMove);
    window.removeEventListener("pointerup", onUp);
    const current = parseInt(workspace.style.getPropertyValue(cfg.widthVar), 10);
    if (Number.isFinite(current)) localStorage.setItem(cfg.widthKey, String(current));
    rerenderTreeAfterLayout();
  };
  handle.addEventListener("pointerdown", (event) => {
    if (workspace.dataset[cfg.attr] === "true") return;
    event.preventDefault();
    startX = event.clientX;
    startWidth = rail.getBoundingClientRect().width;
    handle.setPointerCapture?.(event.pointerId);
    window.addEventListener("pointermove", onMove);
    window.addEventListener("pointerup", onUp);
  });
}

export {
  INFO_DIALOGS,
  renderKindControls,
  renderInfoDialogs,
  renderSourceControls,
  updateSourceGroupSelectedMarkers,
  openInfoDialog,
  resetTreeToTip,
  renderTreePanel,
  markTreeSelection,
  applyTreeHighlight,
  renderSignalControls,
  clearStoredTreeTransform,
  clearSelection,
  RAILS,
  setRailCollapsed,
  applyStoredRailWidth,
  wireRailResize,
};
