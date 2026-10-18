import { loadBlock, loadCompetitions } from "./api-client.js?v=0.9.3";
import { clearSelection, updateSourceGroupSelectedMarkers } from "./controls.js?v=0.9.3";
import { renderContext, renderCoverage, renderHistogram } from "./delta-chart.js?v=0.9.3";
import { applyOutliersOpen, forgetScroll, renderOutliers, revealFocusedRow } from "./delta-outliers.js?v=0.9.3";
import {
  binKeyFor,
  clamp,
  computeBins,
  fmtDelta,
  fmtInt,
  fmtPct,
  fmtSpan,
  fmtTick,
  partitionByDelta,
  quantile,
} from "./delta-scales.js?v=0.9.3";
import { $, esc, matchesSourceFilter, parseEra, state } from "./frontend-state.js?v=0.9.3";
import { syncUrl } from "./tree-query-state.js?v=0.9.3";
import { showInTree } from "./tree-jump.js?v=0.9.3";

const PRESETS = [
  { label: "±10s", half: 10 },
  { label: "±30s", half: 30 },
  { label: "±1m", half: 60 },
  { label: "±2m", half: 120 },
  { label: "±5m", half: 300 },
  { label: "±10m", half: 600 },
  { label: "±1h", half: 3600 },
  { label: "±6h", half: 21600 },
  { label: "Full", half: null },
];

const view = {
  half: 120,
  binWidth: "auto",
  yscale: "linear",
  tab: "histogram",
  outliersOpen: true,
};

let mounted = false;
let lastSelectionRefetch = null;

const rows = () => state.competitions ?? [];

const absMax = (usable) => usable.reduce((max, row) => Math.max(max, Math.abs(row.header_time_delta_s)), 1);

function snapshotPredatesSelection() {
  const hash = state.selectedHash;
  if (!hash || hash === lastSelectionRefetch) return false;
  const detail = state.selectedBlock;
  if (detail && detail.block?.hash === hash && !detail.competition) return false;
  return !state.competitions?.some((row) => row.stale_hash === hash);
}

function repairSettled(hash) {
  if (state.competitions?.some((row) => row.stale_hash === hash)) return true;
  const detail = state.selectedBlock;
  return Boolean(detail && detail.block?.hash === hash && !detail.competition);
}

function registerDeltaView(registerView) {
  registerView("delta", {
    label: "Distribution",
    async load({ force = false } = {}) {
      if (state.competitions && !force && !snapshotPredatesSelection()) return;
      const hash = state.selectedHash;
      await loadCompetitions();
      lastSelectionRefetch = hash && repairSettled(hash) ? hash : null;
    },
    render() {
      mount();
      render();
    },
  });
}

const FIRST_YEAR = 2009;
const lastPlausibleYear = () => new Date().getUTCFullYear() + 1;

function yearOf(row) {
  const year = new Date(row.stale_header_time * 1000).getUTCFullYear();
  if (!Number.isFinite(year)) return null;
  return year >= FIRST_YEAR && year <= lastPlausibleYear() ? year : null;
}

function filtered() {
  const sources = state.query.sources ?? [];
  const all = rows();
  return all.filter((row) => {
    if (!matchesSourceFilter(row.sources, sources)) return false;
    const year = yearOf(row);
    if (year === null) return true;
    return year >= view.yearFrom && year <= view.yearTo;
  });
}

function mount() {
  if (mounted) return;
  syncEraBounds();
  const [from, to] = parseEra(state.query.era) ?? [];
  view.yearFrom = clamp(from ?? view.yearMin, view.yearMin, view.yearMax);
  view.yearTo = clamp(to ?? view.yearMax, view.yearFrom, view.yearMax);
  if (state.query.era) syncEraParam();

  buildControls();
  buildMain();
  wireControls();
  mounted = true;
}

function syncEraBounds() {
  const years = rows().map(yearOf).filter((year) => year !== null);
  const min = years.length ? Math.min(...years) : FIRST_YEAR;
  const max = years.length ? Math.max(...years) : lastPlausibleYear();
  if (min === view.yearMin && max === view.yearMax) return false;
  const wasFullRange = view.yearFrom === view.yearMin && view.yearTo === view.yearMax;
  view.yearMin = min;
  view.yearMax = max;
  view.yearFrom = wasFullRange ? min : clamp(view.yearFrom ?? min, min, max);
  view.yearTo = wasFullRange ? max : clamp(view.yearTo ?? max, view.yearFrom, max);
  return true;
}

