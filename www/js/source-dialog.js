// Source modal body: a three-tab view (General | Technical | Capture) over a
// source. General/Technical come from the generated CHAIN_PROFILES editorial
// corpus (authored in data/sources/chain_profiles.json,
// emitted by `just gen-source-artifacts`); Capture combines editorial provenance
// with the live /sources payload.
// Extracted from controls.js so neither file exceeds the arch-lint budget.
import { CHAIN_PROFILES, SOURCE_LIFECYCLE } from "./source-registry.generated.js?v=0.2.1";
import { esc, formatScalar, relativeTime, sourceChain } from "./frontend-state.js?v=0.2.1";
import { kvRows } from "./drawer-renderer.js?v=0.2.1";
import { sourceSyncLabel } from "./source-status.js?v=0.2.1";

const TABS = [
  { id: "history", label: "General" },
  { id: "technical", label: "Technical" },
  { id: "capture", label: "Capture" },
];

// `chain_status` is a modal-only editorial label. Source grouping/filtering uses
// (kind, SOURCE_LIFECYCLE) via relationshipChip(), not these status labels.
const STATUS_LABELS = { active: "Active", zombie: "Zombie", dormant: "Dormant", dead: "Dead" };

// How this monitor relates to the chain, from (kind, lifecycle) - NOT lifecycle
// alone, so the Bitcoin Core parent context is never labelled a producer.
function relationshipChip(source) {
  const kind = source?.kind || String(source?.code || "").split(":")[0] || "";
  const lifecycle = SOURCE_LIFECYCLE[source?.code] || source?.lifecycle || "";
  if (kind === "live-chaintip") return { label: "Bitcoin Core parent chain", cls: "parent" };
  if (kind === "auxpow" && lifecycle === "catalogued") return { label: "Catalogued (not recovered)", cls: "catalogued" };
  if (kind === "auxpow" && lifecycle === "surveyed") return { label: "Recovered survey", cls: "surveyed" };
  if (kind === "auxpow" && lifecycle === "partial") return { label: "Recovered subset", cls: "partial" };
  if (kind === "auxpow" && lifecycle === "historical") return { label: "Recovered dataset", cls: "historical" };
  if (kind === "auxpow") return { label: "Live AuxPoW producer", cls: "live" };
  return { label: "Registered source", cls: "other" };
}

// The modal subhead (kicker): the chain's editorial tagline, or "" so the caller
// can fall back to the registry meta for a source without a profile.
function sourceTagline(source) {
  return CHAIN_PROFILES[sourceChain(source)]?.tagline || "";
}

function chip(text, cls) {
  return `<span class="sd-chip sd-chip-${esc(cls)}">${esc(text)}</span>`;
}

// One [^N] citation marker -> a superscript link to reference N. url/label are
// escaped (kvRows inserts values unescaped, so escaping here is the only defense).
function citeLink(n, refs, suffix = "", localMap = null) {
  const ref = refs.find((r) => r.id === n);
  if (!ref) return null;
  // localMap renumbers the DISPLAYED marker to a per-tab local 1..k while the
  // link still resolves to the global reference; null keeps global numbering.
  const display = localMap ? (localMap.get(n) ?? n) : n;
  const label = esc(ref.label);
  return `<sup class="sd-cite"><a href="${esc(ref.url)}" target="_blank" rel="noopener noreferrer" aria-label="Reference ${display}: ${label}" title="${label}">${display}${suffix}</a></sup>`;
}

