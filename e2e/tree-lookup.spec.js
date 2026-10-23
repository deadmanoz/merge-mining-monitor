const { expect, test } = require("@playwright/test");
const { makeNode, navigatorPayload, stubApi, treeEnvelope } = require("./support/api-stubs");

test("height shared link requests compact tree height automatically", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/?tree_height=700000");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator('input[name="treeMode"]')).toHaveCount(0);
  await expect(page.locator('input[name="treeContextCompact"]')).toHaveCount(0);
  await expect(page.locator("#filters fieldset legend").first()).toHaveText("Tree");
  await expect(page.locator('input[name="treeHeight"]')).toHaveValue("700000");
  await expect(page.locator('input[name="treeTime"]')).toHaveAttribute("type", "datetime-local");
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  const query = treeRequests.at(-1).searchParams;
  expect(query.get("at_height")).toBe("700000");
  expect(query.get("context")).toBe("compact");
  expect(query.has("from_height")).toBe(false);
  expect(query.has("to_height")).toBe(false);
});

test("tree controls show canonical, stale, invalid-block, and orphan signal filters", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  const filters = page.locator("#filters");
  await expect(filters.locator("legend", { hasText: "Classification" })).toBeVisible();
  await expect(filters.locator('input[name="kind"]')).toHaveCount(3);
  await expect(filters.locator('input[name="classification"]')).toHaveCount(2);
  await expect(filters.getByText("Canonical", { exact: true })).toBeVisible();
  await expect(filters.getByText("Stale", { exact: true })).toBeVisible();
  await expect(filters.getByText("Invalid block", { exact: true })).toBeVisible();
  await expect(filters.getByText("Strict orphan", { exact: true })).toBeVisible();
  await expect(filters.getByText("Weak orphan", { exact: true })).toBeVisible();
  await expect(filters.getByText("Unknown", { exact: true })).toHaveCount(0);
  await expect(filters.getByText("Near", { exact: true })).toHaveCount(0);
  await expect(filters.getByText("Excluded", { exact: true })).toHaveCount(0);
  await expect(filters.getByText("Pending", { exact: true })).toHaveCount(0);

  const legend = page.locator("#tree-legend-body");
  await expect(legend.getByText("canonical", { exact: true })).toBeVisible();
  await expect(legend.getByText("stale", { exact: true })).toBeVisible();
  await expect(legend.getByText("invalid block", { exact: true })).toBeVisible();
  await expect(legend.getByText("strict orphan", { exact: true })).toBeVisible();
  await expect(legend.getByText("weak orphan", { exact: true })).toBeVisible();
  await expect(legend.getByText("near", { exact: true })).toHaveCount(0);
  await expect(legend.getByText("excluded", { exact: true })).toHaveCount(0);
  await expect(legend.getByText("pending", { exact: true })).toHaveCount(0);
});

test("height shared link drops stale tree_context and removed classification", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/?tree_height=700000&tree_context=compact&classification=pending");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator('input[name="treeHeight"]')).toHaveValue("700000");
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  const query = treeRequests.at(-1).searchParams;
  expect(query.get("at_height")).toBe("700000");
  expect(query.get("context")).toBe("compact");
  expect(query.get("classification")).toBe("strict_btc_orphan,weak_btc_orphan");
  expect(query.has("from_height")).toBe(false);
  expect(query.has("to_height")).toBe(false);
  expect(new URL(page.url()).searchParams.has("tree_context")).toBe(false);
  expect(new URL(page.url()).searchParams.has("classification")).toBe(false);
});

test("datetime shared link requests compact tree time automatically", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/?tree_time=2026-05-10T12%3A30%3A00Z");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator('input[name="treeMode"]')).toHaveCount(0);
  await expect(page.locator('input[name="treeTime"]')).toHaveValue(/^2026-05-10T12:30(:00)?$/);
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  const query = treeRequests.at(-1).searchParams;
  expect(query.get("at_time")).toBe("2026-05-10T12:30:00Z");
  expect(query.get("context")).toBe("compact");
  expect(query.has("from_height")).toBe(false);
  expect(query.has("to_height")).toBe(false);
});