function rebuildYearOptions() {
  const from = $("#delta-year-from");
  const to = $("#delta-year-to");
  if (!from || !to) return;
  from.innerHTML = yearOptions(view.yearFrom);
  to.innerHTML = yearOptions(view.yearTo);
}

const yearOptions = (selected) => Array
  .from({ length: view.yearMax - view.yearMin + 1 }, (_, i) => view.yearMin + i)
  .map((year) => `<option value="${year}"${year === selected ? " selected" : ""}>${year}</option>`)
  .join("");

function buildControls() {
  $("#delta-controls").innerHTML = `
    <fieldset>
      <legend>Focus window</legend>
      <div id="delta-presets" class="preset-grid" role="group" aria-label="Focus window preset">
        ${PRESETS.map((preset) => `<button class="button preset-button" type="button" data-half="${preset.half ?? ""}" aria-pressed="false">${esc(preset.label)}</button>`).join("")}
      </div>
      <p class="fieldset-hint">Everything outside the window stays visible in the full-range strip and the outlier list.</p>
    </fieldset>
    <fieldset>
      <legend>Bin width</legend>
      <label><span class="visually-hidden">Bin width</span>
        <select id="delta-bin-width" aria-label="Bin width">
          ${["auto", 1, 2, 5, 10, 30, 60].map((v) => `<option value="${v}">${v === "auto" ? "Auto" : `${v} s`}</option>`).join("")}
        </select>
      </label>
    </fieldset>
    <fieldset>
      <legend>Count scale</legend>
      <div class="stack-sm">
        <label class="radio-row"><input type="radio" name="deltaScale" value="linear" checked /> <span>Linear</span></label>
        <label class="radio-row"><input type="radio" name="deltaScale" value="log" /> <span>Log (1 + count)</span></label>
      </div>
      <p class="fieldset-hint">Log lifts the thin shoulders without hiding the mode.</p>
    </fieldset>
    <fieldset>
      <legend>Era</legend>
      <div class="grid-form-2">
        <label>From <select id="delta-year-from">${yearOptions(view.yearFrom)}</select></label>
        <label>To <select id="delta-year-to">${yearOptions(view.yearTo)}</select></label>
      </div>
      <p class="fieldset-hint">Header clocks were far looser before ~2016.</p>
    </fieldset>`;
}

function buildMain() {
  $("#delta-main").innerHTML = `
    <div class="section-heading">
      <div>
        <h2 id="delta-title">Header Time Delta</h2>
        <div id="delta-meta" class="meta-line"></div>
      </div>
      <div class="tree-heading-actions">
        <button id="delta-about" class="icon-button" type="button" data-delta-info="metric"
                aria-label="About the header time delta" title="About the header time delta">
          <svg class="ui-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="12" cy="12" r="10" /><path d="M9.09 9a3 3 0 1 1 5.82 1c0 2-3 2-3 4" /><path d="M12 17h.01" /></svg>
        </button>
        <div class="view-tabs" role="tablist" aria-label="Chart view">
          <button class="delta-tab" type="button" role="tab" data-tab="histogram" aria-selected="true">Distribution</button>
          <button class="delta-tab" type="button" role="tab" data-tab="coverage" aria-selected="false">Coverage</button>
          <button class="delta-tab" type="button" role="tab" data-tab="table" aria-selected="false">Table</button>
        </div>
      </div>
    </div>
    <div id="delta-stats" class="stat-row"></div>
    <div class="delta-body">
      <div class="delta-plot">
        <div id="delta-legend" class="chart-legend"></div>
        <div id="delta-canvas" class="delta-canvas">
          <svg id="delta-chart" class="delta-svg" role="img" aria-labelledby="delta-chart-title"><title id="delta-chart-title"></title></svg>
          <div id="delta-tooltip" class="chart-tooltip" role="status" hidden></div>
        </div>
        <div id="delta-table-panel" class="delta-panel table-panel" hidden>
          <div class="table-scroll">
            <table class="data-table">
              <caption class="visually-hidden">Header time delta counts per bin</caption>
              <thead><tr><th scope="col">Bin</th><th scope="col">Range (s)</th><th scope="col">Count</th><th scope="col">Share</th><th scope="col">Cumulative</th></tr></thead>
              <tbody id="delta-table-body"></tbody>
            </table>
          </div>
        </div>
      </div>
      <section id="delta-outliers" class="outlier-panel" data-open="true">
        <h3 class="outlier-summary">
          <button id="delta-outliers-toggle" class="outlier-toggle" type="button"
                  aria-expanded="true" aria-controls="delta-outlier-body">
            Outside window <span id="delta-outlier-count" class="outlier-count"></span>
          </button>
        </h3>
        <div id="delta-outlier-body"></div>
      </section>
      <div class="context-strip">
        <div class="context-heading">
          <h3>Full range</h3>
          <span id="delta-context-note" class="meta-line"></span>
        </div>
        <div id="delta-context-canvas" class="delta-canvas context-canvas">
          <svg id="delta-context" class="delta-svg" role="img" aria-labelledby="delta-context-title"><title id="delta-context-title"></title></svg>
        </div>
      </div>
    </div>
    <div id="delta-status" class="status-line"></div>`;
}