// Inline prose formatter for the editorial corpus. INVARIANT: for marker-free
// text this is HTML-equivalent to esc(); it only ever emits <code>/<sup>/<a>
// with every attribute escaped. esc() first, then tokenize on `code` spans so
// [^N] markers inside backticks stay literal, then expand markers elsewhere.
function fmt(text, refs = [], localMap = null) {
  return esc(text)
    .split(/(`[^`]+`)/)
    .map((seg) =>
      seg.length >= 2 && seg.startsWith("`") && seg.endsWith("`")
        ? `<code>${seg.slice(1, -1)}</code>`
        : seg.replace(/\[\^(\d+)\](?=\[\^\d+\])/g, (m, n) => citeLink(Number(n), refs, ", ", localMap) || m)
          .replace(/\[\^(\d+)\]/g, (m, n) => citeLink(Number(n), refs, "", localMap) || m),
    )
    .join("");
}

// The ascending set of [^N] ids cited across a tab's fields, for its Sources list.
function collectCites(...texts) {
  const ids = new Set();
  for (const t of texts) {
    for (const m of String(t ?? "").matchAll(/\[\^(\d+)\]/g)) ids.add(Number(m[1]));
  }
  return [...ids].sort((a, b) => a - b);
}

// Per-tab citation renumbering: map each cited GLOBAL reference id to its local
// 1..k position (ascending). Threaded into fmt()/sourcesSection() so a tab's
// markers and Sources list both read 1..k with no gaps. A null map (used by the
// shared kind dialog) keeps the original global numbering.
function localRefMap(citedIds) {
  return new Map(citedIds.map((id, index) => [id, index + 1]));
}

function paras(refs, localMap, ...texts) {
  return texts
    .flatMap((t) => String(t ?? "").split(/\n\s*\n/))
    .filter((t) => t.trim())
    .map((t) => `<p>${fmt(t, refs, localMap)}</p>`)
    .join("");
}

function subhead(text) {
  return `<div class="source-dialog-subhead">${esc(text)}</div>`;
}

function numberValue(value) {
  if (value == null) return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : null;
}

function height(value) {
  return numberValue(value)?.toLocaleString("en-US") || null;
}

function progressLabel(sync) {
  const progress = height(sync.progress_height);
  const target = height(sync.target_height);
  if (progress && target) return `${progress} / ${target}`;
  return progress;
}

function updatedLabel(sync) {
  const updatedAt = numberValue(sync.progress_updated_at);
  return updatedAt == null ? null : relativeTime(new Date(updatedAt * 1000));
}

function errorLabel(sync) {
  if (!sync.error_code) return null;
  return sync.error_height == null
    ? sync.error_code
    : `${sync.error_code} at ${height(sync.error_height)}`;
}

function operationalRows(source, sync) {
  return [
    ["Status", sourceSyncLabel(source)],
    ["Progress", progressLabel(sync)],
    ["Updated", updatedLabel(sync)],
    ["Last error", errorLabel(sync)],
  ]
    .filter(([, value]) => value !== null && value !== undefined)
    .map(([label, value]) => [label, formatScalar(value)]);
}

function operationalBlock(source, help) {
  const rows = [
    ...(help ? [["Evidence", formatScalar(help.data)], ["Pool attribution", formatScalar(help.pool)]] : []),
    ...operationalRows(source, source.sync || {}),
  ];
  return rows.length ? subhead("Operational status") + kvRows(rows) : "";
}

function bullets(refs, localMap, items) {
  if (!items || !items.length) return "";
  return `<ul class="sd-notable">${items.map((i) => `<li>${fmt(i, refs, localMap)}</li>`).join("")}</ul>`;
}

// Per-tab "Sources" list. With a localMap the items are numbered 1..k locally
// (matching the renumbered inline markers); without one (the shared kind dialog)
// they keep their global reference id, so that caller is unaffected.
function sourcesSection(refs, citedIds, localMap = null) {
  const byId = new Map(refs.map((r) => [r.id, r]));
  const items = citedIds
    .map((id) => byId.get(id))
    .filter(Boolean)
    .map((r) => {
      const num = localMap ? (localMap.get(r.id) ?? r.id) : r.id;
      return `<li value="${num}"><a href="${esc(r.url)}" target="_blank" rel="noopener noreferrer">${esc(r.label)}</a></li>`;
    })
    .join("");
  return items ? subhead("Sources") + `<ol class="sd-sources">${items}</ol>` : "";
}

function historyPanel(source, profile) {
  const refs = profile.references || [];
  const h = profile.history;
  const cited = collectCites(profile.status_detail, h.founded, h.merge_mining, h.ended, h.narrative);
  const localMap = localRefMap(cited);
  // ONE source-class chip (the monitor's relationship to the source). The
  // chain's own state (active/zombie/dormant/dead) moves to a scoped "Chain status"
  // row, so it never reads as the source's evidence-liveness.
  const rel = relationshipChip(source);
  const chips = `<div class="sd-chips">${chip(rel.label, `rel-${rel.cls}`)}</div>`;
  const chainStatus = STATUS_LABELS[profile.chain_status] || "Unknown";
  const detail = profile.status_detail ? `<p class="sd-status-detail">${fmt(profile.status_detail, refs, localMap)}</p>` : "";
  const facts = kvRows([
    ["Founded", fmt(h.founded, refs, localMap)],
    ["Merge-mining", fmt(h.merge_mining, refs, localMap)],
    ["Chain status", `<strong class="sd-chain-status-${esc(profile.chain_status)}">${esc(chainStatus)}.</strong> ${fmt(h.ended, refs, localMap)}`],
  ]);
  return chips + detail + facts + paras(refs, localMap, h.narrative) + sourcesSection(refs, cited, localMap);
}

function keyFactsBlock(refs, localMap, keyFacts) {
  if (!keyFacts || !keyFacts.length) return "";
  return subhead("At a glance") + kvRows(keyFacts.map((k) => [k.label, fmt(k.value, refs, localMap)]));
}

// Technical is the chain's merge-mining science only; how/where this monitor
// captures the data lives in the Capture tab (capturePanel).
function technicalPanel(profile) {
  const refs = profile.references || [];
  const t = profile.technical;
  const cited = collectCites(
    t.mechanism, t.uniqueness,
    ...(t.notable || []), ...(t.key_facts || []).map((k) => k.value),
  );
  const localMap = localRefMap(cited);
  return [
    keyFactsBlock(refs, localMap, t.key_facts),
    paras(refs, localMap, t.mechanism),
    t.uniqueness ? subhead("What's distinctive") + paras(refs, localMap, t.uniqueness) : "",
    t.notable && t.notable.length ? subhead("Notable") + bullets(refs, localMap, t.notable) : "",
    sourcesSection(refs, cited, localMap),
  ].join("");
}

// Capture tab: how/where this monitor captures the chain's evidence. Editorial
// (derivation method, provenance + coverage window) over the live operational
// state (per-source help, sync progress, evidence counts). `profile` is null on
// the defensive no-profile fallback, leaving only the operational rows.
function capturePanel(source, profile) {
  const refs = profile?.references || [];
  const t = profile?.technical || {};
  const prov = profile?.provenance;
  const help = profile?.help;
  const cited = collectCites(
    t.capture, prov?.source, prov?.coverage, t.bitcoin_relevance, ...(t.recovery || []),
  );
  const localMap = localRefMap(cited);
  const derive = t.capture ? subhead("How this monitor derives it") + paras(refs, localMap, t.capture) : "";
  const provenance = prov
    ? subhead("Provenance & coverage") +
      kvRows([["Source", fmt(prov.source, refs, localMap)], ["Coverage", fmt(prov.coverage, refs, localMap)]])
    : "";
  // Recovery: the chain's Bitcoin-recovery yield (was "Why it matters for
  // Bitcoin") plus the recovery-outcome bullets (counts, windows, novelty),
  // moved out of the Technical tab so Technical stays pure chain-science.
  const relevanceHeading = sourceChain(source) === "bitcoin" ? "Placement" : "Recovery";
  const recovery = t.bitcoin_relevance
    ? subhead(relevanceHeading) + paras(refs, localMap, t.bitcoin_relevance) + bullets(refs, localMap, t.recovery || [])
    : "";
  // Catalogued and surveyed sources have no admissible evidence: suppress both
  // the operational block and the always-zero evidence-count table.
  const lifecycle = SOURCE_LIFECYCLE[source?.code] || source?.lifecycle;
  const hasNoEvidence = lifecycle === "catalogued" || lifecycle === "surveyed";
  const operational = hasNoEvidence ? "" : operationalBlock(source, help);
  // Live/parent sources carry a `help` block; historical recovered-dataset
  // sources omit it and show only their operational source status.
  // Near/Unknown are deliberately not surfaced here (see CHANGELOG + e2e).
  const counts = !hasNoEvidence && sourceChain(source) !== "bitcoin" && source.counts
    ? kvRows([
        ["Events", formatScalar(source.counts.events)],
        ["Canonical", formatScalar(source.counts.canonical)],
        ["Stale", formatScalar(source.counts.stale)],
        ["Strict orphans", formatScalar(source.counts.strict_orphan)],
        ["Weak orphans", formatScalar(source.counts.weak_orphan)],
      ])
    : "";
  return (
    derive +
    provenance +
    recovery +
    operational +
    (counts ? subhead("Current Evidence Counts") + counts : "") +
    sourcesSection(refs, cited, localMap)
  );
}

// Render the tabbed source modal body. Falls back to a Capture-only render when
// a chain has no editorial profile (defensive; the Rust completeness test makes
// that unreachable for registry chains in practice).
function renderSourceDialog(source) {
  const profile = CHAIN_PROFILES[sourceChain(source)];
  if (!profile) return capturePanel(source, null);
  const panels = {
    history: historyPanel(source, profile),
    technical: technicalPanel(profile),
    capture: capturePanel(source, profile),
  };
  const tablist = TABS.map((tab, index) => {
    const selected = index === 0;
    return `<button class="modal-tab" type="button" role="tab" id="sd-tab-${tab.id}" aria-controls="sd-panel-${tab.id}" aria-selected="${selected}"${selected ? "" : ' tabindex="-1"'} data-tab="${tab.id}">${esc(tab.label)}</button>`;
  }).join("");
  const tabpanels = TABS.map((tab, index) => {
    const hidden = index === 0 ? "" : " hidden";
    return `<div class="modal-tabpanel" role="tabpanel" id="sd-panel-${tab.id}" aria-labelledby="sd-tab-${tab.id}"${hidden}>${panels[tab.id]}</div>`;
  }).join("");
  return `<div class="modal-tabs">
    <div class="modal-tablist" role="tablist" aria-label="Source details">${tablist}</div>
    ${tabpanels}
  </div>`;
}

export {
  collectCites as collectCitedReferenceIds,
  fmt as formatCitedText,
  renderSourceDialog,
  sourceTagline,
  sourcesSection as renderSourcesSection,
};
