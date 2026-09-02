// SVG rendering for the header-time-delta view: the linear core histogram with
// its off-scale gutters, the coverage curve, and the full-range symmetric-log
// strip with its brush and per-record rug.
//
// Every entry point takes an explicit `view` object (window, bin width, count
// scale, selection) and returns nothing; state lives in delta-view.js.

import {
  SYM_TICKS,
  clamp,
  countAxis,
  countScale,
  fmtAxis,
  fmtDelta,
  fmtInt,
  fmtPct,
  fmtPctRound,
  fmtSpan,
  fmtTick,
  fmtUtc,
  linearTicks,
  symexp,
  symlog,
} from "./delta-scales.js?v=0.7.8";
import { esc } from "./frontend-state.js?v=0.7.8";

const SVG_NS = "http://www.w3.org/2000/svg";
const GUTTER_W = 42;
const GUTTER_GAP = 9;

const el = (name, attrs = {}, text) => {
  const node = document.createElementNS(SVG_NS, name);
  for (const [key, value] of Object.entries(attrs)) node.setAttribute(key, value);
  if (text !== undefined) node.textContent = text;
  return node;
};

function sizeOf(svg) {
  const rect = svg.getBoundingClientRect();
  return { width: Math.round(rect.width), height: Math.round(rect.height) };
}

function clear(svg) {
  svg.replaceChildren(svg.querySelector("title"));
}

// ── Tooltip ─────────────────────────────────────────────────────────────────

/// `point` is anything carrying viewport clientX/clientY: a pointer event, or
/// the anchor a focused element stands in for.
function showTip(point, container, html) {
  const tooltip = document.querySelector("#delta-tooltip");
  if (!tooltip || !container) return;
  if (tooltip.parentElement !== container) container.appendChild(tooltip);
  const rect = container.getBoundingClientRect();
  tooltip.innerHTML = html;
  tooltip.hidden = false;
  tooltip.style.left = `${clamp(point.clientX - rect.left + 14, 6, Math.max(6, rect.width - tooltip.offsetWidth - 6))}px`;
  tooltip.style.top = `${clamp(point.clientY - rect.top + 14, 6, Math.max(6, rect.height - tooltip.offsetHeight - 6))}px`;
}

/// Where a keyboard focus "points": the middle of the focused element.
function anchorOf(node) {
  const box = node.getBoundingClientRect();
  return { clientX: box.left + box.width / 2, clientY: box.top + box.height / 2 };
}

function hideTip() {
  const tooltip = document.querySelector("#delta-tooltip");
  if (tooltip) tooltip.hidden = true;
}

/// Attach the same hover/focus tooltip to a hit rect, so keyboard users get
/// what pointer users get.
function wireTip(hit, container, html) {
  hit.addEventListener("pointerenter", (event) => showTip(event, container, html));
  // A FocusEvent carries no clientX/clientY, so forwarding it produced `NaNpx`
  // offsets and left the tooltip wherever the pointer had last put it.
  hit.addEventListener("focus", () => showTip(anchorOf(hit), container, html));
  hit.addEventListener("pointerleave", hideTip);
  hit.addEventListener("blur", hideTip);
}

// ── Histogram ───────────────────────────────────────────────────────────────

