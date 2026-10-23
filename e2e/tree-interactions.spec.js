const { expect, test } = require("@playwright/test");
const {
  GENERATED_AT,
  makeNode,
  stubApi: stubSharedApi,
  treeEnvelope,
} = require("./support/api-stubs");

const TIP_HEIGHT = 800000;
const STALE_650K = "0".repeat(64); // same-height stale hash that sorts before the canonical's

function hashFor(height) {
  return String(height).padStart(64, "0");
}

function treeFixture(query) {
  const atHeight = query.has("at_height") ? Number(query.get("at_height")) : null;
  const atTime = query.has("at_time") ? query.get("at_time") : null;
  let nodes;
  let edges;
  let branches = [];
  if (atHeight === 650000) {
    nodes = [
      makeNode(hashFor(649999), 649999, hashFor(649998)),
      makeNode(hashFor(650000), 650000, hashFor(649999)),
      makeNode(STALE_650K, 650000, hashFor(649999), "stale"),
    ];
    edges = [
      { from_hash: hashFor(650000), to_hash: hashFor(649999), edge_kind: "canonical", hidden_count: 0 },
      { from_hash: STALE_650K, to_hash: hashFor(649999), edge_kind: "stale_entry", hidden_count: 0 },
    ];
    branches = [{ member_hashes: [STALE_650K], root_hash: STALE_650K }];
  } else if (atHeight === 600000) {
    nodes = [makeNode(hashFor(600000), 600000, hashFor(599999), "unknown")];
    edges = [];
  } else if (atHeight != null) {
    nodes = [
      makeNode(hashFor(699998), 699998, hashFor(699997)),
      makeNode(hashFor(699999), 699999, hashFor(699998)),
      makeNode(hashFor(700000), 700000, hashFor(699999)),
    ];
    edges = [
      { from_hash: hashFor(699999), to_hash: hashFor(699998), edge_kind: "canonical", hidden_count: 0 },
      { from_hash: hashFor(700000), to_hash: hashFor(699999), edge_kind: "canonical", hidden_count: 0 },
    ];
  } else if (atTime != null) {
    nodes = [
      makeNode(hashFor(450000), 450000, hashFor(449999)),
      makeNode(hashFor(450001), 450001, hashFor(450000)),
      makeNode(hashFor(450002), 450002, hashFor(450001)),
      makeNode(hashFor(450003), 450003, hashFor(450002)),
      makeNode(hashFor(450004), 450004, hashFor(450003)),
    ];
    edges = [
      { from_hash: hashFor(450001), to_hash: hashFor(450000), edge_kind: "canonical", hidden_count: 0 },
      { from_hash: hashFor(450002), to_hash: hashFor(450001), edge_kind: "canonical", hidden_count: 0 },
      { from_hash: hashFor(450003), to_hash: hashFor(450002), edge_kind: "canonical", hidden_count: 0 },
      { from_hash: hashFor(450004), to_hash: hashFor(450003), edge_kind: "canonical", hidden_count: 0 },
    ];
  } else {
    nodes = [makeNode(hashFor(TIP_HEIGHT), TIP_HEIGHT, hashFor(TIP_HEIGHT - 1))];
    edges = [];
  }
  const heights = nodes.map((node) => node.height);
  return treeEnvelope(query, {
    query: {
      from_height: null,
      to_height: null,
      at_height: atHeight,
      at_time: atTime,
      window_mode: atHeight != null ? "height" : (atTime != null ? "time" : "tip"),
      context: query.get("context") || "compact",
    },
    window: {
      btc_height_min: Math.min(...heights),
      btc_height_max: Math.max(...heights),
      tip_height: TIP_HEIGHT,
      defaulted_to_tip: atHeight == null && atTime == null,
    },
    nodes,
    edges,
    branches,
  });
}

async function stubApi(page, treeRequests, options = {}) {
  await stubSharedApi(page, treeRequests, {
    treePayload: treeFixture,
    blockPayload: (hash) => {
      const { id, prev_id, prev_hash, ...block } = makeNode(hash, TIP_HEIGHT, null);
      return {
        schema_version: "v1",
        generated_at: GENERATED_AT,
        block,
      };
    },
    ...options,
  });
}

const centerX = (box) => box.x + box.width / 2;
const centerY = (box) => box.y + box.height / 2;

async function panCanvas(page, dx, dy) {
  const svg = await page.locator("#tree-svg").boundingBox();
  const sx = svg.x + 40;
  const sy = svg.y + 40;
  await page.mouse.move(sx, sy);
  await page.mouse.down();
  await page.mouse.move(sx + dx, sy + dy, { steps: 5 });
  await page.mouse.up();
}

function distanceFromCenter(page, node) {
  return (async () => {
    const svg = await page.locator("#tree-svg").boundingBox();
    const box = await node.boundingBox();
    return Math.abs(centerX(box) - centerX(svg)) + Math.abs(centerY(box) - centerY(svg));
  })();
}

