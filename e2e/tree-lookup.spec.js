const { expect, test } = require("@playwright/test");
const { stubApi, treeEnvelope } = require("./support/api-stubs");

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

test("tree controls show only canonical stale and orphan signal filters", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");

  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect.poll(() => treeRequests.length).toBeGreaterThan(0);

  const filters = page.locator("#filters");
  await expect(filters.locator("legend", { hasText: "Classification" })).toBeVisible();
  await expect(filters.locator('input[name="kind"]')).toHaveCount(2);
  await expect(filters.locator('input[name="classification"]')).toHaveCount(2);
  await expect(filters.getByText("Canonical", { exact: true })).toBeVisible();
  await expect(filters.getByText("Stale", { exact: true })).toBeVisible();
  await expect(filters.getByText("Strict orphan", { exact: true })).toBeVisible();
  await expect(filters.getByText("Weak orphan", { exact: true })).toBeVisible();
  await expect(filters.getByText("Unknown", { exact: true })).toHaveCount(0);
  await expect(filters.getByText("Near", { exact: true })).toHaveCount(0);
  await expect(filters.getByText("Excluded", { exact: true })).toHaveCount(0);
  await expect(filters.getByText("Pending", { exact: true })).toHaveCount(0);

  const legend = page.locator("#tree-legend-body");
  await expect(legend.getByText("canonical", { exact: true })).toBeVisible();
  await expect(legend.getByText("stale", { exact: true })).toBeVisible();
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
  const winningHash = "d".repeat(64);
  const staleHeight = 900000;
  const staleRow = {
    id: `stale-${staleHash}`,
    kind: "stale",
    primary_hash: staleHash,
    label: `Stale #${staleHeight}`,
    position: { axis: "height", min: staleHeight, max: staleHeight },
    cursor: "opaque-stale-cursor",
    branch: null,
    orphan: null,
    view: {
      mode: "tree_window",
      target_height: staleHeight,
      tree_from: 899984,
      tree_to: 900016,
      select_hash: staleHash,
      center_hash: staleHash,
    },
    view_error: null,
  };
  await stubApi(page, treeRequests, {
    navigatorRequests,
    navigator: {
      stale: {
        schema_version: "v1",
        generated_at: 1779792000,
        query: { target: "stale", mode: "latest", cursor: null, direction: null, anchor_hash: null, classification: [], limit: 1 },
        target: "stale",
        items: [staleRow],
        total: 2345,
        facets: {},
        next_cursor: "opaque-stale-cursor",
        prev_cursor: null,
      },
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
  await expect(page.locator("#nav-readout")).toContainText("#900,000");
  await expect(page.locator("#nav-readout")).toContainText("2,345 total");
});

test("stale node with child-inferred miner labels the pool instead of unknown", async ({ page }) => {
  await stubApi(page, []);
  // Override the tree route (LIFO: this handler wins) with a single stale node
  // whose Bitcoin coinbase miner is unknown but whose display miner is the
  // RSK-child-inferred F2Pool: the label must read the pool, not "unknown miner".
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