function renderHistogram(svg, container, view, binning) {
  const { bins, below, above, w, edgeLo, edgeHi } = binning;
  const { width, height } = sizeOf(svg);
  const m = { top: 26, right: 16, bottom: 38, left: 52 };
  const plotW = width - m.left - m.right;
  const plotH = height - m.top - m.bottom;
  clear(svg);
  svg.querySelector("title").textContent =
    `Histogram of header time delta between ${fmtTick(edgeLo)} and ${fmtTick(edgeHi)}, ${w} second bins.`;
  if (plotW <= 0 || plotH <= 0) return;

  const leftG = below > 0 ? GUTTER_W + GUTTER_GAP : 0;
  const rightG = above > 0 ? GUTTER_W + GUTTER_GAP : 0;
  const coreX = m.left + leftG;
  const coreW = plotW - leftG - rightG;
  if (coreW <= 0) return;

  const maxCount = Math.max(1, ...bins.map((bin) => bin.count));
  const { ticks, domainMax } = countAxis(maxCount, view.yscale);
  const x = (d) => coreX + ((d - edgeLo) / (edgeHi - edgeLo)) * coreW;
  const y = (count) => m.top + plotH * (1 - countScale(count, domainMax, view.yscale));

  const g = el("g");
  for (const tick of ticks) {
    const ty = y(tick);
    g.appendChild(el("line", { class: "grid-line", x1: m.left, x2: width - m.right, y1: ty, y2: ty }));
    g.appendChild(el("text", { class: "axis-text", x: m.left - 7, y: ty + 3.5, "text-anchor": "end" }, fmtInt(tick)));
  }

  const zeroX = x(0);
  g.appendChild(el("line", { class: "zero-line", x1: zeroX, x2: zeroX, y1: m.top, y2: m.top + plotH }));

  const inWindow = bins.reduce((sum, bin) => sum + bin.count, 0);
  const GAP = 2;
  for (const bin of bins) {
    if (!bin.count) continue;
    // Geometry from the bin's real bounds, not a uniform width around its
    // centre: the outermost bins are clipped to the window (commonly to half
    // width), so a uniform rectangle would overhang the declared window and
    // overlap its neighbour while the table reported the clipped range.
    const x0 = x(bin.lo);
    const x1 = x(bin.hi);
    const barW = Math.max(1, x1 - x0 - GAP);
    const bx = x0 + GAP / 2;
    const by = y(bin.count);
    const cls = bin.centre === 0 ? "bar-zero" : bin.centre < 0 ? "bar-neg" : "bar-pos";
    const isSelected = view.selectedBinK != null && bin.k === view.selectedBinK;
    g.appendChild(el("rect", {
      class: `bar-mark ${cls}${isSelected ? " is-selected" : ""}`,
      x: bx,
      y: by,
      width: barW,
      height: Math.max(m.top + plotH - by, 1),
      rx: Math.min(4, barW / 2),
    }));
    const hit = el("rect", { class: "bar-hit", x: x0, y: m.top, width: Math.max(1, x1 - x0), height: plotH, tabindex: "0" });
    const range = w === 1 ? esc(fmtDelta(bin.centre)) : `${esc(fmtDelta(bin.lo))} … ${esc(fmtDelta(bin.hi))}`;
    const sense = bin.centre < 0
      ? "canonical header earlier"
      : bin.centre > 0
        ? "stale header earlier"
        : w === 1 ? "timestamps tied to the second" : "bin spans zero";
    wireTip(hit, container,
      `<strong>${range}</strong>${fmtInt(bin.count)} competition${bin.count === 1 ? "" : "s"} `
      + `(${fmtPct(bin.count / Math.max(1, inWindow))} of the window)<br /><span>${sense}</span>`
      + (isSelected ? `<br /><span>holds the selected competition</span>` : ""));
    g.appendChild(hit);
  }

  if (below > 0) drawGutter(svg, g, container, m.left, m.top, plotH, below, domainMax, view, "below", edgeLo);
  if (above > 0) {
    drawGutter(svg, g, container, width - m.right - GUTTER_W, m.top, plotH, above, domainMax, view, "above", edgeHi);
  }

  const axisY = m.top + plotH;
  g.appendChild(el("line", { class: "axis-line", x1: m.left, x2: width - m.right, y1: axisY, y2: axisY }));
  for (const tick of linearTicks(view.half)) {
    const tx = x(tick);
    if (tx < coreX - 1 || tx > coreX + coreW + 1) continue;
    g.appendChild(el("line", { class: "axis-line", x1: tx, x2: tx, y1: axisY, y2: axisY + 4 }));
    g.appendChild(el("text", { class: "axis-text", x: tx, y: axisY + 15, "text-anchor": "middle" }, fmtAxis(tick)));
  }
  g.appendChild(el("text", {
    class: "axis-title",
    x: coreX + coreW / 2,
    y: axisY + 30,
    "text-anchor": "middle",
  }, "canonical header time − stale header time"));
  g.appendChild(el("text", { class: "axis-title", x: 8, y: 12 }, "count"));
  svg.appendChild(g);
}

