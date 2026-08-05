const fs = require("node:fs");
const path = require("node:path");

const GENERATED_AT = 1779792000;

// The served frontend reports env!("CARGO_PKG_VERSION") (cache-busting query on
// every asset URL, plus the /api/v1/version payload). Read the same workspace
// version here so stubbed payloads and version assertions follow a release bump
// instead of pinning a literal that goes stale on the next one.
function readWorkspaceVersion() {
  const manifestPath = path.join(__dirname, "..", "..", "Cargo.toml");
  const manifest = fs.readFileSync(manifestPath, "utf8");
  const section = manifest.split(/^\[workspace\.package\][ \t]*$/m)[1];
  if (section === undefined) {
    throw new Error(`no [workspace.package] section in ${manifestPath}`);
  }
  const version = section.split(/^\[/m)[0].match(/^version\s*=\s*"([^"]+)"/m);
  if (!version) {
    throw new Error(`no workspace.package version in ${manifestPath}`);
  }
  return version[1];
}

const RELEASE_VERSION = readWorkspaceVersion();

// Reaching into a frontend module from a test means importing the exact URL
// boot.js used: a differing cache-busting query is a separate module record
// with its own state, so the test would inspect a second, inert copy.
function moduleUrl(name) {
  return `/js/${name}?v=${RELEASE_VERSION}`;
}

function versionPayload(overrides = {}) {
  return {
    schema_version: "v1",
    generated_at: GENERATED_AT,
    version: RELEASE_VERSION,
    release_notes: { source: "RELEASE_NOTES.md", release_count: 0, truncated: false, releases: [] },
    ...overrides,
  };
}

const LIVE_SOURCE_BASE = {
  kind: "auxpow",
  instance: null,
  created_at: 1_700_000_000,
  last_seen_at: GENERATED_AT - 60,
  status: "fresh",
  sync: {
    mode: "live",
    state: "live",
    progress_height: 700000,
    progress_updated_at: GENERATED_AT - 60,
    target_height: 700000,
    latest_evidence_at: GENERATED_AT - 60,
    error_code: null,
    error_height: null,
  },
  counts: {
    events: 0,
    near: 0,
    unknown: 0,
    canonical: 0,
    stale: 0,
    error_block: 0,
    strict_orphan: 0,
    weak_orphan: 0,
  },
};

function liveSourcesPayload() {
  return sourcesPayload([
    { ...LIVE_SOURCE_BASE, id: 1, code: "auxpow:namecoin", chain: "namecoin" },
    { ...LIVE_SOURCE_BASE, id: 2, code: "auxpow:rsk", chain: "rsk" },
  ]);
}

function sourcesPayload(sources = []) {
  return {
    schema_version: "v1",
    generated_at: GENERATED_AT,
    sources,
  };
}

// One competition row in the /api/v1/competitions shape. Defaults give a
// small in-window delta; override `header_time_delta_s` with null to exercise
// the unavailable-delta path.
function makeCompetition(hash, height, deltaSeconds, overrides = {}) {
  return {
    btc_height: height,
    stale_hash: hash,
    header_time_delta_s: deltaSeconds,
    stale_header_time: 1_700_000_000 + height,
    stale_bitcoin_miner_pool: { id: 7, slug: "antpool", name: "AntPool", known: true },
    canonical_bitcoin_miner_pool: { id: 8, slug: "f2pool", name: "F2Pool", known: true },
    sources: ["auxpow:namecoin"],
    ...overrides,
  };
}

// The `competition` block a /api/v1/block payload carries, which is what the
// drawer's Competition section renders. Shaped for the drawer rather than for
// /api/v1/competitions: it names the winning block, which that endpoint omits.
function blockCompetition(hash, height, deltaSeconds, overrides = {}) {
  return {
    btc_height: height,
    stale_hash: hash,
    canonical_hash: "f".repeat(64),
    stale_bitcoin_miner_pool: { id: 7, slug: "antpool", name: "AntPool", known: true },
    canonical_bitcoin_miner_pool: { id: 8, slug: "f2pool", name: "F2Pool", known: true },
    header_time_delta_s: deltaSeconds,
    propagation_delta_s: null,
    ...overrides,
  };
}

function competitionsPayload(competitions = []) {
  return { schema_version: "v1", generated_at: GENERATED_AT, competitions };
}

function makeNode(hash, height, prevHash, kind = "canonical", overrides = {}) {
  return {
    id: height,
    hash,
    height,
    kind,
    btc_orphan_class: null,
    prev_id: prevHash ? height - 1 : null,
    prev_hash: prevHash,
    bitcoin_miner_pool: { id: null, slug: null, name: "Unknown", known: false },
    source_summary: {
      sources: [],
      distinct_sources: 0,
      auxpow_chain_count: 0,
      live_observed: false,
      pow_validates_btc_target: true,
    },
    branch: null,
    proof_state: {
      has_live_observation: false,
      has_tip_ref: false,
      has_auxpow_evidence: false,
    },
    competition: null,
    child_chain_evidence: [],
    ...overrides,
  };
}

