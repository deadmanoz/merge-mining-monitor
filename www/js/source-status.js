import { $, compareSourcesForDisplay, esc, relativeTime, sourceDisplayName } from "./frontend-state.js";

const OPERATIONAL_SOURCE_SYNC_MODES = new Set(["live", "bitcoin-core-backbone"]);
const SOURCE_SYNC_ORDER = ["error", "stale", "waiting", "catching_up", "not_started", "unknown", "live"];
const SOURCE_SYNC_META = {
  waiting: { label: "waiting", displayLabel: "Waiting", title: "Producer is running and waiting for an upstream dependency" },
  live: { label: "live", displayLabel: "Live", title: "Producer completed a healthy cycle, or legacy progress advanced within the past hour" },
  catching_up: {
    label: "catching up",
    displayLabel: "Catching up",
    title: "Producer is starting, recovering, or catching up to the target height",
  },
  stale: {
    label: "stale",
    displayLabel: "Stale",
    title: "Producer heartbeat expired or stopped, or legacy progress is over an hour old",
  },
  error: { label: "error", displayLabel: "Error", title: "Source has an unresolved capture error or a fatal producer failure" },
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
  latestEvidence: "Newest accepted AuxPoW evidence, or newest near share first recorded. This can be older than the live cursor when recent blocks carry neither: blocks without AuxPoW, or parents from other chains, which are never stored as evidence.",
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
    runtime: sync.runtime || null,
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

const RUNTIME_REASONS = {
  core_unreachable: "Core is unreachable", core_timeout: "Core request timed out",
  core_warmup: "Core is warming up", core_initial_block_download: "Core is downloading blocks",
  core_header_lag: "Core is behind its headers", core_stale_tip: "Core tip is stale",
  core_cache_publication_pending: "Waiting to publish a consistent cache snapshot",
  core_cache_bootstrap_pending: "Building the Core header cache",
  core_reconciliation_pending: "Reconciling changed Core evidence",
  core_cursor_recovery_pending: "Repairing the Core cursor",
  capture_pending: "Capture needs a retry", startup_pending: "Starting capture",
  producer_no_progress: "Capture has not advanced", producer_failed: "Producer stopped after a failure",
  core_authentication: "Core authentication failed", core_wrong_network: "Core network is incorrect",
  core_invalid_response: "Core returned an invalid response",
};

function sourceRuntimeRows(sync) {
  const runtime = sync.runtime;
  if (!runtime) return [];
  const stamp = (value) => Number.isFinite(value) ? new Date(value * 1000).toISOString() : "None yet";
  return [
    ["Dependency", runtime.dependency === "bitcoin-core" ? "Bitcoin Core" : "None"],
    ["Runtime", RUNTIME_REASONS[runtime.reason_code] || (runtime.state === "healthy" ? "Healthy cycle completed" : "Recovery pending")],
    ["Last successful cycle", stamp(runtime.last_success_at)],
    ...(Number.isFinite(runtime.next_retry_at) ? [["Next retry", stamp(runtime.next_retry_at)]] : []),
    ["Heartbeat", stamp(runtime.heartbeat_at)],
  ];
}

function sourceStatusDetail(sync) {
  const parts = [
    `height: ${sourceHeightLabel(sync)}`,
    `cursor updated: ${sourceCursorUpdatedLabel(sync)}`,
    `latest evidence: ${sourceEvidenceLabel(sync)}`,
  ];
  parts.push(...sourceRuntimeRows(sync).map(([label, value]) => `${label}: ${value}`));
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
  if (counts.waiting > 0) return "waiting";
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
    const runtime = sourceRuntimeRows(sync).map(([label, value]) => `${label}: ${value}`).join(" · ");
    return `<tr>
      <td><span class="source-status-name" title="${esc(source.code)}">${esc(sourceDisplayName(source))}</span></td>
      <td><span class="source-status-pill source-sync-state-${esc(cls)}" title="${esc(sourceSyncTitle(source))}">${esc(sourceSyncLabel(source))}</span>${runtime ? `<div class="source-status-runtime">${esc(runtime)}</div>` : ""}</td>
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
  sourceRuntimeRows,
  sourceSyncLabel,
  renderSourceRailStatus,
  renderSourceStatus,
  wireSourceStatusPopover,
};