/**
 * Off-scale gutter: everything past a window edge, on the same count axis so it
 * stays comparable, hatched so it never reads as a real bin, and flat-topped
 * with an always-visible count when it overflows the axis.
 */
function drawGutter(svg, g, container, gx, top, plotH, count, domainMax, view, side, edge) {
  ensureHatch(svg);
  const frac = countScale(count, domainMax, view.yscale);
  const h = Math.max(2, Math.min(1, frac) * plotH);
  const gy = top + plotH - h;
  g.appendChild(el("rect", { class: "gutter-panel", x: gx, y: top, width: GUTTER_W, height: plotH, rx: 3 }));
  g.appendChild(el("rect", {
    class: "gutter-mark",
    x: gx,
    y: gy,
    width: GUTTER_W,
    height: h,
    fill: "url(#delta-off-scale-hatch)",
    rx: 3,
  }));
  if (frac > 1) {
    // A stepped cap reads as "this bar is truncated" without inventing a scale.
    for (let i = 0; i < 3; i += 1) {
      g.appendChild(el("line", {
        class: "axis-line",
        x1: gx + 3,
        x2: gx + GUTTER_W - 3,
        y1: top + 3 + i * 3,
        y2: top + 3 + i * 3,
      }));
    }
  }
  g.appendChild(el("text", {
    class: "annot-text",
    x: gx + GUTTER_W / 2,
    y: Math.max(top + 11, gy - 5),
    "text-anchor": "middle",
  }, fmtInt(count)));
  g.appendChild(el("text", {
    class: "axis-text",
    x: gx + GUTTER_W / 2,
    y: top + plotH + 15,
    "text-anchor": "middle",
  }, side === "below" ? `< ${fmtAxis(edge)}` : `> ${fmtAxis(edge)}`));
  const hit = el("rect", { class: "bar-hit", x: gx, y: top, width: GUTTER_W, height: plotH, tabindex: "0" });
  wireTip(hit, container,
    `<strong>${side === "below" ? `Δ &lt; ${esc(fmtDelta(edge))}` : `Δ &gt; ${esc(fmtDelta(edge))}`}</strong>`
    + `${fmtInt(count)} competition${count === 1 ? "" : "s"} off scale<br />`
    + `<span>Listed in the panel at right; ticked on the strip below.</span>`);
  g.appendChild(hit);
}

function ensureHatch(svg) {
  if (svg.querySelector("#delta-off-scale-hatch")) return;
  const defs = el("defs");
  const pattern = el("pattern", {
    id: "delta-off-scale-hatch",
    width: 6,
    height: 6,
    patternUnits: "userSpaceOnUse",
    patternTransform: "rotate(45)",
  });
  pattern.appendChild(el("rect", { width: 6, height: 6, fill: "var(--surface-alt)" }));
  pattern.appendChild(el("line", { x1: 0, y1: 0, x2: 0, y2: 6, stroke: "var(--line-strong)", "stroke-width": 2.5 }));
  defs.appendChild(pattern);
  svg.appendChild(defs);
}

// ── Coverage curve ──────────────────────────────────────────────────────────

/**
 * Share of competitions whose |delta| is within T, for T from 1s to the widest
 * extreme on a log axis. This is the honest tail view: the curve reaches 100%
 * by construction, so how late it gets there IS the outlier story.
 */