test("typing height and pressing enter requests that height", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);
  const requestCount = treeRequests.length;

  await page.locator('input[name="treeHeight"]').fill("900000");
  await page.locator('input[name="treeHeight"]').press("Enter");
  await expect.poll(() => treeRequests.length).toBe(requestCount + 1);
  await page.waitForTimeout(150);
  expect(treeRequests.length).toBe(requestCount + 1);

  const query = treeRequests.at(-1).searchParams;
  expect(query.get("at_height")).toBe("900000");
  expect(query.get("context")).toBe("compact");
  expect(query.has("from_height")).toBe(false);
  expect(query.has("to_height")).toBe(false);
  expect(query.has("at_time")).toBe(false);
  expect(new URL(page.url()).searchParams.get("tree_height")).toBe("900000");
});

test("typing datetime and pressing enter requests that UTC timestamp", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);
  const requestCount = treeRequests.length;

  await page.locator('input[name="treeTime"]').fill("2026-05-10T12:30");
  await page.locator('input[name="treeTime"]').press("Enter");
  await expect.poll(() => treeRequests.length).toBe(requestCount + 1);
  await page.waitForTimeout(150);
  expect(treeRequests.length).toBe(requestCount + 1);

  const query = treeRequests.at(-1).searchParams;
  expect(query.get("at_time")).toBe("2026-05-10T12:30:00Z");
  expect(query.get("context")).toBe("compact");
  expect(query.has("at_height")).toBe(false);
  expect(new URL(page.url()).searchParams.get("tree_time")).toBe("2026-05-10T12:30:00Z");
});

test("datetime field uses a native picker input", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);
  await expect(page.locator('input[name="treeTime"]')).toHaveAttribute("type", "datetime-local");
  await expect(page.locator('input[name="treeTime"]')).toHaveAttribute("step", "1");
});

test("range query parameters are ignored by the simplified controls", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/?tree_from=10&tree_to=12&tree_context=compact");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator('input[name="treeMode"]')).toHaveCount(0);
  await expect(page.locator('input[name="treeHeight"]')).toHaveValue("");
  await expect(page.locator('input[name="treeTime"]')).toHaveValue("");
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  const query = treeRequests.at(-1).searchParams;
  expect(query.has("from_height")).toBe(false);
  expect(query.has("to_height")).toBe(false);
  expect(query.has("context")).toBe(false);
  expect(query.has("at_height")).toBe(false);
  expect(query.has("at_time")).toBe(false);
  expect(new URL(page.url()).searchParams.has("tree_from")).toBe(false);
});

test("generated tree windows are requested from server-owned navigation links", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/?tree_window=generated&tree_from=10&tree_to=12&tree_height=11");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator('input[name="treeHeight"]')).toHaveValue("");
  await expect(page.locator('input[name="treeTime"]')).toHaveValue("");
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  const query = treeRequests.at(-1).searchParams;
  expect(query.get("from_height")).toBe("10");
  expect(query.get("to_height")).toBe("12");
  expect(query.has("context")).toBe(false);
  expect(query.has("at_height")).toBe(false);
  expect(query.has("at_time")).toBe(false);
  const url = new URL(page.url());
  expect(url.searchParams.get("tree_window")).toBe("generated");
  expect(url.searchParams.get("tree_from")).toBe("10");
  expect(url.searchParams.get("tree_to")).toBe("12");
  expect(url.searchParams.get("tree_height")).toBe("11");
});

