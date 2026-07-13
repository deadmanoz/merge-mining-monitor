import { $, compareSourcesForDisplay, esc, relativeTime, sourceDisplayName, state } from "./frontend-state.js?v=0.2.1";


const OPERATIONAL_SOURCE_SYNC_MODES = new Set(["live", "bitcoin-core-backbone"]);
const SOURCE_SYNC_ORDER = ["error", "stale", "catching_up", "not_started", "unknown", "live"];
const SOURCE_SYNC_META = {
  live: { label: "live", displayLabel: "Live", title: "Progress advanced within the past hour" },
  catching_up: {
    label: "catching up",
    displayLabel: "Catching up",
    title: "Progress is recent but has not reached the target height",
  },
  stale: {
    label: "stale",
    displayLabel: "Stale",
    title: "Progress has not updated within the past hour",
  },
  error: { label: "error", displayLabel: "Error", title: "The last source progress update recorded an error" },
  not_started: { label: "not started", displayLabel: "Not started", title: "No source progress recorded yet" },
  historical: { label: "historical", displayLabel: "Historical", title: "Historical archive source" },
  partial: { label: "partial", displayLabel: "Partial", title: "Recovered evidence from an incomplete child-chain record" },
  surveyed: { label: "surveyed", displayLabel: "Surveyed", title: "Recovery completed; no Bitcoin block winner found" },
  catalogued: { label: "catalogued", displayLabel: "Catalogued", title: "Catalogued chain; not polled or recovered" },
  unknown: { label: "unknown", displayLabel: "Unknown", title: "Source progress is unknown" },
};
const SOURCE_SYNC_MODE_META = {
  live: "live capture",
  "bitcoin-core-backbone": "Bitcoin Core backbone",
  historical: "historical archive",
  partial: "recovered subset",
  surveyed: "recovered survey",
  catalogued: "catalogued",
  unknown: "unknown mode",
};
const SOURCE_STATUS_HELP = {
  cursorUpdated: "When the progress cursor was last seeded or advanced. This is not a process heartbeat.",
  latestEvidence: "Newest accepted AuxPoW evidence timestamp. This can be older than the live cursor when recent blocks are near shares or non-BTC parents.",
};

function normalizeSourceSync(source) {
  const sync = source?.sync || {};
  const stateName = SOURCE_SYNC_META[sync.state] ? sync.state : "unknown";
  const mode = SOURCE_SYNC_MODE_META[sync.mode] ? sync.mode : "unknown";
  const progressHeight = Number.isFinite(sync.progress_height) ? Number(sync.progress_height) : null;
  const progressUpdatedAt = Number.isFinite(sync.progress_updated_at) ? Number(sync.progress_updated_at) : null;
  const targetHeight = Number.isFinite(sync.target_height) ? Number(sync.target_height) : null;
  const latestEvidenceAt = Number.isFinite(sync.latest_evidence_at) ? Number(sync.latest_evidence_at) : null;
  const errorHeight = Number.isFinite(sync.error_height) ? Number(sync.error_height) : null;
  const errorCode = typeof sync.error_code === "string" && sync.error_code ? sync.error_code : null;
  return {
    mode,
    state: stateName,
    progress_height: progressHeight,
    progress_updated_at: progressUpdatedAt,
    target_height: targetHeight,
    latest_evidence_at: latestEvidenceAt,
    error_code: errorCode,
    error_height: errorHeight,
  };
}

function isOperationalSource(source) {
  return OPERATIONAL_SOURCE_SYNC_MODES.has(normalizeSourceSync(source).mode);
}

function sourceSyncLabel(source) {
  const sync = normalizeSourceSync(source);
  return SOURCE_SYNC_META[sync.state].displayLabel;
}

function syncStateClass(stateName) {
  return String(stateName || "unknown").replaceAll("_", "-");
}

function sourceHeightLabel(sync) {
  if (sync.progress_height == null) return "No height";
  const progress = sync.progress_height.toLocaleString("en-US");
  if (sync.mode === "live" && sync.target_height != null) {
    return `${progress} / ${sync.target_height.toLocaleString("en-US")}`;
  }
  return progress;
}