function renderCoverage(svg, container, view, rows) {
  const { width, height } = sizeOf(svg);
  const m = { top: 18, right: 20, bottom: 38, left: 52 };
  const plotW = width - m.left - m.right;
  const plotH = height - m.top - m.bottom;
  clear(svg);
  svg.querySelector("title").textContent =
    "Share of competitions within a given absolute header time delta.";
  if (plotW <= 0 || plotH <= 0 || !rows.length) return;

  const abs = rows.map((row) => Math.abs(row.header_time_delta_s)).sort((a, b) => a - b);
  const n = abs.length;
  const logHi = Math.log10(Math.max(abs.at(-1), 10));
  const x = (t) => m.left + (Math.log10(Math.max(t, 1)) / logHi) * plotW;
  const y = (f) => m.top + plotH * (1 - f);
  const coverage = (t) => {
    let lo = 0;
    let hi = n;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if (abs[mid] <= t) lo = mid + 1;
      else hi = mid;
    }
    return lo / n;
  };

  const g = el("g");
  for (let f = 0; f <= 1.0001; f += 0.25) {
    const gy = y(f);
    g.appendChild(el("line", { class: "grid-line", x1: m.left, x2: width - m.right, y1: gy, y2: gy }));
    g.appendChild(el("text", { class: "axis-text", x: m.left - 7, y: gy + 3.5, "text-anchor": "end" }, fmtPctRound(f)));
  }

  const points = [];
  const STEPS = 260;
  for (let s = 0; s <= STEPS; s += 1) {
    const t = Math.pow(10, (s / STEPS) * logHi);
    points.push([x(t), y(coverage(t))]);
  }
  const path = points.map(([px, py], i) => `${i ? "L" : "M"}${px.toFixed(2)},${py.toFixed(2)}`).join("");
  g.appendChild(el("path", { class: "coverage-area", d: `${path}L${x(Math.pow(10, logHi))},${y(0)}L${m.left},${y(0)}Z` }));
  g.appendChild(el("path", { class: "coverage-line", d: path }));

  // Labels flip to the left of their dot near the right edge so they never clip.
  const place = (px, text) => (px + 7 + text.length * 5.6 > width - m.right
    ? { x: px - 7, anchor: "end" }
    : { x: px + 7, anchor: "start" });
  for (const target of [0.5, 0.9, 0.99]) {
    // A log axis cannot separate 0 from 1: x() folds them onto one coordinate,
    // and the curve is sampled from 1s up. Take the threshold, its coverage and
    // its label all from the plotted value, or a 0s percentile among 1s records
    // would sit below the very curve drawn at its own x.
    const t = Math.max(abs[clamp(Math.ceil(target * n) - 1, 0, n - 1)], 1);
    const px = x(t);
    // Plot the dot at the coverage the threshold ACTUALLY reaches, not at the
    // nominal percentile. Duplicate absolute deltas make the two differ: if
    // every delta is equal, the curve jumps straight to 100% at that x, and
    // dots pinned to 50/90/99% would float below their own curve.
    const reached = coverage(t);
    const py = y(reached);
    g.appendChild(el("line", { class: "annot-line", x1: m.left, x2: px, y1: py, y2: py }));
    g.appendChild(el("line", { class: "annot-line", x1: px, x2: px, y1: py, y2: m.top + plotH }));
    g.appendChild(el("circle", { class: "coverage-dot", cx: px, cy: py, r: 4 }));
    const text = `${fmtPct(reached)} within ±${fmtTick(t)}`;
    const pos = place(px, text);
    g.appendChild(el("text", { class: "annot-text", x: pos.x, y: py - 6, "text-anchor": pos.anchor }, text));
  }

  if (view.half < Math.pow(10, logHi)) {
    const px = x(view.half);
    g.appendChild(el("line", { class: "zero-line", x1: px, x2: px, y1: m.top, y2: m.top + plotH }));
    const text = `window ±${fmtTick(view.half)} → ${fmtPct(coverage(view.half))}`;
    const pos = place(px, text);
    g.appendChild(el("text", { class: "annot-text", x: pos.x - 1, y: m.top + 11, "text-anchor": pos.anchor }, text));
  }

  const axisY = m.top + plotH;
  g.appendChild(el("line", { class: "axis-line", x1: m.left, x2: width - m.right, y1: axisY, y2: axisY }));
  for (const t of [1, 10, 60, 600, 3600, 86400, 604800, 2592000]) {
    if (t > Math.pow(10, logHi)) continue;
    const tx = x(t);
    g.appendChild(el("line", { class: "axis-line", x1: tx, x2: tx, y1: axisY, y2: axisY + 4 }));
    g.appendChild(el("text", { class: "axis-text", x: tx, y: axisY + 15, "text-anchor": "middle" }, fmtTick(t)));
  }
  g.appendChild(el("text", {
    class: "axis-title",
    x: m.left + plotW / 2,
    y: axisY + 30,
    "text-anchor": "middle",
  }, "absolute header time delta (log)"));
  g.appendChild(el("text", { class: "axis-title", x: 8, y: 12 }, "share"));

  const hit = el("rect", { class: "bar-hit", x: m.left, y: m.top, width: plotW, height: plotH });
  hit.addEventListener("pointermove", (event) => {
    const rect = svg.getBoundingClientRect();
    const px = clamp(event.clientX - rect.left, m.left, m.left + plotW);
    const t = Math.pow(10, ((px - m.left) / plotW) * logHi);
    showTip(event, container,
      `<strong>±${esc(fmtTick(Math.round(t)))}</strong>${fmtPct(coverage(t))} of competitions<br />`
      + `<span>${fmtInt(Math.round(coverage(t) * n))} of ${fmtInt(n)}</span>`);
  });
  hit.addEventListener("pointerleave", hideTip);
  g.appendChild(hit);
  svg.appendChild(g);
}

