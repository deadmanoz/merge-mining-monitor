// The Header Time Delta view's outlier panel: the list of competitions the
// focus window excludes, what it says about the current selection, and the
// disclosure that opens it.
//
// Split from delta-view.js because that file had grown to mix orchestration
// with this panel's markup. The panel reads no module state of its own beyond
// the scroll cache: the row sets it needs are passed in, so the dependency runs
// one way and this module stays independently exercisable.

import { fmtDelta, fmtInt, fmtUtcDate } from "./delta-scales.js?v=0.7.2";
import { $, esc, state } from "./frontend-state.js?v=0.7.2";

const OUTLIER_LIMIT = 250;

function renderOutliers(binning, { all, visible }) {
  const outside = [...binning.outside]
    .sort((a, b) => Math.abs(b.header_time_delta_s) - Math.abs(a.header_time_delta_s));
  const shown = outside.slice(0, OUTLIER_LIMIT);
  // The cap is by magnitude, and a cross-linked selection need not be among the
  // largest. Dropping it would leave the focused state with no row to flag or
  // scroll to, so it is appended rather than displacing one of the top rows.
  const capped = state.selectedHash
    && !shown.some((row) => row.stale_hash === state.selectedHash)
    && outside.find((row) => row.stale_hash === state.selectedHash);
  if (capped) shown.push(capped);
  $("#delta-outlier-count").textContent = outside.length ? fmtInt(outside.length) : "";

  const hidden = selectionNotice(all, visible);
  $("#delta-outlier-body").innerHTML = outside.length
    ? hidden
      + `<p class="drawer-note sort-note">Sorted by magnitude${outside.length > shown.length
        ? `; showing the ${fmtInt(shown.length - (capped ? 1 : 0))} largest of ${fmtInt(outside.length)}`
          + `${capped ? ", plus the selected competition" : ""}`
        : ""}.</p>`
      + `<div class="outlier-list">${shown.map(outlierRow).join("")}</div>`
    : hidden + `<p class="empty">Nothing outside the window.</p>`;
  // Unconditionally, including the empty list: this is where the scroll cache is
  // cleared, so an early return here would remember a scroll for a selection
  // that has since left the list and suppress the one owed when it returns.
  revealFocusedRow();
}

// The last selection scrolled to. Scrolling on every render would fight a user
// reading down a 250-row panel, so only a CHANGE of selection scrolls.
let scrolledFor = null;

/// Bring the focused outlier row into view. aria-current alone is invisible to a
/// sighted user when the row sits below the fold, which is the common case: a
/// cross-link arrives at a selection that is an outlier precisely because it is
/// extreme, and the list is sorted by magnitude but capped at 250.
function revealFocusedRow() {
  if (!state.selectedHash) {
    scrolledFor = null;
    return;
  }
  const body = $("#delta-outlier-body");
  const row = body?.querySelector('.outlier-row[aria-current="true"]');
  // Nothing to scroll to: the selection is in-window, the filters hide it, or
  // the list is collapsed. CLEAR the cache rather than leaving it, so the same
  // selection reappearing in the list scrolls again; remembering a scroll that
  // never happened would suppress it for as long as the selection holds.
  // getClientRects is empty when the row has no layout box at all: the list is
  // collapsed, or an ancestor is display:none because another view is active
  // and this render is painting off-screen. scrollIntoView is a no-op there, so
  // caching it would leave the row unscrolled when the view comes back.
  if (!row || body.hidden || !row.getClientRects().length) {
    scrolledFor = null;
    return;
  }
  if (scrolledFor === state.selectedHash) return;
  scrolledFor = state.selectedHash;
  row.scrollIntoView({ block: "nearest" });
}

/// What the panel says about the current selection. Every selection gets the
/// route back to the tree, not only one the filters hide: that link is the
/// documented counterpart of the drawer's link into this view, and an ordinary
/// visible outlier needs it just as much. A selection the Source or Era filter
/// excludes says so as well, and offers the one-click escape, rather than
/// silently resetting the user's filters on navigation.
function selectionNotice(all, visible) {
  if (!state.selectedHash) return "";
  const row = all.find((candidate) => candidate.stale_hash === state.selectedHash);
  if (!row) return "";
  // Not `?? 0`: an unavailable delta is not a tie, and saying "0s" here would
  // contradict every other unavailable-delta path in this view.
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

/// Reflect the disclosure state on the panel, the button and the list. The
/// panel's row template changes too: a `1fr` track keeps its share of the height
/// even with a hidden item, so a collapsed list would leave a gap.
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

/// Forget the last scroll, so an explicit navigation gesture scrolls again even
/// when it names the selection already held. Ordinary re-renders keep the cache,
/// which is what stops the panel fighting a user reading down the list.
function forgetScroll() {
  scrolledFor = null;
}

export { applyOutliersOpen, forgetScroll, renderOutliers, revealFocusedRow };