function sourceHeightTitle(sync) {
  const base =
    sync.mode === "bitcoin-core-backbone"
      ? "Current contiguous Bitcoin Core backbone height recorded by the sync daemon."
      : "Latest recorded capture progress height for this source.";
  if (sync.progress_height == null) return "No capture progress height has been recorded yet.";
  if (sync.target_height == null) return base;
  return `${base} Target height: ${sync.target_height.toLocaleString("en-US")}.`;
}

function sourceCursorUpdatedLabel(sync) {
  if (sync.progress_updated_at == null) return "No update";
  return relativeTime(new Date(sync.progress_updated_at * 1000));
}

function sourceCursorUpdatedTitle(sync) {
  if (sync.progress_updated_at == null) {
    return "No cursor seed or advancement time has been recorded yet.";
  }
  return SOURCE_STATUS_HELP.cursorUpdated;
}

function sourceEvidenceLabel(sync) {
  if (sync.latest_evidence_at != null) {
    return relativeTime(new Date(sync.latest_evidence_at * 1000));
  }
  if (sync.mode === "bitcoin-core-backbone") return "N/A";
  return "No evidence";
}

function sourceEvidenceTitle(sync) {
  if (sync.mode === "bitcoin-core-backbone") {
    return "Bitcoin Core is the backbone source, not an AuxPoW evidence source.";
  }
  return SOURCE_STATUS_HELP.latestEvidence;
}

function sourceStatusHeaderHelp(key) {
  const title = SOURCE_STATUS_HELP[key] || "";
  return `<span class="source-status-header-help" tabindex="0" role="img" aria-label="${esc(title)}" title="${esc(title)}">?</span>`;
}

function sourceErrorLabel(sync) {
  if (!sync.error_code) return "";
  return sync.error_height == null
    ? `error ${sync.error_code}`
    : `error ${sync.error_code} at ${sync.error_height.toLocaleString("en-US")}`;
}

function sourceStatusDetail(sync) {
  const parts = [
    `height: ${sourceHeightLabel(sync)}`,
    `cursor updated: ${sourceCursorUpdatedLabel(sync)}`,
    `latest evidence: ${sourceEvidenceLabel(sync)}`,
  ];
  const error = sourceErrorLabel(sync);
  if (error) parts.push(error);
  return parts.join(" · ");
}

function sourceSyncTitle(source) {
  const sync = normalizeSourceSync(source);
  const mode = SOURCE_SYNC_MODE_META[sync.mode];
  const detail = SOURCE_SYNC_META[sync.state].title;
  return `${sourceDisplayName(source)}: ${sourceSyncLabel(source)}. ${mode}. ${detail}. ${sourceStatusDetail(sync)}`;
}

function renderSourceRailStatus(source) {
  if (!isOperationalSource(source)) return "";
  const sync = normalizeSourceSync(source);
  const cls = syncStateClass(sync.state);
  return `<span class="source-sync-mark source-sync-state-${esc(cls)}" role="img" aria-label="${esc(sourceSyncTitle(source))}" title="${esc(sourceSyncTitle(source))}"></span>`;
}

function summarizeSourceSync(sources) {
  const counts = Object.fromEntries(SOURCE_SYNC_ORDER.map((stateName) => [stateName, 0]));
  for (const source of sources) counts[normalizeSourceSync(source).state] += 1;
  return counts;
}

function sourceStatusSummary(counts) {
  const parts = SOURCE_SYNC_ORDER
    .filter((stateName) => counts[stateName] > 0)
    .map((stateName) => `${counts[stateName].toLocaleString("en-US")} ${SOURCE_SYNC_META[stateName].label}`);
  return parts.length ? `Sources ${parts.join(" / ")}` : "Sources none";
}

function aggregateSyncState(counts, unavailable) {
  if (unavailable) return "unknown";
  if (counts.error > 0) return "error";
  if (counts.stale > 0) return "stale";
  if (counts.catching_up > 0) return "catching_up";
  if (counts.not_started > 0) return "not_started";
  if (counts.unknown > 0) return "unknown";
  if (counts.live > 0) return "live";
  return "unknown";
}

