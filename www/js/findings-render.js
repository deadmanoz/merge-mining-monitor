// Pure renderers for the findings view: the month-grouped feed, the article,
// and the structured evidence figures. No state reads and no wiring here; the
// view module owns both. Citation markup reuses the source-dialog helpers so
// `[^N]` markers, `code` spans, and the Sources list render identically to the
// chain profiles (and the jscpd gate stays quiet).

import { esc } from "./frontend-state.js?v=0.3.0";
import {
  collectCitedReferenceIds,
  formatCitedText,
  renderSourcesSection,
} from "./source-dialog.js?v=0.3.0";

// Category/status presentation. Colors are existing semantic tokens, chosen
// once here so chips and rail dots stay in sync.
const CATEGORIES = {
  "hashrate-shift": { label: "Hashrate shift", color: "var(--near)" },
  "pool-exit": { label: "Pool exit", color: "var(--unknown)" },
  "chain-incident": { label: "Chain incident", color: "var(--stale)" },
  "dataset-note": { label: "Dataset note", color: "var(--context)" },
};

const STATUSES = {
  ongoing: { label: "Ongoing", tone: "stale" },
  monitoring: { label: "Monitoring", tone: "near" },
  concluded: { label: "Concluded", tone: "canonical" },
};

const MONTHS = [
  "January", "February", "March", "April", "May", "June",
  "July", "August", "September", "October", "November", "December",
];

/// Parse an ISO date or `YYYY-MM-DDTHH:MM` instant as a UTC epoch (ms). The
/// generator validated the format, so a NaN here means corpus drift, not user
/// input; render defensively anyway.
function instantMs(t) {
  return Date.parse(t.includes("T") ? `${t}:00Z` : `${t}T00:00:00Z`);
}

function monthLabel(isoDate) {
  const d = new Date(instantMs(isoDate));
  return `${MONTHS[d.getUTCMonth()]} ${d.getUTCFullYear()}`;
}

function chainOf(sourceCode) {
  const parts = sourceCode.split(":");
  return parts[1] || sourceCode;
}

function categoryChip(category) {
  const meta = CATEGORIES[category] || { label: category, color: "var(--context)" };
  return `<span class="finding-cat" style="--cat: ${meta.color}">${esc(meta.label)}</span>`;
}

function statusPill(status) {
  const meta = STATUSES[status] || { label: status, tone: "near" };
  return `<span class="finding-status finding-status-${esc(meta.tone)}">${esc(meta.label)}</span>`;
}

/// One feed card. NOT a button: summaries carry live citation links, and an
/// interactive element may not nest another. The card is a plain container
/// the delegated handler opens by `data-finding` (ignoring clicks that land
/// on a link), and the title is the dedicated keyboard-reachable opener.
function findingCard(finding) {
  const chains = finding.affected_sources
    .map((code) => `<span class="finding-chip">${esc(chainOf(code))}</span>`)
    .join("");
  const extras = [];
  if (finding.anchors?.length) {
    extras.push(`<span class="finding-chip finding-chip-count">${finding.anchors.length} anchors</span>`);
  }
  if (finding.figures?.length) {
    extras.push(`<span class="finding-chip finding-chip-count">${finding.figures.length} figure${finding.figures.length > 1 ? "s" : ""}</span>`);
  }
  return `
    <article class="finding-card" data-finding="${esc(finding.slug)}">
      <span class="finding-card-top">
        ${categoryChip(finding.category)}
        <span class="finding-card-date">${esc(finding.observed_at)}</span>
      </span>
      <button type="button" class="finding-card-open" data-finding="${esc(finding.slug)}">
        <span class="finding-card-title">${esc(finding.title)}</span>
      </button>
      <span class="finding-card-summary">${formatCitedText(finding.summary, finding.references, null)}</span>
      <span class="finding-card-meta">${statusPill(finding.status)}${chains}${extras.join("")}</span>
    </article>`;
}