test("Live tip re-anchors the camera on the tip after a block was selected", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();

  const tipNode = page.locator(`g.tree-node[aria-label*="${TIP_HEIGHT}"]`);
  await expect(tipNode).toHaveCount(1);
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "true");
  const initialTipX = centerX(await tipNode.boundingBox());

  await page.locator('input[name="treeHeight"]').fill("700000");
  await page.locator('input[name="treeHeight"]').press("Enter");
  const heightNode = page.locator('g.tree-node[aria-label*="700000"]');
  await expect(heightNode).toHaveCount(1);
  await heightNode.click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");

  await page.selectOption("#nav-goto", "tip");
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "true");
  const tipAfter = page.locator(`g.tree-node[aria-label*="${TIP_HEIGHT}"]`);
  await expect(tipAfter).toHaveCount(1);
  await expect(page.locator("g.tree-node")).toHaveCount(1);

  const finalTipX = centerX(await tipAfter.boundingBox());
  expect(Math.abs(finalTipX - initialTipX)).toBeLessThan(6);
});

test("a block click with a small micro-drag still selects and opens the drawer", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  const node = page.locator("g.tree-node").first();
  await expect(node).toHaveCount(1);
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "true");

  // ~3.6px drift stays under d3-zoom clickDistance(4).
  const box = await node.boundingBox();
  const cx = centerX(box);
  const cy = centerY(box);
  await page.mouse.move(cx, cy);
  await page.mouse.down();
  await page.mouse.move(cx + 3, cy + 2);
  await page.mouse.up();

  await expect(page.locator('g.tree-node[data-selected="true"]')).toHaveCount(1);
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");
});

test("a shared-link restore whose focus block is absent from the window anchors the tip instead of silently no-opping", async ({ page }) => {
  const treeRequests = [];
  const warnings = [];
  page.on("console", (msg) => {
    if (msg.type() === "warning") warnings.push(msg.text());
  });
  await stubApi(page, treeRequests);

  const absent = "f".repeat(64);
  await page.goto(`/?tree_height=700000&selected=${absent}`);
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator("g.tree-node")).toHaveCount(3);

  await expect
    .poll(() => warnings.some((w) => w.includes("absent from rendered window")))
    .toBe(true);

  const svgBox = await page.locator("#tree-svg").boundingBox();
  const tipBox = await page.locator('g.tree-node[aria-label*="700000"]').boundingBox();
  expect(Math.abs(centerX(tipBox) - centerX(svgBox))).toBeLessThan(6);
  expect(Math.abs(centerY(tipBox) - centerY(svgBox))).toBeLessThan(6);
});

test("clicking a near-edge block recenters it", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto(`/?selected=${hashFor(TIP_HEIGHT)}`);
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  const node = page.locator("g.tree-node").first();
  await expect(node).toHaveCount(1);
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");

  const svg = await page.locator("#tree-svg").boundingBox();
  await panCanvas(page, Math.round(svg.width * 0.4), 0);
  expect(await distanceFromCenter(page, node)).toBeGreaterThan(40);
  await node.click();
  await expect.poll(() => distanceFromCenter(page, node)).toBeLessThan(8);
});

test("clicking an in-view block leaves the camera put", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto(`/?selected=${hashFor(TIP_HEIGHT)}`);
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  const node = page.locator("g.tree-node").first();
  await expect(node).toHaveCount(1);
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");

  await panCanvas(page, 70, 50);
  expect(await distanceFromCenter(page, node)).toBeGreaterThan(40);
  const before = await node.boundingBox();
  await node.click();
  await page.waitForTimeout(150);
  const after = await node.boundingBox();
  expect(Math.abs(centerX(after) - centerX(before))).toBeLessThan(4);
  expect(Math.abs(centerY(after) - centerY(before))).toBeLessThan(4);
});

test("entering a height centers the canonical block, not a same-height stale", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator("g.tree-node")).toHaveCount(1);

  await page.locator('input[name="treeHeight"]').fill("650000");
  await page.locator('input[name="treeHeight"]').press("Enter");

  const canonical = page.locator('g.tree-node[aria-label*="canonical 650000"]');
  const stale = page.locator('g.tree-node[aria-label*="stale 650000"]');
  await expect(canonical).toHaveCount(1);
  await expect(stale).toHaveCount(1);

  await expect.poll(() => distanceFromCenter(page, canonical)).toBeLessThan(8);
  const svgBox = await page.locator("#tree-svg").boundingBox();
  const staleBox = await stale.boundingBox();
  expect(Math.abs(centerY(staleBox) - centerY(svgBox))).toBeGreaterThan(20);
});

test("re-entering the active height recenters without refetching", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator("g.tree-node")).toHaveCount(1);

  await page.locator('input[name="treeHeight"]').fill("700000");
  await page.locator('input[name="treeHeight"]').press("Enter");
  const target = page.locator('g.tree-node[aria-label*="700000"]');
  await expect(target).toHaveCount(1);
  await expect.poll(() => treeRequests.some((u) => u.searchParams.get("at_height") === "700000")).toBe(true);
  await expect.poll(() => distanceFromCenter(page, target)).toBeLessThan(8);
  const requestsAfterLoad = treeRequests.length;

  await panCanvas(page, 150, 90);
  expect(await distanceFromCenter(page, target)).toBeGreaterThan(40);
  await page.locator('input[name="treeHeight"]').press("Enter");

  await expect.poll(() => distanceFromCenter(page, target)).toBeLessThan(8);
  expect(treeRequests.length).toBe(requestsAfterLoad);
});