function wireControls() {
  $("#delta-presets").addEventListener("click", (event) => {
    const button = event.target.closest("[data-half]");
    if (!button) return;
    view.half = button.dataset.half === "" ? null : Number(button.dataset.half);
    render();
  });
  $("#delta-bin-width").addEventListener("change", (event) => {
    view.binWidth = event.target.value;
    render();
  });
  for (const radio of document.querySelectorAll("[name='deltaScale']")) {
    radio.addEventListener("change", (event) => {
      view.yscale = event.target.value;
      render();
    });
  }
  $("#delta-year-from").addEventListener("change", (event) => {
    view.yearFrom = Number(event.target.value);
    if (view.yearFrom > view.yearTo) {
      view.yearTo = view.yearFrom;
      $("#delta-year-to").value = String(view.yearTo);
    }
    syncEraParam();
    render();
  });
  $("#delta-year-to").addEventListener("change", (event) => {
    view.yearTo = Number(event.target.value);
    if (view.yearTo < view.yearFrom) {
      view.yearFrom = view.yearTo;
      $("#delta-year-from").value = String(view.yearFrom);
    }
    syncEraParam();
    render();
  });
  for (const tab of document.querySelectorAll(".delta-tab")) {
    tab.addEventListener("click", () => {
      view.tab = tab.dataset.tab;
      for (const other of document.querySelectorAll(".delta-tab")) {
        other.setAttribute("aria-selected", String(other === tab));
      }
      render();
    });
  }
  $("#delta-outlier-body").addEventListener("click", (event) => {
    const button = event.target.closest("[data-outlier-index]");
    if (!button) return;
    if (button.dataset.action === "tree") {
      showInTree(Number(button.dataset.height));
      return;
    }
    if (button.dataset.action === "reveal") {
      revealSelection();
      return;
    }
    selectRow(button.dataset.hash);
  });
  $("#delta-outliers-toggle").addEventListener("click", () => {
    view.outliersOpen = !view.outliersOpen;
    applyOutliersOpen(view.outliersOpen !== false);
    revealFocusedRow();
    renderCharts();
  });
  applyOutliersOpen(view.outliersOpen !== false);
  const observer = new ResizeObserver(() => renderCharts());
  observer.observe($("#delta-canvas"));
  observer.observe($("#delta-context-canvas"));
}

function selectRow(hash) {
  if (state.selectedHash === hash) {
    clearSelection({ resetSelectionNavigator: true });
    render();
    return;
  }
  state.navEpoch += 1;
  loadBlock(hash);
  render();
}

function render() {
  if (!mounted) return;
  if (syncEraBounds()) {
    rebuildYearOptions();
    syncEraParam();
  }
  const all = filtered();
  const { usable, unavailable } = partitionByDelta(all);
  const binning = computeBins(usable, view.half ?? absMax(usable), view.binWidth);
  const context = { ...view, half: binning.half, selectedHash: state.selectedHash };

  applyOutliersOpen(view.outliersOpen !== false);
  syncPresets();
  renderStats(usable, unavailable, binning);
  renderMeta(binning);
  renderStatus(usable, unavailable, binning);
  renderOutliers(binning, { all: rows(), visible: filtered() });

  const showTable = view.tab === "table";
  $("#delta-canvas").hidden = showTable;
  $("#delta-table-panel").hidden = !showTable;
  $("#delta-legend").hidden = showTable || view.tab === "coverage";
  if (showTable) renderTable(usable, unavailable, binning);
  renderCharts(context, usable, binning);
}