/// The feed: cards in corpus order (newest-first), grouped under month labels
/// derived from `observed_at`. `findings` is already filtered by the view.
function renderFeed(findings, total) {
  if (!findings.length) {
    return `
      <div class="findings-head"><h2>Findings</h2>
        <span class="findings-head-sub">0 of ${total} shown</span></div>
      <p class="findings-empty">No findings match the current filters.</p>`;
  }
  let out = `
    <div class="findings-head"><h2>Findings</h2>
      <span class="findings-head-sub">${findings.length === total ? `${total} findings` : `${findings.length} of ${total} shown`} · evidence-anchored</span></div>`;
  let month = "";
  for (const finding of findings) {
    const label = monthLabel(finding.observed_at);
    if (label !== month) {
      month = label;
      out += `<div class="findings-month">${esc(label)}</div>`;
    }
    out += findingCard(finding);
  }
  return out;
}

/// One evidence anchor. Height anchors jump to the tree and source anchors
/// open the source dialog (both via the view's delegated handler); child
/// heights and pools are informational chips with nothing to navigate to yet.
function anchorControl(anchor) {
  const label = anchor.label ? `<span class="fa-label">${esc(anchor.label)}</span>` : "";
  const value = `<span class="fa-value">${esc(anchor.value)}</span>`;
  if (anchor.kind === "btc-height") {
    return `<button type="button" class="finding-anchor" data-anchor-kind="btc-height" data-anchor-value="${esc(anchor.value)}">
      <span class="fa-kind">BTC</span>${value}${label}<span class="fa-go">Tree</span></button>`;
  }
  if (anchor.kind === "source") {
    return `<button type="button" class="finding-anchor" data-anchor-kind="source" data-anchor-value="${esc(anchor.value)}">
      <span class="fa-kind">Source</span>${value}${label}<span class="fa-go">detail</span></button>`;
  }
  const kind = anchor.kind === "child-height" ? "Child" : "Pool";
  return `<span class="finding-anchor finding-anchor-static">
    <span class="fa-kind">${esc(kind)}</span>${value}${label}</span>`;
}

/// Format an axis value: integers with grouping, fractional values to one
/// decimal (the corpus carries block weights and weekly counts).
function figValue(v) {
  return Number.isInteger(v) ? v.toLocaleString("en-US") : v.toFixed(1);
}

/// A `line-series` figure as an inline SVG on theme tokens. Time maps linearly
/// to x; the y range pads 8% around the data so a flat tail stays visible.
/// Markers draw as dashed verticals with a label at the top.
function renderFigure(figure) {
  const W = 560;
  const H = 150;
  const PAD = { left: 46, right: 14, top: 18, bottom: 20 };
  const xs = figure.points.map((p) => instantMs(p.t));
  const ys = figure.points.map((p) => p.v);
  const x0 = xs[0];
  const x1 = xs[xs.length - 1];
  const yMin = Math.min(...ys);
  const yMax = Math.max(...ys);
  const yPad = (yMax - yMin || Math.abs(yMax) || 1) * 0.08;
  const lo = yMin - yPad;
  const hi = yMax + yPad;
  const px = (t) => PAD.left + ((t - x0) / (x1 - x0 || 1)) * (W - PAD.left - PAD.right);
  const py = (v) => PAD.top + ((hi - v) / (hi - lo)) * (H - PAD.top - PAD.bottom);

  const path = figure.points
    .map((p, i) => `${i ? "L" : "M"} ${px(instantMs(p.t)).toFixed(1)} ${py(p.v).toFixed(1)}`)
    .join(" ");
  const gridY = [yMax, yMin]
    .map(
      (v) => `<line x1="${PAD.left}" y1="${py(v).toFixed(1)}" x2="${W - PAD.right}" y2="${py(v).toFixed(1)}"
        stroke="var(--line)" stroke-width="1" stroke-dasharray="2 4"/>
      <text x="${PAD.left - 6}" y="${(py(v) + 3).toFixed(1)}" text-anchor="end" class="fig-tick">${esc(figValue(v))}</text>`,
    )
    .join("");
  const markers = (figure.markers || [])
    .map((m) => {
      const x = px(instantMs(m.t));
      // A center-anchored label at either plot edge would clip outside the
      // viewBox (the Elastos halt marker sits on the final sample); re-anchor
      // near the boundaries so the text stays inside the chart.
      const EDGE = 60;
      const anchor = x < PAD.left + EDGE ? "start" : x > W - PAD.right - EDGE ? "end" : "middle";
      return `<line x1="${x.toFixed(1)}" y1="${PAD.top - 4}" x2="${x.toFixed(1)}" y2="${H - PAD.bottom}"
          stroke="var(--line-strong)" stroke-width="1" stroke-dasharray="3 3"/>
        <text x="${x.toFixed(1)}" y="${PAD.top - 7}" text-anchor="${anchor}" class="fig-tick">${esc(m.label)}</text>`;
    })
    .join("");
  const last = figure.points[figure.points.length - 1];
  const endDot = `<circle cx="${px(instantMs(last.t)).toFixed(1)}" cy="${py(last.v).toFixed(1)}" r="3.5"
    fill="var(--focus)" stroke="var(--surface)" stroke-width="1.5"/>`;
  const xLabels = `
    <text x="${PAD.left}" y="${H - 5}" class="fig-tick">${esc(figure.points[0].t.slice(0, 10))}</text>
    <text x="${W - PAD.right}" y="${H - 5}" text-anchor="end" class="fig-tick">${esc(last.t.slice(0, 10))}</text>`;

  return `
    <figure class="finding-figure">
      <svg viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(figure.caption)}">
        ${gridY}${markers}
        <path d="${path}" fill="none" stroke="var(--focus)" stroke-width="2"
          stroke-linejoin="round" stroke-linecap="round"/>
        ${endDot}${xLabels}
      </svg>
      <figcaption>${esc(figure.caption)} <span class="fig-ylabel">(${esc(figure.y_label)})</span></figcaption>
      <span class="fig-note">${esc(figure.note)}</span>
    </figure>`;
}

