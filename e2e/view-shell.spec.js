const { expect, test } = require("@playwright/test");
const { GENERATED_AT, makeNode, stubApi, treeEnvelope } = require("./support/api-stubs");

// Shell-level coverage for the top-level view mechanism, landed before any
// second view exists. What matters at this stage is that the shell is inert:
// the tree still boots normally, an unbuilt `view=` degrades instead of
// rendering an empty workspace, and no existing tree URL changes shape.

test("the tree view activates and is the only registered view", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);
  await page.goto("/");

  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await expect(page.locator(".tree-card")).toBeVisible();
  expect(treeRequests.length).toBeGreaterThan(0);

  // Only one view is registered, so the switcher must not be on screen at all.
  await expect(page.locator("#view-switcher")).toBeHidden();
  const registered = await page.evaluate(
    () => import("/js/view-shell.js?v=0.2.1").then((module) => module.registeredViews()),
  );
  expect(registered).toEqual(["tree"]);
});

test("every registered view activates without error", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/");

  const errors = [];
  page.on("pageerror", (error) => errors.push(error.message));

  const results = await page.evaluate(async () => {
    const shell = await import("/js/view-shell.js?v=0.2.1");
    const activated = [];
    for (const id of shell.registeredViews()) {
      await shell.activateView(id);
      activated.push(document.querySelector(".workspace").dataset.view);
    }
    return activated;
  });

  expect(results).toEqual(["tree"]);
  expect(errors).toEqual([]);
});

// Registering a second view is the whole point of the registry, and it is what
// slice 3 will do. Everything the shell promises for that moment - a visible
// switcher, click activation, scope swapping, URL persistence, and load/render
// dispatch - is exercised here against a stub view, so the mechanism is covered
// before the real view exists.
async function registerStubDelta(page) {
  await page.evaluate(async () => {
    const shell = await import("/js/view-shell.js?v=0.2.1");
    window.__deltaCalls = { load: 0, render: 0 };
    shell.registerView("delta", {
      label: "Distribution",
      load: async () => { window.__deltaCalls.load += 1; },
      render: () => { window.__deltaCalls.render += 1; },
    });
    // The switcher only re-renders on activation, so paint it for the current view.
    shell.applyViewScopes(document.querySelector(".workspace").dataset.view);
  });
}

test("registering a second view reveals the switcher and swaps scopes", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/");
  await expect(page.locator("#view-switcher")).toBeHidden();

  await registerStubDelta(page);
  await expect(page.locator("#view-switcher")).toBeVisible();
  await expect(page.locator(".view-tab")).toHaveCount(2);

  // Click through to the second view.
  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "delta");
  await expect(page.locator('.view-tab[data-view="delta"]')).toHaveAttribute("aria-selected", "true");
  await expect(page.locator(".tree-card")).toBeHidden();
  await expect(page.locator("#delta-main")).toBeVisible();
  // Not toBeVisible: the rail mount point is still empty in this slice, so it
  // has no box. What matters is that the scope rule stopped hiding it, ready
  // for slice 3 to render the delta fieldsets into it.
  expect(
    await page.evaluate(() => getComputedStyle(document.querySelector("#delta-controls")).display),
  ).not.toBe("none");
  // The shared Source filter stays put across views; the tree-only controls go.
  await expect(page.locator("#source-controls")).toBeVisible();
  await expect(page.locator('.filter-form > [data-view-scope="tree"]')).toBeHidden();
  expect(new URL(page.url()).searchParams.get("view")).toBe("delta");
  expect(await page.evaluate(() => window.__deltaCalls)).toEqual({ load: 1, render: 1 });

  // And back again: the default view drops its query parameter.
  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await expect(page.locator(".tree-card")).toBeVisible();
  await expect(page.locator("#delta-main")).toBeHidden();
  expect(new URL(page.url()).searchParams.get("view")).toBeNull();
});

test("a registered view is reachable straight from the URL", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/");
  await registerStubDelta(page);

  // Activation by value, the path a shared link takes once the view exists.
  await page.evaluate(async () => {
    const shell = await import("/js/view-shell.js?v=0.2.1");
    await shell.activateView("delta");
  });
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "delta");
  expect(await page.evaluate(() => window.__deltaCalls.load)).toBe(1);
});

test("an unregistered view falls back to the tree", async ({ page }) => {
  const treeRequests = [];
  await stubApi(page, treeRequests);
  // `delta` is the view slice 3 will register; until then it must degrade
  // rather than leaving the workspace blank.
  await page.goto("/?view=delta");

  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await expect(page.locator(".tree-card")).toBeVisible();
  expect(treeRequests.length).toBeGreaterThan(0);
  // The fallback is also written back to the URL, so the unreachable view does
  // not survive a copy-paste of the address bar.
  expect(new URL(page.url()).searchParams.get("view")).toBeNull();
});

test("the default view adds no query parameter", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/?tree_height=700000");

  // Existing tree URLs must round-trip unchanged: `view=tree` is the default
  // and is never written.
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

  // Select the OLDEST node, furthest from the tip anchor. Boot's focus rule
  // only recenters for an explicit tree window, and there is none here, so the
  // view must not drag the camera onto it.
  await page.goto(`/?selected=${"a".repeat(64)}`);
  await expect(page.locator("g.tree-node")).toHaveCount(3);

  const offset = await page.evaluate(() => {
    const svg = document.querySelector("#tree-svg").getBoundingClientRect();
    const node = document
      .querySelector('g.tree-node[aria-label*="700000"]')
      .getBoundingClientRect();
    return Math.abs((node.x + node.width / 2) - (svg.x + svg.width / 2));
  });

  // Centering on the selection would put it within a few pixels of the middle.
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

  // Installed after the initial load so only the refresh is measured. Playwright
  // matches the most recently registered route, so this shadows the stub.
  await page.route("**/api/v1/sources", async (route) => {
    await new Promise((resolve) => setTimeout(resolve, 400));
    order.push("sources:done");
    await route.fulfill({ json: { schema_version: "v1", generated_at: GENERATED_AT, sources: [] } });
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/tree")) order.push("tree:start");
  });

  await page.locator("#refresh-now").click();
  await expect.poll(() => order.includes("sources:done")).toBe(true);

  expect(order).toContain("tree:start");
  expect(order.indexOf("tree:start")).toBeLessThan(order.indexOf("sources:done"));
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

test("delta-scoped placeholders are inert while the view is unregistered", async ({ page }) => {
  await stubApi(page, []);
  await page.goto("/");

  // Present in the DOM as mount points, but laid out nowhere and carrying no
  // focusable controls, so they cannot overlap the tree card or steal tabs.
  await expect(page.locator("#delta-main")).toBeHidden();
  await expect(page.locator("#delta-controls")).toBeHidden();
  expect(await page.locator("#delta-main :is(button, input, select, a)").count()).toBe(0);
});