test("Latest stale uses unified navigator endpoint", async ({ page }) => {
  const treeRequests = [];
  const navigatorRequests = [];
  const staleHash = "c".repeat(64);
  const olderHash = "b".repeat(64);
  const winningHash = "d".repeat(64);
  const staleHeight = 900000;
  const olderHeight = 899000;
  const staleRow = (hash, height, index) => ({
    id: `stale-${hash}`,
    kind: "stale",
    primary_hash: hash,
    label: `Stale #${height}`,
    position: { axis: "height", min: height, max: height },
    index,
    cursor: `opaque-stale-cursor-${height}`,
    branch: null,
    orphan: null,
    view: {
      mode: "tree_window",
      target_height: height,
      tree_from: height - 16,
      tree_to: height + 16,
      select_hash: hash,
      center_hash: hash,
    },
    view_error: null,
  });
  const staleEnvelope = (row, mode) => navigatorPayload("stale", {
    query: { target: "stale", mode, cursor: null, direction: null, anchor_hash: null, classification: [], limit: 1 },
    items: [row],
    total: 2345,
    next_cursor: "opaque-stale-cursor-next",
  });
  await stubApi(page, treeRequests, {
    navigatorRequests,
    navigator: (url) => {
      if (url.searchParams.get("direction") === "older") {
        return staleEnvelope(staleRow(olderHash, olderHeight, 2), "page");
      }
      const anchor = url.searchParams.get("anchor_hash");
      if (anchor === olderHash) {
        return staleEnvelope(staleRow(olderHash, olderHeight, 2), "anchor");
      }
      return staleEnvelope(staleRow(staleHash, staleHeight, 1), anchor ? "anchor" : "latest");
    },
    blockPayloads: {
      [staleHash]: {
        schema_version: "v1",
        generated_at: 1779792000,
        block: {
          hash: staleHash,
          height: staleHeight,
          kind: "stale",
          btc_orphan_class: null,
          header: { time: 1779792000 },
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
        },
        competition: {
          btc_height: staleHeight,
          stale_hash: staleHash,
          canonical_hash: winningHash,
          stale_bitcoin_miner_pool: { id: null, slug: null, name: "Unknown", known: false },
          canonical_bitcoin_miner_pool: { id: null, slug: null, name: "Unknown", known: false },
          header_time_delta_s: 0,
          propagation_delta_s: null,
        },
        stale_branch: null,
      },
      [olderHash]: {
        schema_version: "v1",
        generated_at: 1779792000,
        block: {
          hash: olderHash,
          height: olderHeight,
          kind: "stale",
          btc_orphan_class: null,
          header: { time: 1779792000 },
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
        },
        competition: {
          btc_height: olderHeight,
          stale_hash: olderHash,
          canonical_hash: winningHash,
          stale_bitcoin_miner_pool: { id: null, slug: null, name: "Unknown", known: false },
          canonical_bitcoin_miner_pool: { id: null, slug: null, name: "Unknown", known: false },
          header_time_delta_s: 0,
          propagation_delta_s: null,
        },
        stale_branch: null,
      },
    },
  });

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  await page.locator("#nav-goto").selectOption("stale");

  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/stale")
      && url.searchParams.get("limit") === "1"
  ))).toBe(true);
  await expect.poll(() => treeRequests.some((url) => (
    url.searchParams.get("from_height") === "899984"
      && url.searchParams.get("to_height") === "900016"
  ))).toBe(true);
  await expect(page.locator("#nav-readout")).toContainText("#900,000 · 1 of 2,345");

  await page.locator("#nav-older").click();
  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/stale")
      && url.searchParams.get("direction") === "older"
  ))).toBe(true);
  await expect(page.locator("#nav-readout")).toContainText("#899,000 · 2 of 2,345");
});

test("stale node with child-inferred miner labels the pool instead of unknown", async ({ page }) => {
  await stubApi(page, []);
  await page.route("**/api/v1/tree?**", async (route) => {
    const url = new URL(route.request().url());
    const fixture = treeEnvelope(url.searchParams);
    const node = fixture.nodes[0];
    node.kind = "stale";
    node.bitcoin_miner_pool = { id: null, slug: null, name: "Unknown", known: false };
    node.display_miner_pool = { id: 8, slug: "f2pool", name: "F2Pool", known: true };
    node.display_miner_basis = "child_inferred";
    await route.fulfill({ json: fixture });
  });

  await page.goto("/?tree_height=700000");

  await expect(page.locator(".tree-block-pool", { hasText: "F2Pool" })).toBeVisible();
  await expect(page.locator(".tree-block-pool", { hasText: "unknown miner" })).toHaveCount(0);
});

