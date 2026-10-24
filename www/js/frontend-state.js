import { CHAIN_COLORS, CHAIN_DISPLAY_NAMES, CHAIN_PROFILES, SOURCE_DISPLAY_ORDER, SOURCE_LIFECYCLE } from "./source-registry.generated.js";
import { inputDateTimeToUtc, utcDateTimeToInput } from "./tree-lookup.js";


const API_BASE = "/api/v1";
const KINDS = ["canonical", "stale", "invalid_block", "unknown"];
const VISIBLE_KIND_CONTROLS = ["canonical", "stale", "invalid_block"];
const KIND_HELP = {
  canonical: {
    name: "Canonical",
    meta: "On Bitcoin's active chain",
    criteria: "Bitcoin Core places this block on the active best chain.",
    interpretation: "Canonical blocks form the main chain spine, with merge-mined child-chain evidence attached to each block.",
    notes: [
      "Evidence outside the active chain is classified separately as stale, orphan, invalid-block or unresolved evidence.",
    ],
  },
  stale: {
    name: "Stale",
    meta: "Bitcoin branch, off the active chain",
    criteria: "Core observations or Core-backed predecessor and header checks place this header on a branch off Bitcoin's active chain.",
    interpretation: "A stale block belongs to a branch off the active chain, including blocks displaced by a reorganisation.[^1]",
    references: [
      { id: 1, label: "Lightspark glossary: Stale block", url: "https://www.lightspark.com/glossary/stale-block" },
    ],
    notes: [
      "Stale branch navigation steps through single-block and multi-block losing branches.",
      "Header or proof evidence does not establish validation of an unavailable block body.",
    ],
  },
  invalid_block: {
    name: "Invalid block",
    meta: "Bitcoin proof of work, consensus-invalid",
    criteria: "The header meets Bitcoin's proof-of-work target but has an established consensus failure from live median-time-past checks or the reviewed invalid-block catalogue.",
    interpretation: "Invalid blocks are kept separate so consensus-invalid work is never misread as a stale or orphan Bitcoin block.",
    notes: [
      "Block detail names the violated rule; invalid blocks are excluded from stale branches, orphan counts, and competitions.",
    ],
  },
  strict_btc_orphan: {
    name: "Strict orphan",
    meta: "BIP34 height and nBits agree",
    criteria: "The header has valid Bitcoin proof of work, is absent from Bitcoin Core, and carries a post-BIP34 coinbase height matching the header's nBits.",
    interpretation: "Matching BIP34 height and difficulty epoch let the tree place this Core-absent Bitcoin block at an exact fork height.[^1]",
    references: [
      { id: 1, label: "BIP 34: Block v2, Height in Coinbase", url: "https://bips.dev/34/" },
      { id: 2, label: "BIP 90: Buried Deployments", url: "https://bips.dev/90/" },
    ],
    notes: [
      "BIP34 requires the block height in the coinbase scriptSig from height 227,931.[^1][^2]",
      "Post-BIP34 observations without an authenticated Bitcoin coinbase height fall back to weak classification.",
    ],
  },
  weak_btc_orphan: {
    name: "Weak orphan",
    meta: "Timestamp nBits match, no BIP34 height",
    criteria: "The header has valid Bitcoin proof of work and is absent from Bitcoin Core, without an authenticated BIP34 coinbase height.",
    interpretation: "Placement uses the header timestamp and difficulty epoch rather than a coinbase height.",
    references: [
      { id: 1, label: "BIP 34: Block v2, Height in Coinbase", url: "https://bips.dev/34/" },
      { id: 2, label: "BIP 90: Buried Deployments", url: "https://bips.dev/90/" },
    ],
    notes: [
      "Covers pre-BIP34 blocks and post-BIP34 sources that omit the Bitcoin coinbase.[^1][^2]",
    ],
  },
};

const DELTA_HELP = {
  metric: {
    name: "Header time delta",
    meta: "canonical.btc_header_time \u2212 stale.btc_header_time",
    body: [
      "Each competition pairs a stale Bitcoin block with the canonical block at the same height. The delta subtracts the stale block's timestamp from the winner's, so a positive delta means the stale block carries the earlier self-reported clock timestamp; consensus only bounds header clocks between median-time-past and two hours ahead of network time.",
      "Multi-hour tails are largely pre-2017 miner-clock artefacts, such as height 153,211 where 18 stale blocks span weeks on a stuck tip. Pairs differing by more than a 32-bit second count are reported as unavailable and excluded from statistics.",
    ],
  },
};