// ── Full-range symlog strip ─────────────────────────────────────────────────

function renderContext(svg, container, view, rows, binning, handlers) {
  const { width, height } = sizeOf(svg);
  const m = { top: 10, right: 16, bottom: 30, left: 52 };
  const barsH = height - m.top - m.bottom - 14;
  const rugY = m.top + barsH + 4;
  const plotW = width - m.left - m.right;
  clear(svg);
  svg.querySelector("title").textContent =
    "Full range of header time deltas on a symmetric log axis, with the focus window highlighted.";
  // No `!rows.length` guard: the domain is anchored on zero and both focus
  // edges regardless of data, so an empty filter still draws a usable axis and
  // brush, and a selection the filters exclude still gets its marker.
  if (plotW <= 0 || barsH <= 0) return;

  // The domain must cover more than the filtered data. Zero is the axis the
  // whole view is about; both focus edges have to be drawable or the brush
  // handles land off-canvas; and a selection the filters exclude still gets a
  // marker. A single positive row would otherwise give symLo === symHi, padded
  // the same direction, putting zero and both handles outside the SVG.
  const anchors = rows.map((row) => row.header_time_delta_s);
  anchors.push(0, -view.half, view.half);
  if (view.selectedDelta != null) anchors.push(view.selectedDelta);
  const lo = symlog(Math.min(...anchors));
  const hi = symlog(Math.max(...anchors));
  // Pad outward from each end rather than scaling, so a domain that is already
  // symmetric about zero does not collapse.
  const pad = Math.max((hi - lo) * 0.03, 0.15);
  const symLo = lo - pad;
  const symHi = hi + pad;
  const span = symHi - symLo;
  const x = (d) => m.left + ((symlog(d) - symLo) / span) * plotW;
  const xInv = (px) => symexp(symLo + ((px - m.left) / plotW) * span);

  const NBINS = 170;
  const counts = new Array(NBINS).fill(0);
  for (const row of rows) {
    const b = clamp(Math.floor(((symlog(row.header_time_delta_s) - symLo) / span) * NBINS), 0, NBINS - 1);
    counts[b] += 1;
  }
  const maxC = Math.max(1, ...counts);
  const g = el("g");

  // Log counts here: the strip's job is presence-of-mass across seven decades,
  // not magnitude, and a linear count would erase every single-record bin.
  const bw = plotW / NBINS;
  for (let b = 0; b < NBINS; b += 1) {
    if (!counts[b]) continue;
    const h = Math.max(2, (Math.log10(1 + counts[b]) / Math.log10(1 + maxC)) * barsH);
    const bx = m.left + b * bw;
    const centre = xInv(bx + bw / 2);
    g.appendChild(el("rect", {
      class: `bar-mark ${centre < -0.5 ? "bar-neg" : centre > 0.5 ? "bar-pos" : "bar-zero"}`,
      x: bx,
      y: m.top + barsH - h,
      width: Math.max(1, bw - 1),
      height: h,
    }));
  }

  // Per-record rug for everything the window excludes: a lone 78-day outlier is
  // one pixel of histogram but an unmissable tick here. The selected tick is
  // appended last so it paints over its neighbours in a dense stretch.
  let selectedTick = null;
  for (const row of binning.outside) {
    const px = x(row.header_time_delta_s);
    const selected = row.stale_hash === view.selectedHash;
    const tick = el("line", {
      class: `rug-tick ${row.header_time_delta_s < 0 ? "rug-neg" : "rug-pos"}${selected ? " is-selected" : ""}`,
      x1: px,
      x2: px,
      y1: rugY,
      y2: rugY + 10,
    });
    if (selected) selectedTick = tick;
    else g.appendChild(tick);
  }

  const bLo = x(-view.half);
  const bHi = x(view.half);
  g.appendChild(el("rect", { class: "brush-veil", x: m.left, y: m.top, width: Math.max(0, bLo - m.left), height: barsH + 14 }));
  g.appendChild(el("rect", { class: "brush-veil", x: bHi, y: m.top, width: Math.max(0, m.left + plotW - bHi), height: barsH + 14 }));
  g.appendChild(el("rect", { class: "brush-window", x: bLo, y: m.top, width: Math.max(1, bHi - bLo), height: barsH + 14 }));
  g.appendChild(el("text", {
    class: "brush-label",
    x: (bLo + bHi) / 2,
    y: m.top - 1,
    "text-anchor": "middle",
  }, `±${fmtTick(view.half)}`));

  // A selection hidden by the active filters still gets its marker, mirroring
  // the tree, which exempts the selected node from source dimming.
  if (selectedTick) g.appendChild(selectedTick);
  else if (view.selectedDelta != null) {
    const px = x(view.selectedDelta);
    g.appendChild(el("line", { class: "rug-tick is-selected", x1: px, x2: px, y1: rugY, y2: rugY + 10 }));
  }

  const axisY = m.top + barsH + 18;
  g.appendChild(el("line", { class: "axis-line", x1: m.left, x2: width - m.right, y1: axisY, y2: axisY }));
  for (const t of SYM_TICKS) {
    for (const sign of t === 0 ? [1] : [-1, 1]) {
      const tx = x(t * sign);
      if (tx < m.left - 1 || tx > m.left + plotW + 1) continue;
      g.appendChild(el("line", { class: "axis-line", x1: tx, x2: tx, y1: axisY, y2: axisY + 4 }));
      g.appendChild(el("text", {
        class: "axis-text",
        x: tx,
        y: axisY + 14,
        "text-anchor": "middle",
      }, t === 0 ? "0" : `${sign < 0 ? "−" : "+"}${fmtTick(t)}`));
    }
  }
  g.appendChild(el("text", { class: "axis-title", x: 8, y: m.top + 8 }, "log n"));

  // One hover handler beats a listener per rug tick and keeps sparse ticks
  // reachable.
  const hover = el("rect", { class: "bar-hit", x: m.left, y: m.top, width: plotW, height: barsH + 14 });
  const nearest = (px) => {
    const target = symlog(xInv(px));
    let best = null;
    let bestD = Infinity;
    for (const row of binning.outside) {
      const d = Math.abs(symlog(row.header_time_delta_s) - target);
      if (d < bestD) { bestD = d; best = row; }
    }
    return bestD < 0.35 ? best : null;
  };
  hover.addEventListener("pointermove", (event) => {
    const px = event.clientX - svg.getBoundingClientRect().left;
    if (px > bLo && px < bHi) {
      showTip(event, container,
        `<strong>Focus window ±${esc(fmtTick(view.half))}</strong>`
        + `<span>Drag an edge, or drag the band left and right, to resize. The window stays centred on zero.</span>`);
      return;
    }
    const row = nearest(px);
    if (!row) { hideTip(); return; }
    showTip(event, container, outlierTip(row));
  });
  hover.addEventListener("pointerleave", hideTip);
  hover.addEventListener("click", (event) => {
    const px = event.clientX - svg.getBoundingClientRect().left;
    if (px > bLo && px < bHi) return;
    const row = nearest(px);
    if (row) handlers.onSelect(row);
  });
  g.appendChild(hover);

  addBrush(svg, g, { bLo, bHi, top: m.top, height: barsH + 14, left: m.left, plotW, xInv, half: view.half, handlers });
  svg.appendChild(g);
}