test("Latest invalid block uses the unified navigator endpoint and steps", async ({ page }) => {
  const treeRequests = [];
  const navigatorRequests = [];
  const invalidHash = "e".repeat(64);
  const olderHash = "f".repeat(64);
  const invalidHeight = 946213;
  const olderHeight = 717696;

  const invalidRow = (hash, height) => ({
    id: `invalid-block-${hash}`,
    kind: "invalid-block",
    primary_hash: hash,
    label: `Invalid block #${height}`,
    position: { axis: "height", min: height, max: height },
    index: hash === olderHash ? 2 : 1,
    cursor: `opaque-invalid-cursor-${height}`,
    branch: null,
    orphan: null,
    view: {
      mode: "tree_window",
      target_height: height,
      tree_from: height - 16,
      tree_to: height + 16,
      select_hash: hash,
      center_hash: hash,
    },
    view_error: null,
  });

  const invalidEnvelope = (rows, mode) => ({
    schema_version: "v1",
    generated_at: 1779792000,
    query: {
      target: "invalid-block",
      mode,
      cursor: null,
      direction: null,
      anchor_hash: null,
      classification: [],
      limit: 1,
    },
    target: "invalid-block",
    items: rows,
    total: 33,
    facets: {},
    next_cursor: "opaque-invalid-cursor-next",
    prev_cursor: null,
  });

  const invalidBlockPayload = (hash, height) => ({
    schema_version: "v1",
    generated_at: 1779792000,
    block: {
      hash,
      height,
      kind: "invalid_block",
      btc_orphan_class: null,
      invalid_block_reason: "time_below_mtp",
      header: { time: 1779792000 },
      bitcoin_miner_pool: { id: null, slug: null, name: "Unknown", known: false },
      source_summary: {
        sources: [],
        distinct_sources: 0,
        auxpow_chain_count: 0,
        live_observed: false,
        pow_validates_btc_target: true,
      },
      competition: null,
      child_chain_evidence: [],
    },
    competition: null,
    stale_branch: null,
  });

  await stubApi(page, treeRequests, {
    treePayload: (query) => {
      const from = Number(query.get("from_height") || 0);
      const [hash, height] = from === olderHeight - 16
        ? [olderHash, olderHeight]
        : [invalidHash, invalidHeight];
      return treeEnvelope(query, {
        nodes: [makeNode(hash, height, null, "invalid_block", { id: 1, prev_id: null })],
        edges: [],
        legend: {
          kinds: ["canonical", "stale", "invalid_block", "unknown"],
          edge_kinds: ["canonical", "stale_entry", "stale", "hidden"],
        },
      });
    },
    navigatorRequests,
    navigator: (url) => {
      if (url.searchParams.get("direction") === "older") {
        return invalidEnvelope([invalidRow(olderHash, olderHeight)], "page");
      }
      const anchor = url.searchParams.get("anchor_hash");
      if (anchor) {
        const row = anchor === olderHash
          ? invalidRow(olderHash, olderHeight)
          : invalidRow(invalidHash, invalidHeight);
        return invalidEnvelope([row], "anchor");
      }
      return invalidEnvelope([invalidRow(invalidHash, invalidHeight)], "latest");
    },
    blockPayloads: {
      [invalidHash]: invalidBlockPayload(invalidHash, invalidHeight),
      [olderHash]: invalidBlockPayload(olderHash, olderHeight),
    },
  });

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  await expect(
    page.locator('#nav-goto option[value="invalidBlock"]'),
  ).toHaveText("Latest invalid block");

  await page.locator("#nav-goto").selectOption("invalidBlock");

  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/invalid-block")
      && url.searchParams.get("limit") === "1"
  ))).toBe(true);
  expect(
    navigatorRequests
      .filter((url) => url.pathname.endsWith("/api/v1/navigator/invalid-block"))
      .every((url) => url.searchParams.get("classification") === null),
  ).toBe(true);

  await expect.poll(() => treeRequests.some((url) => (
    url.searchParams.get("from_height") === String(invalidHeight - 16)
      && url.searchParams.get("to_height") === String(invalidHeight + 16)
  ))).toBe(true);

  await expect(
    page.locator(`g.tree-node[aria-label*="invalid_block ${invalidHeight}"]`),
  ).toHaveCount(1);
  await expect(page).toHaveURL(new RegExp(`selected=${invalidHash}`));
  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/invalid-block")
      && url.searchParams.get("anchor_hash") === invalidHash
  ))).toBe(true);

  await expect(page.locator("#nav-readout")).toContainText("#946,213 · 1 of 33");

  await page.locator("#nav-older").click();
  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/invalid-block")
      && url.searchParams.get("direction") === "older"
  ))).toBe(true);
  await expect.poll(() => treeRequests.some((url) => (
    url.searchParams.get("from_height") === String(olderHeight - 16)
      && url.searchParams.get("to_height") === String(olderHeight + 16)
  ))).toBe(true);
  await expect(page).toHaveURL(new RegExp(`selected=${olderHash}`));
  await expect(
    page.locator(`g.tree-node[aria-label*="invalid_block ${olderHeight}"]`),
  ).toHaveCount(1);
  await expect(page.locator("#nav-readout")).toContainText("#717,696 · 2 of 33");
  await expect(page.locator("#nav-readout")).not.toContainText("#946,213");
});