const EDGE_KINDS = ["canonical", "stale_entry", "stale", "hidden", "orphan", "orphan_approx"];
const EDGE_LEGEND = [
  { label: "canonical chain", kinds: ["canonical"], swatch: "canonical" },
  { label: "stale branch", kinds: ["stale_entry", "stale"], swatch: "stale" },
  { label: "orphan fork", kinds: ["orphan"], swatch: "orphan" },
  { label: "orphan fork (approx)", kinds: ["orphan_approx"], swatch: "orphan_approx" },
  { label: "hidden span", kinds: ["hidden"], swatch: "hidden" },
];

const ORPHAN_LEGEND = [
  { label: "strict orphan", swatchClass: "fill-strict_btc_orphan" },
  { label: "weak orphan", swatchClass: "fill-weak_btc_orphan" },
];

// Core-gated refinement of kind='unknown' driving the `classification=` parameter.
const CLASSIFICATIONS = ["strict_btc_orphan", "weak_btc_orphan", "excluded", "pending"];
const CLASSIFICATION_DEFAULT = ["strict_btc_orphan", "weak_btc_orphan"];
const CLASSIFICATION_META = {
  strict_btc_orphan: { name: "Strict orphan", count: "strict" },
  weak_btc_orphan: { name: "Weak orphan", count: "weak" },
  excluded: { name: "Excluded", count: "excluded" },
  pending: { name: "Pending", count: "pending" },
};

const BLOCK_W = 110;
const BLOCK_H = 72;

const PROTOCOL_LABELS = { core: "CORE", exsat: "exSat" };

// Fallback palette for chains without an explicit entry in CHAIN_COLORS.
const CHAIN_FALLBACK_COLORS = [
  "#4e79a7", "#59a14f", "#e0843c", "#d35fb7", "#76b7b2",
  "#af7aa1", "#9c755f", "#5b8ff9", "#2ca58d", "#ff9da7",
];
const SOURCE_GROUPS = [
  {
    key: "live",
    title: "Live sources",
    meta: "Active producers, plus Bitcoin Core context.",
    defaultOpen: true,
  },
  {
    key: "historical",
    title: "Recovered datasets",
    meta: "Recovered AuxPoW datasets from historical chains.",
    defaultOpen: false,
  },
  {
    key: "partial",
    title: "Recovered subsets",
    meta: "Ingestible evidence recovered without the complete child blockchain.",
    defaultOpen: false,
  },
  {
    key: "surveyed",
    title: "Recovered surveys",
    meta: "Recovered chains reviewed with no admissible Bitcoin evidence.",
    defaultOpen: false,
  },
  {
    key: "catalogued",
    title: "Catalogued (not recovered)",
    meta: "Chains known to have BTC-merge-mined but with no recovered data.",
    defaultOpen: false,
  },
];
function chainColor(chain) {
  const key = String(chain || "").toLowerCase().replace(/^auxpow:/, "");
  if (CHAIN_COLORS[key]) return CHAIN_COLORS[key];
  let hash = 0;
  for (let index = 0; index < key.length; index += 1) {
    hash = (hash * 31 + key.charCodeAt(index)) >>> 0;
  }
  return CHAIN_FALLBACK_COLORS[hash % CHAIN_FALLBACK_COLORS.length] || "#888888";
}

function protocolLabel(protocol) {
  return PROTOCOL_LABELS[protocol] || protocol;
}

function protocolStatusLabel(status) {
  return status.replace("_", " ");
}

function sourceChain(value) {
  if (value && typeof value === "object") return String(value.chain || sourceChain(value.code) || "").toLowerCase();
  const text = String(value || "").toLowerCase();
  const parts = text.split(":");
  if (parts.length >= 2) return parts[1];
  return text.replace(/^auxpow:/, "");
}

function sourceCode(source) {
  if (source && typeof source === "object") return String(source.code || "");
  return String(source || "");
}

function chainDisplayName(chain) {
  const key = sourceChain(chain);
  if (CHAIN_DISPLAY_NAMES[key]) return CHAIN_DISPLAY_NAMES[key];
  if (!key) return "Unknown";
  return key.split("-").map((part) => part.charAt(0).toUpperCase() + part.slice(1)).join(" ");
}