/// The article: nav, meta, cited prose paragraphs, figures, anchors, Sources.
/// `index`/`total` position the finding in corpus order for the newer/older
/// controls (index 0 is newest).
function renderArticle(finding, index, total) {
  const meta = [
    `Observed <span class="fa-value">${esc(finding.observed_at)}</span>${
      finding.observed_until ? ` to <span class="fa-value">${esc(finding.observed_until)}</span>` : ""
    }`,
    `Published <span class="fa-value">${esc(finding.published_at)}</span>`,
    `Affects <span class="fa-value">${finding.affected_sources.map(esc).join(" · ")}</span>`,
  ].join('<span class="fa-sep">·</span>');
  const paragraphs = finding.body
    .split("\n\n")
    .map((p) => `<p>${formatCitedText(p, finding.references, null)}</p>`)
    .join("");
  const figures = (finding.figures || []).map(renderFigure).join("");
  const anchors = finding.anchors?.length
    ? `<div class="finding-anchors"><h2>Evidence anchors</h2>
        <div class="finding-anchor-row">${finding.anchors.map(anchorControl).join("")}</div></div>`
    : "";
  const cited = collectCitedReferenceIds(finding.summary, finding.body);
  return `
    <div class="finding-article-nav">
      <button type="button" class="finding-nav-btn" data-action="findings-back">&#8592; All findings</button>
      <div class="finding-nav-group">
        <button type="button" class="finding-nav-btn" data-action="findings-newer" ${index <= 0 ? "disabled" : ""}>&#8592; Newer</button>
        <button type="button" class="finding-nav-btn" data-action="findings-older" ${index >= total - 1 ? "disabled" : ""}>Older &#8594;</button>
      </div>
    </div>
    <div class="finding-article-head">
      <div class="finding-card-top">${categoryChip(finding.category)}${statusPill(finding.status)}</div>
      <h2 class="finding-article-title">${esc(finding.title)}</h2>
      <div class="finding-article-meta">${meta}</div>
    </div>
    <div class="finding-article-body">${paragraphs}</div>
    ${figures}
    ${anchors}
    <div class="finding-sources">${renderSourcesSection(finding.references, cited, null)}</div>`;
}

export { CATEGORIES, STATUSES, renderFeed, renderArticle };
