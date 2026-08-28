const { expect, test } = require("@playwright/test");
const { GENERATED_AT, makeNode, moduleUrl, stubApi, treeEnvelope } = require("./support/api-stubs");

const VIEW_SHELL_MODULE = moduleUrl("view-shell.js");

// Shell-level coverage for the top-level view mechanism. Three views are now
// registered, so these assert the switcher, the scope swap and the URL
// contract; behaviour specific to the distribution lives in delta-view.spec.js
// and to the findings feed in findings-view.spec.js.

test("every view is registered and the tree is the default", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);
  await page.goto("/");

  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await expect(page.locator(".tree-card")).toBeVisible();
  expect(treeRequests.length).toBeGreaterThan(0);

  await expect(page.locator("#view-switcher")).toBeVisible();
  await expect(page.locator(".view-tab")).toHaveCount(3);
  const registered = await page.evaluate(
    (moduleUrl) => import(moduleUrl).then((module) => module.registeredViews()),
    VIEW_SHELL_MODULE,
  );
  expect(registered).toEqual(["tree", "delta", "findings"]);
});

test("every registered view activates without error", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/");

  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));

  const activated = await page.evaluate(async (moduleUrl) => {
    const shell = await import(moduleUrl);
    const seen = [];
    for (const id of shell.registeredViews()) {
      await shell.activateView(id);
      seen.push(document.querySelector(".workspace").dataset.view);
    }
    return seen;
  }, VIEW_SHELL_MODULE);

  expect(activated).toEqual(["tree", "delta", "findings"]);
  expect(errors).toEqual([]);
});

test("the switcher swaps scopes in both directions", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/");

  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "delta");
  await expect(page.locator('.view-tab[data-view="delta"]')).toHaveAttribute("aria-selected", "true");
  await expect(page.locator(".tree-card")).toBeHidden();
  await expect(page.locator("#delta-main")).toBeVisible();
  // The shared Source filter stays across views; the tree-only controls go.
  await expect(page.locator("#source-controls")).toBeVisible();
  await expect(page.locator('.filter-form > [data-view-scope="tree"]')).toBeHidden();
  expect(new URL(page.url()).searchParams.get("view")).toBe("delta");

  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await expect(page.locator(".tree-card")).toBeVisible();
  await expect(page.locator("#delta-main")).toBeHidden();
  // The default view drops its query parameter.
  expect(new URL(page.url()).searchParams.get("view")).toBeNull();
});

test("an unregistered view falls back to the tree", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);
  // `trends` is the view that will exist eventually; until it does it must
  // degrade rather than leaving the workspace blank.
  await page.goto("/?view=trends");

  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await expect(page.locator(".tree-card")).toBeVisible();
  expect(treeRequests.length).toBeGreaterThan(0);
  expect(new URL(page.url()).searchParams.get("view")).toBeNull();
});

test("the default view adds no query parameter", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/?tree_height=700000");

  const params = new URL(page.url()).searchParams;
  expect(params.get("view")).toBeNull();
  expect(params.get("tree_height")).toBe("700000");
});

test("a bare selection does not recenter the camera on it", async ({ page }) => {
  // A multi-node window so "centered on the selection" is distinguishable from
  // "tip-anchored". Neither an absolute transform nor a node's screen box works
  // here: selecting also auto-expands the drawer, which narrows the tree column
  // and moves everything for reasons unrelated to the camera. What is invariant
  // is WHICH node sits under the canvas centre.
  const nodes = [
    makeNode("a".repeat(64), 700000, null, "canonical", { id: 1, prev_id: null }),
    makeNode("b".repeat(64), 700001, "a".repeat(64), "canonical", { id: 2, prev_id: 1 }),
    makeNode("c".repeat(64), 700002, "b".repeat(64), "canonical", { id: 3, prev_id: 2 }),
  ];
  await stubApi(page, [], { treePayload: (params) => treeEnvelope(params, { nodes }) });

  await page.goto(`/?selected=${"a".repeat(64)}`);
  await expect(page.locator("g.tree-node")).toHaveCount(3);

  const offset = await page.evaluate(() => {
    const svg = document.querySelector("#tree-svg").getBoundingClientRect();
    const node = document.querySelector('g.tree-node[aria-label*="700000"]').getBoundingClientRect();
    return Math.abs((node.x + node.width / 2) - (svg.x + svg.width / 2));
  });

  expect(offset).toBeGreaterThan(40);
});

test("manual refresh loads sources and the view concurrently", async ({ page }) => {
  // Serialising these would leave a window in which a navigator jump starts
  // during the sources fetch and is then superseded by the refresh's own
  // sequence-guarded tree load, restoring the pre-jump window. The observable
  // property is ordering: with a slow /sources, the refresh's tree request must
  // go out before that fetch resolves.
  const order = [];
  await stubApi(page, []);
  await page.goto("/");
  await expect(page.locator(".tree-card")).toBeVisible();

  await page.route("**/api/v1/sources", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 400));
    order.push("sources:done");
    await route.fulfill({ json: { schema_version: "v1", generated_at: GENERATED_AT, sources: [] } });
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/tree")) order.push("tree:start");
  });

  await page.locator("#last-updated").click();
  await expect.poll(() => order.includes("sources:done")).toBe(true);

  expect(order).toContain("tree:start");
  expect(order.indexOf("tree:start")).toBeLessThan(order.indexOf("sources:done"));
});

test("the freshness stamp is a button and keyboard-activates refresh", async ({ page }) => {
  const order = [];
  await stubApi(page, []);
  await page.goto("/");
  await expect(page.locator(".tree-card")).toBeVisible();

  const stamp = page.locator("#last-updated");
  expect(await stamp.evaluate((el) => el.tagName)).toBe("BUTTON");
  await expect(stamp).toHaveAccessibleName(/Updated \d{2}:\d{2}:\d{2}; refresh now/);

  await page.route("**/api/v1/sources", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 400));
    order.push("sources:done");
    await route.fulfill({ json: { schema_version: "v1", generated_at: GENERATED_AT, sources: [] } });
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/tree")) order.push("tree:start");
  });

  await stamp.focus();
  await page.keyboard.press("Enter");
  await expect.poll(() => order.includes("sources:done")).toBe(true);
  expect(order).toContain("tree:start");
  expect(order.indexOf("tree:start")).toBeLessThan(order.indexOf("sources:done"));

  const afterEnter = { sources: order.filter((item) => item === "sources:done").length };
  await stamp.focus();
  await page.keyboard.press("Space");
  await expect.poll(() => order.filter((item) => item === "sources:done").length)
    .toBeGreaterThan(afterEnter.sources);
});

test("the tree-scoped wrapper keeps the rail spacing", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/");

  // Scoping is structural only: wrapping fieldsets in a [data-view-scope]
  // container must not collapse the gap .filter-form gives its children.
  const gap = await page.evaluate(() => {
    const wrapper = document.querySelector('.filter-form > [data-view-scope="tree"]');
    return getComputedStyle(wrapper).rowGap;
  });
  expect(gap).toBe("10px");
});
