const { expect, test } = require("@playwright/test");
const {
  GENERATED_AT,
  makeNode,
  stubApi: stubSharedApi,
  treeEnvelope,
} = require("./support/api-stubs");

// Regression coverage for tree camera/selection behaviour:
//  1. "Live tip" re-centers on the tip even when a block was selected (the
//     drawer-collapse rAF must not lock a stale transform).
//  2. A block click with a sub-pixel micro-drag still selects (d3-zoom
//     clickDistance), and now also centers.
//  3. centerCameraOnNode anchors the tip (not a stale camera) when the focus block
//     is absent from the rendered window.
//  4. The explicit focus gestures center the focal block: a node click, an entered
//     tree height (canonical-first when a stale shares the height, no refetch on a
//     redundant re-entry), and the orphan/unknown empty-anchor fallback.

const TIP_HEIGHT = 800000;
const STALE_650K = "0".repeat(64); // a same-height stale hash that sorts before the canonical's

function hashFor(height) {
  return String(height).padStart(64, "0");
}

// The rendered window depends on the at_height lookup so each test gets the
// geometry it needs:
//   tip (no at_height) -> one canonical node at TIP_HEIGHT
//   700000             -> three canonical nodes (distinct ranks)
//   650000             -> a canonical block plus a same-height stale competitor
//                         whose hash sorts BEFORE the canonical's (tie-break)
//   600000             -> a single unknown-kind node (orphan fallback)
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
    // A branch entry lanes the stale below the canonical so the two are not
    // stacked at the same point and a wrong (stale) center is observable.
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
    // A date/time lookup resolves to a canonical block and the backend builds the
    // compact window around it. Return a small canonical window so mid-height
    // centering lands on the middle block (450002).
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

async function stubApi(page, treeRequests) {
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
  });
}

const centerX = (box) => box.x + box.width / 2;
const centerY = (box) => box.y + box.height / 2;

// Drag an empty corner of the canvas to pan the camera (a >4px move is a pan, not
// a background deselect), so a focal block ends up off-center before a gesture
// that must recenter it.
async function panCanvas(page, dx, dy) {
  const svg = await page.locator("#tree-svg").boundingBox();
  const sx = svg.x + 40;
  const sy = svg.y + 40;
  await page.mouse.move(sx, sy);
  await page.mouse.down();
  await page.mouse.move(sx + dx, sy + dy, { steps: 5 });
  await page.mouse.up();
}

// Poll a node's distance (px) from the SVG viewport center (both axes summed).
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

  // Initial tip render: one node, drawer collapsed. Capture where the tip block
  // sits so we can assert Live tip returns the camera to exactly here.
  const tipNode = page.locator(`g.tree-node[aria-label*="${TIP_HEIGHT}"]`);
  await expect(tipNode).toHaveCount(1);
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "true");
  const initialTipX = centerX(await tipNode.boundingBox());

  // Navigate to a height window (different geometry). Entering a height now centers
  // on that block, so click the centered block to open the drawer before Live tip.
  await page.locator('input[name="treeHeight"]').fill("700000");
  await page.locator('input[name="treeHeight"]').press("Enter");
  const heightNode = page.locator('g.tree-node[aria-label*="700000"]');
  await expect(heightNode).toHaveCount(1);
  await heightNode.click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");

  // Live tip: must re-anchor on the tip (drawer collapses, tip view re-renders).
  await page.selectOption("#nav-goto", "tip");
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "true");
  const tipAfter = page.locator(`g.tree-node[aria-label*="${TIP_HEIGHT}"]`);
  await expect(tipAfter).toHaveCount(1);
  await expect(page.locator("g.tree-node")).toHaveCount(1);

  // The tip block must land back at its original anchored position. A stale
  // transform (the bug) would leave it offset by a full window's worth of pitch.
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

  // Press, drift ~3.6px (sqrt(3^2 + 2^2)), release: that displacement sits under
  // the clickDistance(4) threshold, so the gesture stays a click. Under d3-zoom's
  // default clickDistance(0) any drift is classified as a pan and the click is
  // swallowed.
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

  // Explicit height window (3 nodes at 699998..700000); the ?selected hash is NOT
  // one of them, so centerCameraOnNode cannot find its focus. It must report the
  // miss (and anchor the tip) rather than leaving a stale camera with no signal.
  const absent = "f".repeat(64);
  await page.goto(`/?tree_height=700000&selected=${absent}`);
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator("g.tree-node")).toHaveCount(3);

  await expect
    .poll(() => warnings.some((w) => w.includes("absent from rendered window")))
    .toBe(true);

  // The tip block (highest in the window) is anchored at the viewport center on
  // both axes (the same fallback position a fresh render uses), not a stale camera.
  const svgBox = await page.locator("#tree-svg").boundingBox();
  const tipBox = await page.locator('g.tree-node[aria-label*="700000"]').boundingBox();
  expect(Math.abs(centerX(tipBox) - centerX(svgBox))).toBeLessThan(6);
  expect(Math.abs(centerY(tipBox) - centerY(svgBox))).toBeLessThan(6);
});

