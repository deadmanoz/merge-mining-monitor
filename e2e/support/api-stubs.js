const GENERATED_AT = 1779792000;

function versionPayload(overrides = {}) {
  return {
    schema_version: "v1",
    generated_at: GENERATED_AT,
    version: "0.2.0",
    release_notes: { source: "RELEASE_NOTES.md", release_count: 0, truncated: false, releases: [] },
    ...overrides,
  };
}

function sourcesPayload(sources = []) {
  return {
    schema_version: "v1",
    generated_at: GENERATED_AT,
    sources,
  };
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
      kinds: ["canonical", "stale", "unknown", "near"],
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
      kinds: ["canonical", "stale", "unknown", "near"],
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
  await page.route("**/api/v1/block/**", async (route) => {
    const hash = route.request().url().split("/").at(-1);
    const payload = options.blockPayloads?.[hash] || resolvePayload(options.blockPayload, hash, route);
    await route.fulfill({ json: payload || blockPayload(hash) });
  });
}

module.exports = {
  GENERATED_AT,
  makeNode,
  sourcesPayload,
  stubApi,
  treeEnvelope,
  versionPayload,
};