test("a shared link to an invalid block adopts that navigator target", async ({ page }) => {
  const treeRequests = [];
  const navigatorRequests = [];
  const invalidHash = "e".repeat(64);
  const olderHash = "f".repeat(64);
  const invalidHeight = 946213;
  const olderHeight = 717696;

  const invalidRow = (hash, height) => ({
    id: `invalid-block-${hash}`,
    kind: "invalid-block",
    primary_hash: hash,
    label: `Invalid block #${height}`,
    position: { axis: "height", min: height, max: height },
    index: hash === olderHash ? 2 : 1,
    cursor: `opaque-invalid-cursor-${height}`,
    branch: null,
    orphan: null,
    view: {
      mode: "tree_window",
      target_height: height,
      tree_from: height - 16,
      tree_to: height + 16,
      select_hash: hash,
      center_hash: hash,
    },
    view_error: null,
  });

  const envelope = (rows, mode) => ({
    schema_version: "v1",
    generated_at: 1779792000,
    query: {
      target: "invalid-block",
      mode,
      cursor: null,
      direction: null,
      anchor_hash: null,
      classification: [],
      limit: 1,
    },
    target: "invalid-block",
    items: rows,
    total: 33,
    facets: {},
    next_cursor: "opaque-invalid-cursor-next",
    prev_cursor: null,
  });

  const blockPayload = (hash, height) => ({
    schema_version: "v1",
    generated_at: 1779792000,
    block: {
      hash,
      height,
      kind: "invalid_block",
      btc_orphan_class: null,
      invalid_block_reason: "time_below_mtp",
      header: { time: 1779792000 },
      bitcoin_miner_pool: { id: null, slug: null, name: "Unknown", known: false },
      source_summary: {
        sources: [],
        distinct_sources: 0,
        auxpow_chain_count: 0,
        live_observed: false,
        pow_validates_btc_target: true,
      },
      competition: null,
      child_chain_evidence: [],
    },
    competition: null,
    stale_branch: null,
  });

  await stubApi(page, treeRequests, {
    treePayload: (query) => {
      const from = Number(query.get("from_height") || 0);
      const [hash, height] = from === olderHeight - 16
        ? [olderHash, olderHeight]
        : [invalidHash, invalidHeight];
      return treeEnvelope(query, {
        nodes: [makeNode(hash, height, null, "invalid_block", { id: 1, prev_id: null })],
        edges: [],
        legend: {
          kinds: ["canonical", "stale", "invalid_block", "unknown"],
          edge_kinds: ["canonical", "stale_entry", "stale", "hidden"],
        },
      });
    },
    navigatorRequests,
    navigator: (url) => {
      if (url.searchParams.get("direction") === "older") {
        return envelope([invalidRow(olderHash, olderHeight)], "page");
      }
      const anchor = url.searchParams.get("anchor_hash");
      if (anchor) {
        return envelope(
          [anchor === olderHash ? invalidRow(olderHash, olderHeight) : invalidRow(invalidHash, invalidHeight)],
          "anchor",
        );
      }
      return envelope([invalidRow(invalidHash, invalidHeight)], "latest");
    },
    blockPayloads: {
      [invalidHash]: blockPayload(invalidHash, invalidHeight),
      [olderHash]: blockPayload(olderHash, olderHeight),
    },
  });

  await page.goto(`/?tree_height=${invalidHeight}&selected=${invalidHash}`);
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();

  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/invalid-block")
      && url.searchParams.get("anchor_hash") === invalidHash
  ))).toBe(true);

  await expect(page.locator("#nav-goto option[value='']")).toHaveText("Invalid blocks");
  await expect(page.locator("#nav-readout")).toContainText("#946,213 · 1 of 33");

  await expect(page.locator("#nav-older")).toBeEnabled();
  await page.locator("#nav-older").click();
  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/invalid-block")
      && url.searchParams.get("direction") === "older"
  ))).toBe(true);
  await expect(page.locator("#nav-readout")).toContainText("#717,696 · 2 of 33");
});