function renderCharts(context, usable, binning) {
  if (!mounted) return;
  if (!context) {
    const partition = partitionByDelta(filtered());
    usable = partition.usable;
    binning = computeBins(usable, view.half ?? absMax(usable), view.binWidth);
    context = { ...view, half: binning.half, selectedHash: state.selectedHash };
  }
  const selected = usable.find((row) => row.stale_hash === state.selectedHash);
  context.selectedDelta = selected
    ? (Number.isFinite(selected.header_time_delta_s) ? selected.header_time_delta_s : null)
    : selectedDeltaOutsideFilter();
  context.selectedBinK = selected ? binKeyFor(binning, selected.header_time_delta_s) : null;

  const canvas = $("#delta-canvas");
  const svg = $("#delta-chart");
  if (view.tab === "coverage") renderCoverage(svg, canvas, context, usable);
  else if (view.tab === "histogram") renderHistogram(svg, canvas, context, binning);

  renderContext($("#delta-context"), $("#delta-context-canvas"), context, usable, binning, {
    onSelect: (row) => selectRow(row.stale_hash),
    onWindow: (half) => {
      const ceiling = Math.max(absMax(usable), context.half);
      view.half = clamp(Math.round(half), 1, ceiling);
      render();
    },
    onWindowScale: (factor) => {
      const current = view.half ?? absMax(usable);
      view.half = clamp(Math.round(current * factor), 1, Math.max(absMax(usable), current));
      render();
    },
  });
  renderLegend(binning);
}

function selectedDeltaOutsideFilter() {
  if (!state.selectedHash) return null;
  const row = rows().find((candidate) => candidate.stale_hash === state.selectedHash);
  return row && Number.isFinite(row.header_time_delta_s) ? row.header_time_delta_s : null;
}

function renderLegend(binning) {
  const w = binning.w;
  const zero = binning.bins.find((bin) => bin.lo <= 0 && bin.hi >= 0);
  const span = !zero
    ? `±${fmtSpan(w / 2)}`
    : -zero.lo === zero.hi
      ? `±${fmtSpan(zero.hi)}`
      : `${fmtDelta(zero.lo)} … ${fmtDelta(zero.hi)}`;
  $("#delta-legend").innerHTML = [
    ["var(--delta-canonical-earlier)", "Canonical header earlier (Δ &lt; 0)"],
    ["var(--delta-stale-earlier)", "Stale header earlier (Δ &gt; 0)"],
    ["var(--delta-tied)", w === 1 ? "Tied to the second (Δ = 0)" : `Bin spans zero (${span})`],
  ]
    .map(([colour, label]) => `<span class="legend-item"><span class="legend-swatch" style="background:${colour}"></span>${label}</span>`)
    .join("")
    + (binning.below || binning.above
      ? `<span class="legend-item"><span class="legend-swatch legend-swatch-hatch"></span>Off-scale, clipped to the axis</span>`
      : "");
}

function renderStats(usable, unavailable, binning) {
  const sorted = usable.map((row) => row.header_time_delta_s).sort((a, b) => a - b);
  const n = sorted.length;
  const inside = n - binning.below - binning.above;
  const mean = n ? Math.round(sorted.reduce((a, b) => a + b, 0) / n) : 0;
  const tiles = [
    { label: "Competitions", value: fmtInt(n), sub: unavailable.length ? `${fmtInt(unavailable.length)} delta unavailable` : `${fmtInt(rows().length)} unfiltered` },
    { label: "Median", value: n ? fmtDelta(quantile(sorted, 0.5)) : "—", sub: n ? `mean ${fmtDelta(mean)}` : "" },
    { label: "Middle half", value: n ? `${fmtDelta(quantile(sorted, 0.25))} … ${fmtDelta(quantile(sorted, 0.75))}` : "—", sub: "25th–75th percentile", wide: true },
    { label: "In window", value: n ? fmtPct(inside / n) : "—", sub: `${fmtInt(inside)} of ${fmtInt(n)}` },
    { label: "Outside", value: fmtInt(binning.below + binning.above), sub: `${fmtInt(binning.below)} below · ${fmtInt(binning.above)} above` },
    { label: "Extremes", value: n ? `${fmtDelta(sorted[0])} / ${fmtDelta(sorted[n - 1])}` : "—", sub: "min / max", wide: true },
  ];
  $("#delta-stats").innerHTML = tiles
    .map((tile) => `<div class="stat-tile"${tile.wide ? ' data-wide="true"' : ""}>`
      + `<span class="stat-label">${esc(tile.label)}</span>`
      + `<span class="stat-value">${esc(tile.value)}</span>`
      + `<span class="stat-sub">${esc(tile.sub)}</span></div>`)
    .join("");
}

function renderMeta(binning) {
  const era = view.yearFrom === view.yearMin && view.yearTo === view.yearMax
    ? "all eras"
    : `${view.yearFrom}–${view.yearTo}`;
  const sources = state.query.sources?.length
    ? `${state.query.sources.length} source${state.query.sources.length === 1 ? "" : "s"}`
    : "all sources";
  $("#delta-meta").textContent =
    `canonical header time − stale header time · window ±${fmtTick(binning.half)} · ${fmtSpan(binning.w)} bins · ${era} · ${sources}`;
}

