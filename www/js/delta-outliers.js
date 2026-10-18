import { fmtDelta, fmtInt, fmtUtcDate } from "./delta-scales.js";
import { $, esc, state } from "./frontend-state.js";

const OUTLIER_LIMIT = 250;

function renderOutliers(binning, { all, visible }) {
  const outside = [...binning.outside]
    .sort((a, b) => Math.abs(b.header_time_delta_s) - Math.abs(a.header_time_delta_s));
  const shown = outside.slice(0, OUTLIER_LIMIT);
  const capped = state.selectedHash
    && !shown.some((row) => row.stale_hash === state.selectedHash)
    && outside.find((row) => row.stale_hash === state.selectedHash);
  if (capped) shown.push(capped);
  $("#delta-outlier-count").textContent = outside.length ? fmtInt(outside.length) : "";

  const hidden = selectionNotice(all, visible);
  $("#delta-outlier-body").innerHTML = outside.length
    ? hidden
      + `<p class="drawer-note">Sorted by magnitude${outside.length > shown.length
        ? `; showing the ${fmtInt(shown.length - (capped ? 1 : 0))} largest of ${fmtInt(outside.length)}`
          + `${capped ? ", plus the selected competition" : ""}`
        : ""}.</p>`
      + `<div class="outlier-list">${shown.map(outlierRow).join("")}</div>`
    : hidden + `<p class="empty">Nothing outside the window.</p>`;
  revealFocusedRow();
}

let scrolledFor = null;

function revealFocusedRow() {
  if (!state.selectedHash) {
    scrolledFor = null;
    return;
  }
  const body = $("#delta-outlier-body");
  const row = body?.querySelector('.outlier-row[aria-current="true"]');
  if (!row || body.hidden || !row.getClientRects().length) {
    scrolledFor = null;
    return;
  }
  if (scrolledFor === state.selectedHash) return;
  scrolledFor = state.selectedHash;
  row.scrollIntoView({ block: "nearest" });
}

function selectionNotice(all, visible) {
  if (!state.selectedHash) return "";
  const row = all.find((candidate) => candidate.stale_hash === state.selectedHash);
  if (!row) return "";
  const delta = Number.isFinite(row.header_time_delta_s)
    ? esc(fmtDelta(row.header_time_delta_s))
    : "delta unavailable";
  const named = `Selected competition (${delta}, height ${fmtInt(row.btc_height)})`;
  const tree = `<button class="secondary-button" type="button" data-outlier-index="-1"`
    + ` data-action="tree" data-height="${row.btc_height}">Show in tree</button>`;
  if (visible.some((candidate) => candidate.stale_hash === state.selectedHash)) {
    return `<p class="drawer-note selection-note">${named}.${tree}</p>`;
  }
  return `<p class="drawer-note hidden-selection">`
    + `${named} is hidden by the active Source or Era filter, and is excluded from every count here. `
    + `<button class="secondary-button" type="button" data-outlier-index="-1" data-action="reveal">Clear the filters hiding it</button>`
    + tree + `</p>`;
}

function applyOutliersOpen(open) {
  $("#delta-outliers").dataset.open = String(open);
  $("#delta-outliers-toggle").setAttribute("aria-expanded", String(open));
  $("#delta-outlier-body").hidden = !open;
}

function outlierRow(row, index) {
  const delta = row.header_time_delta_s;
  const selected = row.stale_hash === state.selectedHash;
  return `<button class="outlier-row" type="button" data-outlier-index="${index}" data-hash="${esc(row.stale_hash)}"`
    + `${selected ? ' aria-current="true"' : ""}>`
    + `<span class="outlier-primary">`
    + `<span class="outlier-delta" data-sign="${delta < 0 ? "neg" : "pos"}">${esc(fmtDelta(delta))}</span>`
    + `<span class="outlier-meta">${esc(fmtUtcDate(row.stale_header_time))} · `
    + `${esc(row.stale_bitcoin_miner_pool.name)} vs ${esc(row.canonical_bitcoin_miner_pool.name)}</span>`
    + `</span><span class="outlier-height">${fmtInt(row.btc_height)}</span></button>`;
}

function forgetScroll() {
  scrolledFor = null;
}

export { applyOutliersOpen, forgetScroll, renderOutliers, revealFocusedRow };