function sourceDisplayName(source) {
  return chainDisplayName(sourceChain(source));
}

function sourceMeta(source) {
  const chain = sourceChain(source);
  return CHAIN_PROFILES[chain]?.byline || "Registered source";
}

function sourceDisplayRank(source) {
  const chain = sourceChain(source);
  return SOURCE_DISPLAY_ORDER[chain] ?? 50;
}

function sourceGroupKey(source) {
  const lifecycle = SOURCE_LIFECYCLE[sourceCode(source)] || source?.lifecycle;
  if (["historical", "partial", "surveyed", "catalogued"].includes(lifecycle)) return lifecycle;
  return "live";
}

function compareSourcesForDisplay(a, b) {
  const rank = sourceDisplayRank(a) - sourceDisplayRank(b);
  if (rank !== 0) return rank;
  const name = sourceDisplayName(a).localeCompare(sourceDisplayName(b));
  if (name !== 0) return name;
  return String(a?.code || a).localeCompare(String(b?.code || b));
}

function formatSourceList(codes = []) {
  if (!codes.length) return formatScalar([]);
  return codes.map((code) => esc(sourceDisplayName(code))).join(", ");
}

function formatSourceRef(source) {
  if (!source) return formatScalar(null);
  return esc(sourceDisplayName(source));
}

function kindHelpFor(kind) {
  return KIND_HELP[kind] || {
    name: kind ? kind.charAt(0).toUpperCase() + kind.slice(1) : "Unknown",
    meta: "Parent kind",
    criteria: "This parent kind is reported by the monitor API.",
    interpretation: "Use this state to understand how the Bitcoin parent header should be read in the tree.",
    notes: [],
  };
}

const DEFAULTS = {
  view: "tree",
  era: "",
  finding: "",
  treeHeight: "",
  treeTime: "",
  treeWindow: "",
  treeFrom: "",
  treeTo: "",
  treeTargetHeight: "",
  kinds: VISIBLE_KIND_CONTROLS.slice(),
  classification: CLASSIFICATION_DEFAULT.slice(),
  sources: [],
};

