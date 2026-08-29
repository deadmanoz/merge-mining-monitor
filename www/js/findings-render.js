// Pure renderers for the findings view: the month-grouped feed, the article,
// and the structured evidence figures. No state reads and no wiring here; the
// view module owns both. Citation markup reuses the source-dialog helpers so
// `[^N]` markers, `code` spans, and the Sources list render identically to the
// chain profiles (and the jscpd gate stays quiet).

import { esc } from "./frontend-state.js?v=0.7.6";
import { renderFigure } from "./findings-figures.js?v=0.7.6";
import {
  collectCitedReferenceIds,
  formatCitedText,
  renderSourcesSection,
} from "./source-dialog.js?v=0.7.6";

// Category/status presentation. Colors are existing semantic tokens, chosen
// once here so chips and rail dots stay in sync.
const CATEGORIES = {
  "hashrate-shift": { label: "Hashrate shift", color: "var(--near)" },
  "pool-exit": { label: "Pool exit", color: "var(--unknown)" },
  "pool-incident": { label: "Pool incident", color: "var(--orphan-strict)" },
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