function renderStatus(usable, unavailable, binning) {
  const status = $("#delta-status");
  const outside = binning.below + binning.above;
  if (state.errors.competitions) {
    status.dataset.state = "error";
    const error = state.errors.competitions;
    status.textContent = `Competitions could not be loaded (${error.code || "error"}). `
      + `${error.message || "Try Refresh."}`;
    return;
  }
  if (!usable.length) {
    status.dataset.state = "error";
    status.textContent = unavailable.length
      ? `${fmtInt(unavailable.length)} competition${unavailable.length === 1 ? "" : "s"} match, `
        + "but none has a derivable header time delta, so there is nothing to plot."
      : "No competitions match the current Source and Era filters.";
    return;
  }
  delete status.dataset.state;
  status.textContent = outside
    ? `${fmtInt(outside)} competition${outside === 1 ? "" : "s"} fall outside the window (${fmtPct(outside / usable.length)}); `
      + `they are stacked in the off-scale gutters, ticked on the full-range strip, and listed at right.`
    : "Every competition in the current filter fits inside the window.";
}

function renderTable(usable, unavailable, { bins, below, above, edgeLo, edgeHi }) {
  const total = usable.length || 1;
  const parts = [];
  if (below) {
    parts.push(`<tr data-outside="true"><td>Off-scale</td><td>&lt; ${fmtInt(edgeLo)}</td>`
      + `<td>${fmtInt(below)}</td><td>${fmtPct(below / total)}</td><td>${fmtPct(below / total)}</td></tr>`);
  }
  let cumulative = below;
  for (const bin of bins) {
    cumulative += bin.count;
    if (!bin.count) continue;
    parts.push(`<tr><td>${esc(fmtDelta(bin.centre))}</td><td>${fmtInt(bin.lo)} … ${fmtInt(bin.hi)}</td>`
      + `<td>${fmtInt(bin.count)}</td><td>${fmtPct(bin.count / total)}</td><td>${fmtPct(cumulative / total)}</td></tr>`);
  }
  if (above) {
    parts.push(`<tr data-outside="true"><td>Off-scale</td><td>&gt; ${fmtInt(edgeHi)}</td>`
      + `<td>${fmtInt(above)}</td><td>${fmtPct(above / total)}</td><td>100%</td></tr>`);
  }
  const empty = state.errors.competitions
    ? "Competitions could not be loaded."
    : unavailable.length
      ? `${fmtInt(unavailable.length)} matching competition${unavailable.length === 1 ? "" : "s"}, none with a derivable delta.`
      : "No competitions match the current filters.";
  $("#delta-table-body").innerHTML = parts.join("")
    || `<tr><td colspan="5">${esc(empty)}</td></tr>`;
}

function revealSelection() {
  const row = rows().find((candidate) => candidate.stale_hash === state.selectedHash);
  if (!row) return;

  const year = yearOf(row);
  let eraWidened = false;
  if (year !== null && (year < view.yearFrom || year > view.yearTo)) {
    view.yearFrom = Math.min(view.yearFrom, year);
    view.yearTo = Math.max(view.yearTo, year);
    rebuildYearOptions();
    syncEraParam();
    eraWidened = true;
  }

  const sources = state.query.sources ?? [];
  const hiddenBySource = !matchesSourceFilter(row.sources, sources);
  if (!hiddenBySource) {
    if (eraWidened) render();
    return;
  }
  const boxes = [...document.querySelectorAll('#source-controls input[name="source"]:checked')];
  if (boxes.length) {
    for (const box of boxes) box.checked = false;
    updateSourceGroupSelectedMarkers();
    boxes[0].dispatchEvent(new Event("change", { bubbles: true }));
    return;
  }
  state.query.sources = [];
  syncUrl();
  render();
}

function syncPresets() {
  for (const button of document.querySelectorAll("#delta-presets [data-half]")) {
    const value = button.dataset.half === "" ? null : Number(button.dataset.half);
    button.setAttribute("aria-pressed", String(value === view.half));
  }
}

function syncEraParam() {
  state.query.era = view.yearFrom === view.yearMin && view.yearTo === view.yearMax
    ? ""
    : `${view.yearFrom}-${view.yearTo}`;
  syncUrl();
}

function prepareFocus() {
  view.outliersOpen = true;
  forgetScroll();
}

export { prepareFocus, registerDeltaView };