const state = {
  query: {
    ...DEFAULTS,
    kinds: DEFAULTS.kinds.slice(),
    classification: DEFAULTS.classification.slice(),
    sources: [],
    unheightedAnchor: "",
  },
  sources: null,
  sourceGroupOpen: {},
  tree: null,
  treeDirty: false,
  updatedAt: {},
  treeRepaintPending: false,
  competitions: null,
  findings: null,
  findingsUi: { hideCategories: [], hideStatuses: [], feedScroll: 0 },
  selectedHash: null,
  selectedBlock: null,
  // `source` is 'navigator' (menu/stepper) or 'selection' (node click).
  nav: { target: "tip", source: "navigator" },
  navEpoch: 0,
  treeLoadSeq: 0,
  // Must have a slot for every target in nav-targets.js.
  stale: { item: null, anchor: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  branch: { item: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  invalidBlock: { item: null, anchor: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  orphan: { item: null, anchor: null, total: null, counts: null, hasOlder: false, hasNewer: false, busy: false },
  orphanBranch: { item: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  treePointerActive: false,
  errors: {},
  seq: {},
};

function $(selector, root = document) {
  return root.querySelector(selector);
}

function $all(selector, root = document) {
  return Array.from(root.querySelectorAll(selector));
}

function esc(value) {
  return String(value ?? "")
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

const RELATIVE_TIME_UNITS = [
  ["year", 31536000],
  ["month", 2592000],
  ["week", 604800],
  ["day", 86400],
  ["hour", 3600],
  ["minute", 60],
  ["second", 1],
];

function relativeTime(date) {
  const diffSec = Math.round((date.getTime() - Date.now()) / 1000);
  const rtf = new Intl.RelativeTimeFormat(undefined, { numeric: "always" });
  for (const [unit, secs] of RELATIVE_TIME_UNITS) {
    if (Math.abs(diffSec) >= secs || unit === "second") {
      return rtf.format(Math.round(diffSec / secs), unit);
    }
  }
  return rtf.format(0, "second");
}

function formatEpoch(value) {
  if (value === null || value === undefined) return `<span class="null-value">null</span>`;
  const date = new Date(Number(value) * 1000);
  if (Number.isNaN(date.getTime())) return esc(value);
  const iso = esc(date.toISOString().replace(".000Z", "Z"));
  return `${iso} <span class="relative-time" data-epoch="${esc(value)}">(${esc(relativeTime(date))})</span>`;
}

function refreshRelativeTimes() {
  for (const el of $all(".relative-time[data-epoch]")) {
    const date = new Date(Number(el.dataset.epoch) * 1000);
    if (Number.isNaN(date.getTime())) continue;
    el.textContent = `(${relativeTime(date)})`;
  }
}

const RELATIVE_TIME_TICK_MS = 30000;
let relativeTimeTimer = null;

function startRelativeTimeTicker() {
  if (relativeTimeTimer) clearInterval(relativeTimeTimer);
  relativeTimeTimer = setInterval(refreshRelativeTimes, RELATIVE_TIME_TICK_MS);
}

function formatScalar(value) {
  if (value === null || value === undefined) return `<span class="null-value">null</span>`;
  if (value === true) return `<span class="true-value">true</span>`;
  if (value === false) return `<span class="false-value">false</span>`;
  if (Array.isArray(value) && value.length === 0) return `<span class="empty-array">[]</span>`;
  if (Array.isArray(value)) return esc(value.join(", "));
  if (typeof value === "object") return `<code>${esc(JSON.stringify(value))}</code>`;
  return esc(value);
}

function selectedKinds() {
  const checked = $all('input[name="kind"]:checked').map((input) => input.value);
  return checked.length ? checked : VISIBLE_KIND_CONTROLS.slice();
}

function selectedClassifications() {
  const checked = $all('input[name="classification"]:checked').map((input) => input.value);
  return checked.length ? checked : CLASSIFICATION_DEFAULT.slice();
}

function classificationParam() {
  const selected = state.query.classification?.length
    ? state.query.classification
    : CLASSIFICATION_DEFAULT;
  return selected.join(",");
}

function sameSet(a, b) {
  if (a.length !== b.length) return false;
  const set = new Set(a);
  return b.every((value) => set.has(value));
}

function matchesSourceFilter(values, selected) {
  return !selected?.length || values?.some((value) => selected.includes(value));
}

function parseEra(value) {
  const match = /^(\d{4})-(\d{4})$/.exec(value || "");
  return match ? [Number(match[1]), Number(match[2])] : null;
}

function selectedSources(root = document) {
  return $all('input[name="source"]:checked', root).map((input) => input.value).sort();
}

function readForm({ source = "form" } = {}) {
  const form = $("#filters");
  if (!form) return;
  const data = new FormData(form);
  const sourceInputs = $all('input[name="source"]', form);
  const readLookup = source !== "filter-change" && source !== "generated-window";
  let hasLookupIntent = false;
  state.query = {
    ...state.query,
    kinds: selectedKinds(),
    classification: selectedClassifications(),
    sources: sourceInputs.length ? selectedSources(form) : state.query.sources,
  };
  if (readLookup) {
    const rawTreeHeight = String(data.get("treeHeight") || "").trim();
    const rawTreeTime = String(data.get("treeTime") || "").trim();
    const treeHeight = rawTreeHeight;
    const treeTime = treeHeight !== "" ? "" : inputDateTimeToUtc(rawTreeTime);
    hasLookupIntent = treeHeight !== "" || rawTreeTime !== "";
    Object.assign(state.query, {
      treeHeight,
      treeTime,
    });
  }
  if (readLookup && (source === "lookup-commit" || hasLookupIntent)) {
    state.query.unheightedAnchor = "";
    state.query.treeWindow = "";
    state.query.treeFrom = "";
    state.query.treeTo = "";
    state.query.treeTargetHeight = "";
  }
}

function hydrateFormFromUrl() {
  const params = new URLSearchParams(window.location.search);
  const view = params.get("view");
  if (view) state.query.view = view;
  const era = params.get("era");
  if (parseEra(era)) state.query.era = era;
  const finding = params.get("finding");
  if (finding && state.query.view === "findings") state.query.finding = finding;
  const generatedFrom = params.get("tree_from");
  const generatedTo = params.get("tree_to");
  if (
    params.get("tree_window") === "generated"
    && /^\d+$/.test(generatedFrom || "")
    && /^\d+$/.test(generatedTo || "")
    && Number(generatedFrom) <= Number(generatedTo)
  ) {
    state.query.treeWindow = "generated";
    state.query.treeFrom = generatedFrom;
    state.query.treeTo = generatedTo;
    state.query.treeTargetHeight = /^\d+$/.test(params.get("tree_height") || "")
      ? params.get("tree_height")
      : "";
    state.query.treeHeight = "";
    state.query.treeTime = "";
    state.query.unheightedAnchor = "";
  } else if (params.has("tree_height")) {
    const height = params.get("tree_height");
    if (/^\d+$/.test(height)) {
      state.query.treeHeight = height;
      state.query.treeTime = "";
      state.query.unheightedAnchor = "";
      state.query.treeWindow = "";
      state.query.treeFrom = "";
      state.query.treeTo = "";
      state.query.treeTargetHeight = "";
    }
  } else if (params.has("tree_time")) {
    const time = inputDateTimeToUtc(params.get("tree_time"));
    if (time) {
      state.query.treeTime = time;
      state.query.treeHeight = "";
      state.query.unheightedAnchor = "";
      state.query.treeWindow = "";
      state.query.treeFrom = "";
      state.query.treeTo = "";
      state.query.treeTargetHeight = "";
    }
  }
  if (params.has("kinds")) {
    const kinds = params.get("kinds").split(",").filter((kind) => VISIBLE_KIND_CONTROLS.includes(kind));
    if (kinds.length) state.query.kinds = kinds;
  }
  if (params.has("classification")) {
    const classification = params
      .get("classification")
      .split(",")
      .filter((value) => CLASSIFICATION_DEFAULT.includes(value));
    if (classification.length) state.query.classification = classification;
  }
  if (params.has("sources")) {
    state.query.sources = params
      .get("sources")
      .split(",")
      .filter(Boolean)
      .filter((code) => SOURCE_LIFECYCLE[code])
      .filter((code) => !["catalogued", "surveyed"].includes(SOURCE_LIFECYCLE[code]))
      .sort();
  }
  if (params.has("unheighted_anchor")) {
    state.query.unheightedAnchor = params.get("unheighted_anchor");
    state.query.treeHeight = "";
    state.query.treeTime = "";
    state.query.treeWindow = "";
    state.query.treeFrom = "";
    state.query.treeTo = "";
    state.query.treeTargetHeight = "";
    state.nav = { target: "orphan", source: "navigator" };
  }
  if (params.has("selected")) {
    const selected = params.get("selected");
    state.selectedHash = /^[0-9a-fA-F]{64}$/.test(selected) ? selected.toLowerCase() : selected;
  }
}

function writeForm() {
  const form = $("#filters");
  if (!form) return;
  if (form.treeHeight) form.treeHeight.value = state.query.treeHeight;
  if (form.treeTime) {
    form.treeTime.value = utcDateTimeToInput(state.query.treeTime);
  }
  $all('input[name="kind"]', form).forEach((input) => {
    input.checked = state.query.kinds.includes(input.value);
  });
  $all('input[name="classification"]', form).forEach((input) => {
    input.checked = state.query.classification.includes(input.value);
  });
}


export {
  API_BASE,
  DEFAULTS,
  DELTA_HELP,
  KINDS,
  VISIBLE_KIND_CONTROLS,
  EDGE_KINDS,
  EDGE_LEGEND,
  ORPHAN_LEGEND,
  CLASSIFICATIONS,
  CLASSIFICATION_DEFAULT,
  CLASSIFICATION_META,
  BLOCK_W,
  BLOCK_H,
  SOURCE_GROUPS,
  chainColor,
  protocolLabel,
  protocolStatusLabel,
  sourceChain,
  chainDisplayName,
  sourceDisplayName,
  sourceMeta,
  sourceGroupKey,
  compareSourcesForDisplay,
  formatSourceList,
  formatSourceRef,
  kindHelpFor,
  state,
  $,
  $all,
  esc,
  relativeTime,
  formatEpoch,
  refreshRelativeTimes,
  startRelativeTimeTicker,
  formatScalar,
  classificationParam,
  matchesSourceFilter,
  parseEra,
  sameSet,
  readForm,
  hydrateFormFromUrl,
  writeForm,
};