test("Latest stale branch readout includes depth and 1 of N", async ({ page }) => {
  const navigatorRequests = [];
  const hash = "a".repeat(64);
  await stubApi(page, [], {
    navigatorRequests,
    navigator: {
      "stale-branch": navigatorPayload("stale-branch", {
        items: [{
          id: `stale-700005-${hash}`,
          kind: "stale-branch",
          primary_hash: hash,
          label: "Stale branch #700005-#700006",
          position: { axis: "height", min: 700005, max: 700006 },
          index: 1,
          cursor: "opaque-stale-branch-cursor",
          branch: { branch_id: `stale-700005-${hash}`, root_hash: hash, tip_hashes: ["1".repeat(64)], depth: 2 },
          orphan: null,
          view: {
            mode: "tree_window",
            target_height: 700005,
            tree_from: 699989,
            tree_to: 700021,
            select_hash: hash,
            center_hash: hash,
          },
          view_error: null,
        }],
        total: 14,
        next_cursor: "opaque-stale-branch-cursor",
      }),
    },
  });
  await page.goto("/");
  await page.locator("#nav-goto").selectOption("branch");
  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/stale-branch")
  ))).toBe(true);
  await expect(page.locator("#nav-readout")).toContainText("#700,005-700,006 · depth 2 · 1 of 14");
});

test("Latest orphan readout includes n of N and class counts", async ({ page }) => {
  const navigatorRequests = [];
  const hash = "2".repeat(64);
  await stubApi(page, [], {
    navigatorRequests,
    navigator: {
      orphan: navigatorPayload("orphan", {
        items: [{
          id: `orphan-${hash}`,
          kind: "orphan",
          primary_hash: hash,
          label: "Orphan 1435968000",
          position: { axis: "time", min: 1435968000, max: 1435968000 },
          index: 1,
          cursor: "opaque-orphan-cursor",
          branch: null,
          orphan: { btc_orphan_class: "strict_btc_orphan" },
          view: { mode: "unheighted_anchor", anchor_hash: hash, select_hash: hash, center_hash: hash },
          view_error: null,
        }],
        total: 30,
        facets: { orphan_classes: { strict: 16, weak: 14, excluded: 0, pending: 0 } },
        next_cursor: "opaque-orphan-cursor",
      }),
    },
  });
  await page.goto("/");
  await page.locator("#nav-goto").selectOption("orphan");
  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/orphan")
  ))).toBe(true);
  await expect(page.locator("#nav-readout")).toContainText("2015-07-04 · 1 of 30 · strict 16 · weak 14");
});

test("Latest orphan branch readout includes depth and 1 of N", async ({ page }) => {
  const navigatorRequests = [];
  const hash = "3".repeat(64);
  await stubApi(page, [], {
    navigatorRequests,
    navigator: {
      "orphan-branch": navigatorPayload("orphan-branch", {
        items: [{
          id: `orphan-${hash}`,
          kind: "orphan-branch",
          primary_hash: hash,
          label: "Orphan branch 1435968000",
          position: { axis: "time", min: 1435968000, max: 1435968000 },
          index: 1,
          cursor: "opaque-orphan-branch-cursor",
          branch: { branch_id: `orphan-${hash}`, root_hash: hash, tip_hashes: ["4".repeat(64)], depth: 2 },
          orphan: null,
          view: { mode: "unheighted_anchor", anchor_hash: hash, select_hash: hash, center_hash: hash },
          view_error: null,
        }],
        total: 6,
        next_cursor: "opaque-orphan-branch-cursor",
      }),
    },
  });
  await page.goto("/");
  await page.locator("#nav-goto").selectOption("orphanBranch");
  await expect.poll(() => navigatorRequests.some((url) => (
    url.pathname.endsWith("/api/v1/navigator/orphan-branch")
  ))).toBe(true);
  await expect(page.locator("#nav-readout")).toContainText("2015-07-04 · depth 2 · 1 of 6");
});