test("clicking an orphan/unknown node opens it via the empty-anchor fallback", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/?tree_height=600000");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  const unknown = page.locator('g.tree-node[aria-label*="unknown 600000"]');
  await expect(unknown).toHaveCount(1);

  await unknown.click();

  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");
  await expect(page.locator('g.tree-node[data-selected="true"]')).toHaveCount(1);
});

test("a catalogued invalid block is a standalone tree node with its rejection reason", async ({ page }) => {
  const treeRequests = [];
  const invalidHash = "e".repeat(64);
  const invalidBlock = {
    ...makeNode(invalidHash, 946213, null, "invalid_block"),
    invalid_block_reason: "time_below_mtp",
  };
  await stubApi(page, treeRequests, {
    treePayload: (query) => treeEnvelope(query, {
      nodes: [invalidBlock],
      edges: [],
      legend: {
        kinds: ["canonical", "stale", "invalid_block", "unknown"],
        edge_kinds: ["canonical", "stale_entry", "stale", "hidden"],
      },
    }),
    blockPayload: () => ({
      schema_version: "v1",
      generated_at: GENERATED_AT,
      block: invalidBlock,
    }),
  });

  await page.goto("/?tree_height=946213");
  const invalidNode = page.locator('g.tree-node[aria-label*="invalid_block 946213"]');
  await expect(invalidNode).toHaveCount(1);
  await expect(page.locator("#tree-svg line.tree-edge")).toHaveCount(0);

  await invalidNode.click();
  await expect(page.locator("#drawer")).toContainText("Consensus rejection");
  await expect(page.locator("#drawer")).toContainText(
    "Block time at or below median-time-past",
  );
  await expect(page.locator("#drawer")).not.toContainText("time_below_mtp");
  await expect(page.locator("#drawer")).not.toContainText("Orphan class");
  await expect(page.locator("#drawer")).toContainText("never raced");

  await page
    .locator('#drawer button[data-consensus-rule-info="time_below_mtp"]')
    .click();
  const ruleDialog = page.locator("#consensus-rule-dialog");
  await expect(ruleDialog).toBeVisible();
  await expect(ruleDialog).toContainText("median-time-past");
});

test("an unmapped consensus rule token falls back to the raw value with no help control", async ({ page }) => {
  const invalidHash = "e".repeat(64);
  const invalidBlock = {
    ...makeNode(invalidHash, 946213, null, "invalid_block"),
    invalid_block_reason: "some_future_rule_token",
  };
  await stubApi(page, [], {
    treePayload: (query) => treeEnvelope(query, {
      nodes: [invalidBlock],
      edges: [],
      legend: {
        kinds: ["canonical", "stale", "invalid_block", "unknown"],
        edge_kinds: ["canonical", "stale_entry", "stale", "hidden"],
      },
    }),
    blockPayload: () => ({
      schema_version: "v1",
      generated_at: GENERATED_AT,
      block: invalidBlock,
    }),
  });

  await page.goto("/?tree_height=946213");
  await page.locator('g.tree-node[aria-label*="invalid_block 946213"]').click();
  await expect(page.locator("#drawer")).toContainText("Consensus rejection");
  await expect(page.locator("#drawer")).toContainText("some_future_rule_token");
  await expect(page.locator("#drawer button[data-consensus-rule-info]")).toHaveCount(0);
});

test("entering a date centers the era roughly on the window mid-height", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator("g.tree-node")).toHaveCount(1);

  await page.locator('input[name="treeTime"]').fill("2017-01-01T00:00");
  await page.locator('input[name="treeTime"]').press("Enter");

  const mid = page.locator('g.tree-node[aria-label*="450002"]');
  await expect(mid).toHaveCount(1);
  await expect.poll(() => distanceFromCenter(page, mid)).toBeLessThan(8);
});

test("re-entering the active date recenters without refetching", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator("g.tree-node")).toHaveCount(1);

  await page.locator('input[name="treeTime"]').fill("2017-01-01T00:00");
  await page.locator('input[name="treeTime"]').press("Enter");
  const mid = page.locator('g.tree-node[aria-label*="450002"]');
  await expect(mid).toHaveCount(1);
  await expect.poll(() => treeRequests.some((u) => u.searchParams.has("at_time"))).toBe(true);
  await expect.poll(() => distanceFromCenter(page, mid)).toBeLessThan(8);
  const requestsAfterLoad = treeRequests.length;

  await panCanvas(page, 150, 90);
  expect(await distanceFromCenter(page, mid)).toBeGreaterThan(40);
  await page.locator('input[name="treeTime"]').press("Enter");

  await expect.poll(() => distanceFromCenter(page, mid)).toBeLessThan(8);
  expect(treeRequests.length).toBe(requestsAfterLoad);
});