function outlierTip(row) {
  return `<strong>${esc(fmtDelta(row.header_time_delta_s))} · height ${fmtInt(row.btc_height)}</strong>`
    + `${esc(fmtUtc(row.stale_header_time))}<br />`
    + `<span>${esc(row.stale_bitcoin_miner_pool.name)} (stale) vs ${esc(row.canonical_bitcoin_miner_pool.name)} (canonical)</span>`;
}

/// Draggable focus window. The window is always symmetric about zero, so both
/// the edge handles and the band itself resize rather than pan.
function addBrush(svg, g, { bLo, bHi, top, height, left, plotW, xInv, half, handlers }) {
  const body = el("rect", { class: "brush-body", x: bLo, y: top, width: Math.max(1, bHi - bLo), height });
  g.appendChild(body);
  for (const side of ["lo", "hi"]) {
    const handle = el("rect", {
      class: "brush-handle",
      x: (side === "lo" ? bLo : bHi) - 3,
      y: top,
      width: 6,
      height,
      rx: 2,
      tabindex: "0",
      role: "slider",
      "aria-label": `${side === "lo" ? "Lower" : "Upper"} focus window edge`,
    });
    handle.addEventListener("pointerdown", (event) => startDrag(svg, event, (px) => {
      handlers.onWindow(Math.abs(xInv(clamp(px, left, left + plotW))));
    }));
    handle.addEventListener("keydown", (event) => {
      const factor = event.key === "ArrowRight" ? 1.35 : event.key === "ArrowLeft" ? 1 / 1.35 : null;
      if (!factor) return;
      event.preventDefault();
      handlers.onWindowScale(side === "lo" ? 1 / factor : factor);
    });
    g.appendChild(handle);
  }
  body.addEventListener("pointerdown", (event) => {
    // Scale from the half-width captured at pointerdown, not from the current
    // one. Applying a factor to the live value compounds it on every
    // pointermove, so a few pixels of drag run the window away by orders of
    // magnitude.
    const startX = event.clientX;
    const startHalf = half;
    startDrag(svg, event, (_px, clientX) => {
      handlers.onWindow(startHalf * Math.pow(10, ((clientX - startX) / plotW) * 6 * 0.6));
    });
  });
}

function startDrag(svg, event, onMove) {
  event.preventDefault();
  const rect = svg.getBoundingClientRect();
  const move = (moveEvent) => onMove(moveEvent.clientX - rect.left, moveEvent.clientX);
  const up = () => {
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
}

export { hideTip, renderContext, renderCoverage, renderHistogram };