test("clicking a near-edge block recenters it", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  // Start with the drawer open (shared link) and the block centered so the click
  // does not reflow and the edge check runs on stable geometry.
  await page.goto(`/?selected=${hashFor(TIP_HEIGHT)}`);
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  const node = page.locator("g.tree-node").first();
  await expect(node).toHaveCount(1);
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");

  // Pan the block hard toward the right edge (to ~90% of the width), then click it:
  // a near-edge click recenters.
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

  // Pan the block a little so it is off-center but comfortably in view (not near an
  // edge), then click it: a routine in-view click must NOT jerk the camera.
  await panCanvas(page, 70, 50);
  expect(await distanceFromCenter(page, node)).toBeGreaterThan(40);
  const before = await node.boundingBox();
  await node.click();
  await page.waitForTimeout(150); // let the post-click rAF run; it must not center
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

  // The CANONICAL at the height is centered (canonical-first tie-break) even though
  // the stale competitor's hash sorts before it.
  await expect.poll(() => distanceFromCenter(page, canonical)).toBeLessThan(8);
  // The stale (same height, laned below) is therefore NOT at the vertical center.
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

  // Enter a height: loads the window and centers on the canonical at that height.
  await page.locator('input[name="treeHeight"]').fill("700000");
  await page.locator('input[name="treeHeight"]').press("Enter");
  const target = page.locator('g.tree-node[aria-label*="700000"]');
  await expect(target).toHaveCount(1);
  await expect.poll(() => treeRequests.some((u) => u.searchParams.get("at_height") === "700000")).toBe(true);
  await expect.poll(() => distanceFromCenter(page, target)).toBeLessThan(8);
  const requestsAfterLoad = treeRequests.length;

  // Pan away, then re-enter the SAME height: it must recenter WITHOUT a refetch.
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

  // The unknown node routes through the orphan navigator, whose stub returns no
  // item; the loadBlockThenCenter fallback must still select it and open the
  // drawer (its recenter is edge-conditional, covered by the click tests above).
  await unknown.click();

  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");
  await expect(page.locator('g.tree-node[data-selected="true"]')).toHaveCount(1);
});

test("entering a date centers the era roughly on the window mid-height", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Bitcoin Header Tree" })).toBeVisible();
  await expect(page.locator("g.tree-node")).toHaveCount(1);

  await page.locator('input[name="treeTime"]').fill("2017-01-01T00:00");
  await page.locator('input[name="treeTime"]').press("Enter");

  // The date resolves to a canonical window (450000..450004); approximate centering
  // lands the mid-height block (450002) at the viewport center.
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

  // Pan away, then re-enter the SAME date: it must recenter WITHOUT a refetch.
  await panCanvas(page, 150, 90);
  expect(await distanceFromCenter(page, mid)).toBeGreaterThan(40);
  await page.locator('input[name="treeTime"]').press("Enter");

  await expect.poll(() => distanceFromCenter(page, mid)).toBeLessThan(8);
  expect(treeRequests.length).toBe(requestsAfterLoad);
});