function treeEnvelope(query = new URLSearchParams(), options = {}) {
  const height = Number(query.get("at_height") || query.get("from_height") || 700000);
  const isTime = query.has("at_time");
  const isHeight = query.has("at_height");
  const nodes = options.nodes || [
    makeNode("a".repeat(64), height, "b".repeat(64), "canonical", {
      id: 1,
      prev_id: null,
    }),
  ];
  return {
    schema_version: "v1",
    generated_at: GENERATED_AT,
    query: {
      from_height: query.has("from_height") ? Number(query.get("from_height")) : null,
      to_height: query.has("to_height") ? Number(query.get("to_height")) : null,
      at_height: isHeight ? height : null,
      at_time: query.get("at_time"),
      window_mode: isTime ? "time" : isHeight ? "height" : "explicit",
      context: query.get("context") || "exact",
      kinds: ["canonical", "stale", "error_block", "unknown", "near"],
      classification: ["strict_btc_orphan", "weak_btc_orphan"],
      sources: [],
      include_near: false,
      min_sources: 1,
      include_unheighted: false,
      ...options.query,
    },
    window: {
      btc_height_min: height,
      btc_height_max: height,
      tip_height: null,
      defaulted_to_tip: false,
      empty_reason: null,
      truncated_before: false,
      truncated_after: false,
      hidden_linear_block_count: 0,
      ...options.window,
    },
    nodes,
    edges: options.edges || [],
    branches: options.branches || [],
    legend: options.legend || {
      kinds: ["canonical", "stale", "error_block", "unknown", "near"],
      edge_kinds: ["canonical", "stale_entry", "stale", "hidden"],
    },
  };
}

function navigatorPayload(target, overrides = {}) {
  return {
    schema_version: "v1",
    generated_at: GENERATED_AT,
    query: { target, mode: "latest", cursor: null, direction: null, anchor_hash: null, classification: [], limit: 1 },
    target,
    items: [],
    total: 0,
    facets: {},
    next_cursor: null,
    prev_cursor: null,
    ...overrides,
  };
}

function blockPayload(hash, overrides = {}) {
  const { id, prev_id, prev_hash, ...block } = makeNode(hash, 700000, null);
  return {
    schema_version: "v1",
    generated_at: GENERATED_AT,
    block,
    ...overrides,
  };
}

function resolvePayload(payload, ...args) {
  return typeof payload === "function" ? payload(...args) : payload;
}

// One findings-corpus entry with every render-critical field present, so a
// spec only overrides what it is exercising. Mirrors the generated shape
// (findings_registry.rs), not the authoring shape: newest-first ordering is
// the caller's job, like the generator's.
function makeFinding(overrides = {}) {
  return {
    slug: "test-finding",
    title: "A test finding",
    category: "dataset-note",
    status: "concluded",
    observed_at: "2026-06-10",
    published_at: "2026-07-29",
    affected_sources: ["auxpow:namecoin"],
    summary: "Summary prose.[^1]",
    body: "Body prose.[^1]\n\nSecond paragraph with `code`.",
    anchors: [{ kind: "btc-height", value: "700000" }],
    references: [{ id: 1, label: "Example", url: "https://example.com" }],
    ...overrides,
  };
}

// Serve a spec-controlled corpus as the generated findings module. The real
// committed module is a static import in the app graph, so this must be
// routed before page.goto; the exact ?v= URL matters (a different URL would
// be a second, inert module record - see moduleUrl).
async function stubFindings(page, findings) {
  await page.route(`**/js/findings.generated.js?v=${RELEASE_VERSION}`, async (route) => {
    await route.fulfill({
      contentType: "text/javascript",
      body: `export const FINDINGS = ${JSON.stringify(findings, null, 2)};\n`,
    });
  });
}

async function stubApi(page, treeRequests = [], options = {}) {
  await page.route("**/api/v1/version", async (route) => {
    await route.fulfill({
      json: resolvePayload(options.versionPayload, route) || versionPayload(),
    });
  });
  await page.route("**/api/v1/tree**", async (route) => {
    const url = new URL(route.request().url());
    treeRequests.push(url);
    await route.fulfill({
      json: resolvePayload(options.treePayload, url.searchParams, url) || treeEnvelope(url.searchParams),
    });
  });
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({
      json: resolvePayload(options.sourcesPayload, route) || sourcesPayload(),
    });
  });
  await page.route("**/api/v1/navigator/**", async (route) => {
    const url = new URL(route.request().url());
    options.navigatorRequests?.push(url);
    const target = url.pathname.split("/").at(-1);
    const payload = typeof options.navigator === "function"
      ? options.navigator(url, target)
      : options.navigator?.[target];
    await route.fulfill({ json: payload || navigatorPayload(target) });
  });
  await page.route("**/api/v1/competitions", async (route) => {
    await route.fulfill({
      json: resolvePayload(options.competitionsPayload, route) || competitionsPayload(),
    });
  });
  await page.route("**/api/v1/block/**", async (route) => {
    const hash = route.request().url().split("/").at(-1);
    const payload = options.blockPayloads?.[hash] || resolvePayload(options.blockPayload, hash, route);
    await route.fulfill({ json: payload || blockPayload(hash) });
  });
}

module.exports = {
  GENERATED_AT,
  RELEASE_VERSION,
  blockCompetition,
  blockPayload,
  competitionsPayload,
  liveSourcesPayload,
  makeCompetition,
  makeFinding,
  makeNode,
  moduleUrl,
  stubFindings,
  sourcesPayload,
  stubApi,
  treeEnvelope,
  versionPayload,
};
