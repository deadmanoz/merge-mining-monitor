// Source modal body: General, Technical, and Capture tabs over CHAIN_PROFILES and /sources.
import { CHAIN_PROFILES, SOURCE_LIFECYCLE } from "./source-registry.generated.js";
import { esc, formatScalar, kvRows, relativeTime, sourceChain } from "./frontend-state.js";
import { sourceSyncLabel, sourceRuntimeRows } from "./source-status.js";

const TABS = [
  { id: "history", label: "General" },
  { id: "technical", label: "Technical" },
  { id: "capture", label: "Capture" },
];

const STATUS_LABELS = { active: "Active", zombie: "Zombie", dormant: "Dormant", dead: "Dead", unknown: "Unknown" };

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

function sourceTagline(source) {
  return CHAIN_PROFILES[sourceChain(source)]?.tagline || "";
}

function chip(text, cls) {
  return `<span class="sd-chip sd-chip-${esc(cls)}">${esc(text)}</span>`;
}

function citeLink(n, refs, suffix = "", localMap = null) {
  const ref = refs.find((r) => r.id === n);
  if (!ref) return null;
  const display = localMap ? (localMap.get(n) ?? n) : n;
  const label = esc(ref.label);
  return `<sup class="sd-cite"><a href="${esc(ref.url)}" target="_blank" rel="noopener noreferrer" aria-label="Reference ${display}: ${label}" title="${label}">${display}${suffix}</a></sup>`;
}

// Formats backtick code spans and [^N] citation links with escaped attributes.
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

function collectCites(...texts) {
  const ids = new Set();
  for (const t of texts) {
    for (const m of String(t ?? "").matchAll(/\[\^(\d+)\]/g)) ids.add(Number(m[1]));
  }
  return [...ids].sort((a, b) => a - b);
}

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
    ...sourceRuntimeRows(source.sync || {}).map(([label, value]) => [label, formatScalar(value)]),
  ];
  return rows.length ? subhead("Operational status") + kvRows(rows) : "";
}

function bullets(refs, localMap, items) {
  if (!items || !items.length) return "";
  return `<ul class="sd-notable">${items.map((i) => `<li>${fmt(i, refs, localMap)}</li>`).join("")}</ul>`;
}

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

function capturePanel(source, profile) {
  const refs = profile?.references || [];
  const t = profile?.technical || {};
  const prov = profile?.provenance;
  const help = profile?.help;
  const cited = collectCites(
    t.capture,
    prov?.source,
    prov?.coverage,
    t.bitcoin_relevance,
    ...(Array.isArray(t.recovery) ? t.recovery : []),
  );
  const localMap = localRefMap(cited);
  const derive = t.capture ? subhead("How this monitor derives it") + paras(refs, localMap, t.capture) : "";
  const provenance = prov
    ? subhead("Provenance & coverage") +
      kvRows([["Source", fmt(prov.source, refs, localMap)], ["Coverage", fmt(prov.coverage, refs, localMap)]])
    : "";
  const relevanceHeading = sourceChain(source) === "bitcoin" ? "Placement" : "Recovery";
  const recovery = t.bitcoin_relevance
    ? subhead(relevanceHeading) +
      paras(refs, localMap, t.bitcoin_relevance) +
      (t.recovery && t.recovery.length ? bullets(refs, localMap, t.recovery) : "")
    : "";
  const lifecycle = SOURCE_LIFECYCLE[source?.code] || source?.lifecycle;
  const hasNoEvidence = lifecycle === "catalogued" || lifecycle === "surveyed";
  const operational = hasNoEvidence ? "" : operationalBlock(source, help);
  const counts = !hasNoEvidence && sourceChain(source) !== "bitcoin" && source.counts
    ? kvRows([
        ["Events", formatScalar(source.counts.events)],
        ["Canonical", formatScalar(source.counts.canonical)],
        ["Stale", formatScalar(source.counts.stale)],
        ["Invalid blocks", formatScalar(source.counts.invalid_block)],
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