function renderSourceStatusRows(sources) {
  if (!sources.length) return `<div class="source-status-empty">No operational live sources</div>`;
  const rows = sources.slice().sort(compareSourcesForDisplay).map((source) => {
    const sync = normalizeSourceSync(source);
    const cls = syncStateClass(sync.state);
    const height = sourceHeightLabel(sync);
    const cursorUpdated = sourceCursorUpdatedLabel(sync);
    const evidence = sourceEvidenceLabel(sync);
    const error = sourceErrorLabel(sync);
    return `<tr>
      <td><span class="source-status-name" title="${esc(source.code)}">${esc(sourceDisplayName(source))}</span></td>
      <td><span class="source-status-pill source-sync-state-${esc(cls)}" title="${esc(sourceSyncTitle(source))}">${esc(sourceSyncLabel(source))}</span></td>
      <td class="source-status-height" title="${esc(sourceHeightTitle(sync))}">${esc(height)}</td>
      <td class="source-status-progress" title="${esc(sourceCursorUpdatedTitle(sync))}"><span>${esc(cursorUpdated)}</span>${error ? `<span class="source-status-error">${esc(error)}</span>` : ""}</td>
      <td class="source-status-progress" title="${esc(sourceEvidenceTitle(sync))}">${esc(evidence)}</td>
    </tr>`;
  }).join("");
  return `<div class="source-status-table-wrap"><table class="source-status-table">
    <thead><tr>
      <th>Source</th>
      <th>Status</th>
      <th title="Latest recorded capture progress height for this source.">Height</th>
      <th title="${esc(SOURCE_STATUS_HELP.cursorUpdated)}"><span class="source-status-heading-label">Cursor Updated ${sourceStatusHeaderHelp("cursorUpdated")}</span></th>
      <th title="${esc(SOURCE_STATUS_HELP.latestEvidence)}"><span class="source-status-heading-label">Latest Evidence ${sourceStatusHeaderHelp("latestEvidence")}</span></th>
    </tr></thead>
    <tbody>${rows}</tbody>
  </table></div>`;
}

function renderSourceStatus(sources = [], { unavailable = false } = {}) {
  const button = $("#source-status-button");
  const label = $("#source-status-label");
  const count = $("#source-status-count");
  const content = $("#source-status-content");
  if (!button || !label || !content) return;
  const operationalSources = sources.filter(isOperationalSource);
  const counts = summarizeSourceSync(operationalSources);
  const aggregate = aggregateSyncState(counts, unavailable);
  button.dataset.state = syncStateClass(aggregate);
  label.textContent = unavailable ? "Sources unavailable" : sourceStatusSummary(counts);
  button.setAttribute("aria-label", unavailable ? "Source registry unavailable" : label.textContent);
  if (count) {
    count.textContent = unavailable ? "" : `${operationalSources.length.toLocaleString("en-US")} operational`;
  }
  if (unavailable) {
    content.innerHTML = `<div class="source-status-empty" role="status">Source registry unavailable</div>`;
  } else {
    content.innerHTML = renderSourceStatusRows(operationalSources);
  }
}

function setSourceStatusPopover(open) {
  const button = $("#source-status-button");
  const popover = $("#source-status-popover");
  if (!button || !popover) return;
  button.setAttribute("aria-expanded", open ? "true" : "false");
  popover.hidden = !open;
}

function wireSourceStatusPopover() {
  const button = $("#source-status-button");
  const shell = $(".source-status-shell");
  if (!button || !shell) return;
  button.addEventListener("click", () => {
    setSourceStatusPopover(button.getAttribute("aria-expanded") !== "true");
  });
  document.addEventListener("click", (event) => {
    if (!shell.contains(event.target)) setSourceStatusPopover(false);
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape") setSourceStatusPopover(false);
  });
}

export {
  sourceSyncLabel,
  renderSourceRailStatus,
  renderSourceStatus,
  wireSourceStatusPopover,
};
