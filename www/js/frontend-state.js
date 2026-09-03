import { CHAIN_COLORS, CHAIN_DISPLAY_NAMES, CHAIN_PROFILES, SOURCE_DISPLAY_ORDER, SOURCE_LIFECYCLE } from "./source-registry.generated.js?v=0.7.12";
import { inputDateTimeToUtc, utcDateTimeToInput } from "./tree-lookup.js?v=0.7.12";


const API_BASE = "/api/v1";
const KINDS = ["canonical", "stale", "error_block", "unknown", "near"];
const VISIBLE_KIND_CONTROLS = ["canonical", "stale", "error_block"];
const KIND_HELP = {
  canonical: {
    name: "Canonical",
    meta: "On Bitcoin's active chain",
    criteria: "Bitcoin Core has placed this Bitcoin block on the active best chain.",
    interpretation: "Canonical is the main Bitcoin chain. In this view, the canonical chain is the spine: each Bitcoin block can show the merge-mined child chains that committed to it.",
    notes: [
      "Canonical means the block is part of Bitcoin's main chain, the active chain Bitcoin nodes build on.",
      "The view shows that Bitcoin chain first, with merge-mined child-chain evidence attached to each Bitcoin block.",
      "Blocks that competed with the main chain but lost appear as connected stale or orphan blocks.",
    ],
  },
  stale: {
    name: "Stale",
    meta: "Bitcoin-valid, off the active chain",
    criteria: "The block has valid Bitcoin proof of work, but it is not part of Bitcoin's active chain.",
    interpretation: "A stale block is a Bitcoin-valid block from a branch that lost the chain race.[^1] Bitcoin nodes build on the winning branch, but merge-mined child chains can still carry the losing block's header.",
    references: [
      { id: 1, label: "Lightspark glossary: Stale block", url: "https://www.lightspark.com/glossary/stale-block" },
    ],
    notes: [
      "Stale does not mean invalid. The block had enough proof of work for Bitcoin; it just was not part of the branch Bitcoin kept building on.",
      "Stale branch navigation follows those losing Bitcoin branches, including multi-block branches when present.",
    ],
  },
  error_block: {
    name: "Error block",
    meta: "Bitcoin proof of work, consensus-invalid",
    criteria: "The header meets Bitcoin's proof-of-work target but matches a pinned, reproducibly validated consensus-invalid error-block catalogue.",
    interpretation: "Error blocks are neither stale blocks nor BTC orphan evidence. They are retained so a full-proof-of-work header rejected by Bitcoin consensus cannot be misread as an off-chain Bitcoin block.",
    notes: [
      "The block detail names the primary consensus rule that the catalogue found it violates.",
      "Error blocks are shown separately and never contribute to stale branches, orphan classification, or competition statistics.",
    ],
  },
  unknown: {
    name: "Unknown",
    meta: "Bitcoin-valid, not placed yet",
    criteria: "The embedded parent header passes the Bitcoin target, but the monitor does not yet have Bitcoin-chain proof that classifies it as canonical or stale.",
    interpretation: "Unknown is the holding state for Bitcoin-PoW-valid evidence before chain placement is known. It deserves attention because it may later become canonical or stale when classification catches up.",
    notes: [
      "Unknown parents usually have no Bitcoin height in this UI, so they live on the unheighted/time axis rather than the height spine.",
      "A later classifier or repair pass can promote unknown evidence into canonical or stale context.",
    ],
  },
  near: {
    name: "Near",
    meta: "Fails Bitcoin target",
    criteria: "The child-chain evidence embeds a Bitcoin-shaped parent header, but that header does not satisfy Bitcoin's own proof-of-work target.",
    interpretation: "Near rows can still be legitimate child-chain evidence, but they are not Bitcoin-valid blocks. They are useful for near-miss analysis, miner attribution clues, and checking producer behavior.",
    notes: [
      "Near is separated from unknown so Bitcoin-PoW-valid evidence is not mixed with parent headers that fail the Bitcoin target.",
      "Child-chain acceptance rules can differ from Bitcoin's parent target, so near does not automatically mean the child block was invalid.",
    ],
  },
  strict_btc_orphan: {
    name: "Strict orphan",
    meta: "BIP34 height and nBits agree",
    criteria: "The header has valid Bitcoin proof of work, Bitcoin Core does not know it, and its preserved Bitcoin coinbase names a post-BIP34 height whose expected nBits matches the header.",
    interpretation: "Strict orphan means a Core-absent Bitcoin-PoW block can be placed at a specific Bitcoin height. This is the post-BIP34 path when the child-chain evidence includes the real Bitcoin coinbase: BIP34 gives the claimed height, and the nBits check confirms that height sits in the right Bitcoin difficulty epoch.[^1]",
    references: [
      { id: 1, label: "BIP 34: Block v2, Height in Coinbase", url: "https://bips.dev/34/" },
      { id: 2, label: "BIP 90: Buried Deployments", url: "https://bips.dev/90/" },
    ],
    notes: [
      "BIP34 is the Bitcoin consensus rule that makes miners put the block height in the coinbase transaction. It came into effect during the March 2013 version-2 block transition; Bitcoin Core now treats height 227,931 as the activation point.[^1][^2]",
      "It was introduced to make coinbase transactions unique and to help nodes validate blocks that arrive before their ancestors.[^1]",
      "nBits is the compact difficulty target in the Bitcoin header. Matching it to the claimed height filters out wrong-height or non-Bitcoin parent headers.",
      "Not every post-BIP34 orphan can be strict. If the source does not preserve a trustworthy Bitcoin coinbase height, the monitor falls back to weak classification.",
      "Because both checks agree, the tree can attach the orphan at an exact fork position instead of estimating from timestamp.",
    ],
  },
  weak_btc_orphan: {
    name: "Weak orphan",
    meta: "Timestamp nBits match, no BIP34 height",
    criteria: "The header has valid Bitcoin proof of work and Bitcoin Core does not know it, but the monitor cannot prove an exact BIP34 height for it.",
    interpretation: "Weak orphan means the block is still credible Bitcoin-PoW orphan evidence, but placement has to come from the header timestamp and expected nBits rather than a coinbase-declared height.",
    references: [
      { id: 1, label: "BIP 34: Block v2, Height in Coinbase", url: "https://bips.dev/34/" },
      { id: 2, label: "BIP 90: Buried Deployments", url: "https://bips.dev/90/" },
    ],
    notes: [
      "Before BIP34 activation, Bitcoin coinbases did not reliably commit to the block height, so strict placement is unavailable.[^1][^2]",
      "Weak also covers post-BIP34 evidence from sources that do not preserve a trustworthy Bitcoin coinbase height.",
      "The tree uses the timestamp-selected difficulty epoch for placement, so labels can be approximate.",
    ],
  },
};
// Explainer for the header-time-delta view, shown from the (i) beside its
// title. Sign convention and the era-shaped tail are the two things a reader
// reliably gets wrong.
const DELTA_HELP = {
  metric: {
    name: "Header time delta",
    meta: "canonical.btc_header_time \u2212 stale.btc_header_time",
    body: [
      "Each competition pairs a stale Bitcoin block with the canonical block that beat it at the same height. The delta subtracts the stale block's header timestamp from the winner's, so a positive delta means the STALE block carries the earlier timestamp.",
      "These are self-reported header clocks, not observation times. Consensus only bounds them loosely: a header must beat median-time-past, and may sit up to two hours ahead of network time. That bound, not network latency, is what the tail measures.",
      "The tail is an era artefact. Competitions beyond a couple of hours are almost all pre-2017, when miner clocks were far looser. One height, 153,211, contributes 18 stale blocks spread over weeks, the signature of a pool mining on a stuck tip with a live clock.",
      "A delta can be unavailable when the two timestamps differ by more than a 32-bit second count. Those rows are excluded from every statistic here and reported separately, never counted as zero.",
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

// Blocks-legend expansion of the single "unknown" node kind into the visible
// strict/weak orphan signal. Other kinds render one row keyed by their
// structural fill-<kind>.
const ORPHAN_LEGEND = [
  { label: "strict orphan", swatchClass: "fill-strict_btc_orphan" },
  { label: "weak orphan", swatchClass: "fill-weak_btc_orphan" },
];

// BTC-orphan classes: the Core-gated refinement of kind='unknown'. These drive
// the navigator's `classification=` filter, which is a SEPARATE backend parameter
// from `kinds=` (a client-side highlight). Strict + weak are the navigable signal
// (default on); excluded and pending are muted refinements (off by default). The
// order here is the canonical display order, matching the backend echo.
const CLASSIFICATIONS = ["strict_btc_orphan", "weak_btc_orphan", "excluded", "pending"];
const CLASSIFICATION_DEFAULT = ["strict_btc_orphan", "weak_btc_orphan"];
const CLASSIFICATION_META = {
  strict_btc_orphan: { name: "Strict orphan", count: "strict" },
  weak_btc_orphan: { name: "Weak orphan", count: "weak" },
  excluded: { name: "Excluded", count: "excluded" },
  pending: { name: "Pending", count: "pending" },
};

// Uniform tree-block geometry. Every block is BLOCK_W x BLOCK_H. Layout pitch is
// centralized in tree-layout.js so the stale-branch geometry can be unit-tested.
const BLOCK_W = 110;
const BLOCK_H = 72;

// Stable per-chain swatch palette, reused wherever child chains are colored.
// Today only the selection card consumes it; later child-chain layers (cadence
// rail, intensity gutter, lens) should reuse the same function so a chain keeps
// one color everywhere. Live chains get explicit assignments; any other AuxPoW
// chain hashes deterministically into the qualitative fallback so its color is
// stable across renders rather than depending on draw order. Colors deliberately
// avoid the green/red/amber/purple parent-kind hues so a swatch is never read as
// a kind.
// CHAIN_COLORS, CHAIN_DISPLAY_NAMES, SOURCE_DISPLAY_ORDER, and SOURCE_LIFECYCLE
// are generated from the Rust source registry (src/source_registry); the per-chain
// byline, modal help, and profiles come from data/sources/chain_profiles.json via CHAIN_PROFILES.
// Edit the registry / data/sources/chain_profiles.json, then run `just gen-source-artifacts`.
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
  // Every registered chain has a byline in CHAIN_PROFILES: a "year · cadence"
  // summary for live/parent chains, or a BTC merge-mining operating window for
  // historical chains. The "Registered source" fallback only fires for an
  // unregistered source code with no profile.
  const byline = CHAIN_PROFILES[chain]?.byline;
  if (byline) return byline;
  return "Registered source";
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
  // Active top-level view. Only views registered in view-shell.js are
  // reachable; anything else falls back to the default, so an unknown or
  // not-yet-built `view=` in a shared URL degrades to the tree.
  view: "tree",
  // Delta-view era filter, "<from>-<to>"; empty means the full range.
  era: "",
  // Findings-view open article slug; empty means the feed. Kept in memory
  // across view switches (so returning to findings restores the article) but
  // serialized to the URL only while findings is the active view.
  finding: "",
  treeHeight: "",
  treeTime: "",
  treeLookupContext: "compact",
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
  // Set by every tree-query mutator that changes WHICH window the tree should
  // show without fetching it. Query mutators only rewrite state.query, so a
  // cross-link that retargets the tree while another view is active would
  // otherwise leave a stale (or unbuilt) tree on the next activation.
  treeDirty: false,
  // When each view last received data, keyed by view id. The freshness
  // indicator is shared but the views refresh independently: competitions load
  // once and never auto-refresh, so showing the tree's timestamp beside a
  // distribution loaded hours earlier would materially overstate it.
  updatedAt: {},
  // A layout change that landed while the tree panel was hidden. It could not
  // be painted then (a display:none panel measures 0, so the layout would bake
  // in the fallback geometry), so the tree view pays it back on return.
  treeRepaintPending: false,
  // The whole competition set, loaded once per delta-view activation. Null
  // until first load; the view registry uses that to decide whether to fetch.
  competitions: null,
  // The findings corpus (from the generated module). Null until the findings
  // view first activates; the registry uses that to decide whether to seed.
  findings: null,
  // Findings-local UI state, deliberately not URL-persisted in this slice:
  // category/status filter exclusions and the feed scroll position the
  // article state restores on back.
  findingsUi: { hideCategories: [], hideStatuses: [], feedScroll: 0 },
  selectedHash: null,
  selectedBlock: null,
  // Consolidated navigator: the active target and how it was set. `source`
  // 'navigator' is a LOCKED menu/stepper choice; 'selection' is derived from a
  // direct node click (precedence branch -> stale -> orphan).
  nav: { target: "tip", source: "navigator" },
  // Monotonic token bumped by every user navigation gesture (goTo, stepNav,
  // selectTreeNode, resetTreeToTip). Each async jump captures it and discards its
  // post-await result if a newer gesture has since superseded it, so a slow jump
  // cannot reselect/reanchor a target the user has already moved away from.
  navEpoch: 0,
  // Monotonic token bumped at the START of every loadTree (jump, softRefresh,
  // filter/validation, initial). A load owns the shared tree state (state.tree and
  // state.errors.tree) only while it is the latest invocation; a stale load leaves
  // both for the newer one rather than clobbering its result or its error.
  treeLoadSeq: 0,
  // Navigator targets: each stores the current server-owned item cursor,
  // filtered total, edge flags, and a busy guard. Stale/errorBlock/orphan keep an
  // `anchor` mirror for the existing readout helpers. A target registered in
  // nav-targets.js with no slot here resolves to undefined through
  // `targetState`, which silently discards its payloads and leaves the stepper
  // blank, so the two must be added together.
  stale: { item: null, anchor: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  branch: { item: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  errorBlock: { item: null, anchor: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  orphan: { item: null, anchor: null, total: null, counts: null, hasOlder: false, hasNewer: false, busy: false },
  orphanBranch: { item: null, total: null, hasOlder: false, hasNewer: false, busy: false },
  // True while a pointer is pressed on the tree SVG. The auto-refresh timer skips
  // its re-render while set, so a renderTree() cannot remove the pressed node's <g>
  // (and its click handler) between the press and the resulting click.
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

// Largest-fitting unit, in seconds, for the relative-time label.
const RELATIVE_TIME_UNITS = [
  ["year", 31536000],
  ["month", 2592000],
  ["week", 604800],
  ["day", 86400],
  ["hour", 3600],
  ["minute", 60],
  ["second", 1],
];

// "how long ago" relative to the viewer's clock, e.g. "3 hours ago" or, for a
// header time that is briefly ahead of local time (clock skew), "in 2 minutes".
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
  // data-epoch lets refreshRelativeTimes() re-stamp the label in place against a
  // newer clock without re-rendering (and collapsing) the whole detail drawer.
  return `${iso} <span class="relative-time" data-epoch="${esc(value)}">(${esc(relativeTime(date))})</span>`;
}

// Re-stamp every rendered relative-time label against the current clock so
// "3 hours ago" advances on each refresh. Updates text in place rather than
// re-rendering the drawer, so expanded events and scroll position survive.
function refreshRelativeTimes() {
  for (const el of $all(".relative-time[data-epoch]")) {
    const date = new Date(Number(el.dataset.epoch) * 1000);
    if (Number.isNaN(date.getTime())) continue;
    el.textContent = `(${relativeTime(date)})`;
  }
}

// The labels also tick on their own fixed cadence, independent of the data
// auto-refresh interval, so "how long ago" keeps advancing between tree
// refreshes. 30s is fine granularity for minute/hour/day labels;
// refreshRelativeTimes() is a no-op when no detail is open.
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

// The selected orphan classes, defaulting to the navigable strict+weak signal
// when none are checked (mirrors the backend default).
function selectedClassifications() {
  const checked = $all('input[name="classification"]:checked').map((input) => input.value);
  return checked.length ? checked : CLASSIFICATION_DEFAULT.slice();
}

// The `classification=` query value sent to orphan navigator targets and the
// anchor tree.
function classificationParam() {
  const selected = state.query.classification?.length
    ? state.query.classification
    : CLASSIFICATION_DEFAULT;
  return selected.join(",");
}

// Set-equality for an orphan-class selection (order-independent), so the default
// strict+weak set is not echoed to the URL regardless of toggle order.
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
      treeLookupContext: "compact",
    });
  }
  // readForm runs on every filter change, including the client-side source and
  // parent-kind toggles, so it must PRESERVE the unheighted anchor (the spread
  // above does). Only a genuine explicit lookup supersedes anchor mode.
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
  // The view is only recorded here; view-shell.js validates it against the
  // registry on activation and falls back to the default when it names a view
  // that does not exist yet.
  const view = params.get("view");
  if (view) state.query.view = view;
  const era = params.get("era");
  if (parseEra(era)) state.query.era = era;
  // `finding=` only means anything on the findings view; hydrating it under
  // another view would silently re-open an article the URL never showed.
  // Slug validity is the findings view's job at activation (an unknown slug
  // degrades to the feed there).
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
    state.query.treeLookupContext = "compact";
    state.query.unheightedAnchor = "";
  } else if (params.has("tree_height")) {
    const height = params.get("tree_height");
    if (/^\d+$/.test(height)) {
      state.query.treeHeight = height;
      state.query.treeTime = "";
      state.query.treeLookupContext = "compact";
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
      state.query.treeLookupContext = "compact";
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
    // Catalogued and surveyed sources are not selectable filters, so drop them
    // from deep links rather than pinning a checked, un-clearable source. Codes
    // absent from the public lifecycle map are unsupported and also dropped.
    state.query.sources = params
      .get("sources")
      .split(",")
      .filter(Boolean)
      .filter((code) => SOURCE_LIFECYCLE[code])
      .filter((code) => !["catalogued", "surveyed"].includes(SOURCE_LIFECYCLE[code]))
      .sort();
  }
  if (params.has("unheighted_anchor")) {
    // Anchor mode: `unheighted_anchor` controls the VIEW (the orphan strip);
    // `selected` independently controls the SELECTION. A deep link with both
    // restores the view and opens the block; a bare anchor (the deselected state
    // clearTreeSelection writes) restores the strip with nothing selected, and
    // must NOT auto-select the anchor or a deselected view could not round-trip.
    // reloadAll centers the camera on the anchor even when nothing is selected.
    state.query.unheightedAnchor = params.get("unheighted_anchor");
    state.query.treeHeight = "";
    state.query.treeTime = "";
    state.query.treeLookupContext = "compact";
    state.query.treeWindow = "";
    state.query.treeFrom = "";
    state.query.treeTo = "";
    state.query.treeTargetHeight = "";
    state.nav = { target: "orphan", source: "navigator" };
  }
  // Lower-cased: the API accepts uppercase hex but every payload returns
  // lowercase, and the selection is compared by string equality against tree
  // nodes, competition rows and block detail alike. Keeping the URL's spelling
  // would load the drawer and match nothing else. Anything that is not 64 hex
  // characters is left untouched for the existing validation to reject.
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
