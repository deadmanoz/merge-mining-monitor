const { expect, test } = require("@playwright/test");
const {
  GENERATED_AT,
  blockCompetition,
  blockPayload,
  competitionsPayload,
  liveSourcesPayload,
  makeCompetition,
  makeNode,
  moduleUrl,
  stubApi,
  treeEnvelope,
} = require("./support/api-stubs");

const STALE = "a".repeat(64);
const OTHER = "b".repeat(64);

// A tree holding the stale block, plus the competitions and block payloads.
function scenario({ delta = 90000, rows = null, competitionOverrides = {} } = {}) {
  const nodes = [
    makeNode(STALE, 700000, null, "stale", { id: 1, prev_id: null }),
    makeNode(OTHER, 700001, STALE, "canonical", { id: 2, prev_id: 1 }),
  ];
  return {
    sourcesPayload: liveSourcesPayload,
    treePayload: (params) => treeEnvelope(params, { nodes }),
    competitionsPayload: () => competitionsPayload(rows ?? [
      makeCompetition(STALE, 700000, delta),
      makeCompetition(OTHER, 700001, 3),
    ]),
    blockPayload: (hash) => blockPayload(hash, {
      competition: blockCompetition(hash, 700000, delta, competitionOverrides),
    }),
  };
}

test("a block's Header Time Delta opens the distribution focused on it", async ({ page }) => {
  const competitionRequests = [];
  await stubApi(page, [], scenario());
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) competitionRequests.push(request.url());
  });
  await page.goto(`/?selected=${STALE}`);
  await expect(page.locator("#drawer")).toContainText("Competition");

  const link = page.locator('#drawer .kv-link[data-action="delta"]');
  await expect(link).toBeVisible();
  await link.click();

  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "delta");
  const params = new URL(page.url()).searchParams;
  expect(params.get("view")).toBe("delta");
  expect(params.get("selected")).toBe(STALE);
  await expect.poll(() => competitionRequests.length).toBeGreaterThan(0);

  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("a null delta is not a cross-link", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: null }));
  await page.goto(`/?selected=${STALE}`);
  await expect(page.locator("#drawer")).toContainText("Competition");

  await expect(page.locator('#drawer .kv-link[data-action="delta"]')).toHaveCount(0);
  await expect(page.locator("#drawer")).toContainText("Header Time Delta");
});

test("an in-window selection marks the bin that holds it", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: 12 }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator('#delta-presets [data-half="30"]').click();
  await page.locator("#delta-bin-width").selectOption("10");

  const bars = await page.locator("#delta-chart .bar-mark").evaluateAll(
    (nodes) => nodes.map((node) => node.getAttribute("class")),
  );
  expect(bars.length).toBe(2);
  expect(bars.filter((cls) => cls.includes("is-selected"))).toEqual([
    expect.stringContaining("bar-pos"),
  ]);
  expect(bars.find((cls) => !cls.includes("is-selected"))).toContain("bar-zero");
  await page.locator("#delta-chart .bar-hit").last().hover();
  await expect(page.locator("#delta-tooltip")).toContainText("holds the selected competition");
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
});

test("a selection outside the window marks no bin", async ({ page }) => {
  // Include a +28s row to populate the outermost bin so a clamped lookup would mis-mark it.
  await stubApi(page, [], scenario({
    rows: [
      makeCompetition(STALE, 700000, 90000),
      makeCompetition(OTHER, 700001, 3),
      makeCompetition("c".repeat(64), 700002, 28),
    ],
  }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator('#delta-presets [data-half="30"]').click();
  await page.locator("#delta-bin-width").selectOption("10");
  await expect(page.locator("#delta-chart .bar-mark")).toHaveCount(2);

  await expect(page.locator("#delta-chart .bar-mark.is-selected")).toHaveCount(0);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("clearing the filters that hide a selection reveals it", async ({ page }) => {
  await stubApi(page, [], scenario({
    rows: [
      makeCompetition(STALE, 700000, 90000, { sources: ["auxpow:namecoin"] }),
      makeCompetition(OTHER, 700001, 3, { sources: ["auxpow:rsk"] }),
    ],
  }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();

  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await expect(page.locator(".hidden-selection")).toBeVisible();
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(0);

  await page.locator(".hidden-selection [data-action='reveal']").click();

  await expect(page.locator(".hidden-selection")).toHaveCount(0);
  await expect(page.locator('input[name="source"][value="auxpow:rsk"]')).not.toBeChecked();
  expect(new URL(page.url()).searchParams.get("sources")).toBeNull();
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("revealing an era-hidden selection leaves the Source filter alone", async ({ page }) => {
  const Y2013 = 1_370_000_000;
  const Y2024 = 1_710_000_000;
  await stubApi(page, [], scenario({
    rows: [
      makeCompetition(STALE, 700000, 90000, { sources: ["auxpow:rsk"], stale_header_time: Y2013 }),
      makeCompetition(OTHER, 700001, 3, { sources: ["auxpow:rsk"], stale_header_time: Y2024 }),
    ],
  }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();

  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await page.locator("#delta-year-from").selectOption("2024");
  await expect(page.locator(".hidden-selection")).toBeVisible();

  await page.locator(".hidden-selection [data-action='reveal']").click();

  await expect(page.locator(".hidden-selection")).toHaveCount(0);
  await expect(page.locator("#delta-year-from")).toHaveValue("2013");
  await expect(page.locator('input[name="source"][value="auxpow:rsk"]')).toBeChecked();
  expect(new URL(page.url()).searchParams.get("sources")).toBe("auxpow:rsk");
});

test("a cold link restores the view, the selection and the open drawer", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?view=delta&selected=${STALE}`);

  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "delta");
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");
  await expect(page.locator("#drawer")).toContainText("Competition");
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("the focused outlier row is scrolled into view", async ({ page }) => {
  const rows = [
    makeCompetition(STALE, 700000, 3600),
    ...Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(41);

  const geometry = await page.evaluate(() => {
    const scroller = document.querySelector("#delta-outlier-body");
    const row = scroller.querySelector('.outlier-row[aria-current="true"]');
    const s = scroller.getBoundingClientRect();
    const r = row.getBoundingClientRect();
    return {
      scrolls: scroller.scrollHeight > scroller.clientHeight,
      insideViewport: r.top >= 0 && r.bottom <= window.innerHeight,
      insideScroller: r.top >= s.top - 1 && r.bottom <= s.bottom + 1,
      scrollTop: scroller.scrollTop,
    };
  });
  expect(geometry.scrolls).toBe(true);
  expect(geometry.scrollTop).toBeGreaterThan(0);
  expect(geometry.insideScroller).toBe(true);
  expect(geometry.insideViewport).toBe(true);
});

test("the outlier list is a scroller, and its disclosure gives the row back", async ({ page }) => {
  await stubApi(page, [], scenario({
    rows: Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  }));
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(40);

  const open = await page.evaluate(() => {
    const scroller = document.querySelector("#delta-outlier-body");
    const panel = document.querySelector("#delta-outliers");
    return {
      scrolls: scroller.scrollHeight > scroller.clientHeight,
      panelClips: panel.scrollHeight > panel.clientHeight,
    };
  });
  expect(open.scrolls).toBe(true);
  expect(open.panelClips).toBe(false);

  const toggle = page.locator("#delta-outliers-toggle");
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
  await expect(page.locator("#delta-outlier-body")).toBeHidden();
  const collapsedRows = await page.locator("#delta-outliers").evaluate(
    (panel) => getComputedStyle(panel).gridTemplateRows,
  );
  expect(collapsedRows.split(" ").length).toBe(1);
});

test("the outlier cap never drops the focused competition", async ({ page }) => {
  const rows = [
    makeCompetition(STALE, 700000, 3600),
    ...Array.from({ length: 260 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();

  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(251);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body"))
    .toContainText("250 largest of 261, plus the selected competition");
});

test("a selection that becomes an outlier is scrolled to then", async ({ page }) => {
  const rows = [
    makeCompetition(STALE, 700000, 45),
    ...Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(0);

  await page.locator('#delta-presets [data-half="30"]').click();
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);

  const geometry = await page.evaluate(() => {
    const scroller = document.querySelector("#delta-outlier-body");
    const row = scroller.querySelector('.outlier-row[aria-current="true"]');
    const s = scroller.getBoundingClientRect();
    const r = row.getBoundingClientRect();
    return { scrollTop: scroller.scrollTop, inside: r.top >= s.top - 1 && r.bottom <= s.bottom + 1 };
  });
  expect(geometry.scrollTop).toBeGreaterThan(0);
  expect(geometry.inside).toBe(true);
});

test("a cross-link refreshes a snapshot that predates the selection", async ({ page }) => {
  let competitionRows = [makeCompetition(OTHER, 700001, 3)];
  const requests = [];
  await stubApi(page, [], {
    ...scenario(),
    competitionsPayload: () => competitionsPayload(competitionRows),
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) requests.push(request.url());
  });

  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect.poll(() => requests.length).toBe(1);

  competitionRows = [makeCompetition(STALE, 700000, 90000), makeCompetition(OTHER, 700001, 3)];
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.evaluate(async ([hash, api_url]) => {
    const api = await import(api_url);
    await api.loadBlock(hash);
  }, [STALE, moduleUrl("api-client.js")]);
  await page.locator('#drawer .kv-link[data-action="delta"]').click();

  await expect.poll(() => requests.length).toBe(2);
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("a selection hidden then revealed is scrolled to again", async ({ page }) => {
  const rows = [
    makeCompetition(STALE, 700000, 3600, { sources: ["auxpow:namecoin"] }),
    ...Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000, { sources: ["auxpow:namecoin"] },
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  expect(await page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop)).toBeGreaterThan(0);

  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await expect(page.locator(".hidden-selection")).toBeVisible();
  await page.locator('input[name="source"][value="auxpow:rsk"]').uncheck();

  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);
});

test("opening the collapsed list scrolls to the focused row", async ({ page }) => {
  const rows = [
    makeCompetition(STALE, 700000, 45),
    ...Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(40);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(0);

  await page.locator("#delta-outliers-toggle").click();
  await expect(page.locator("#delta-outlier-body")).toBeHidden();
  await page.locator('#delta-presets [data-half="30"]').click();

  await page.locator("#delta-outliers-toggle").click();
  await expect(page.locator("#delta-outlier-body")).toBeVisible();
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);
});

test("reveal still clears a Source filter when the rail failed to load", async ({ page }) => {
  await stubApi(page, [], {
    ...scenario(),
    competitionsPayload: () => competitionsPayload([
      makeCompetition(STALE, 700000, 90000, { sources: ["auxpow:namecoin"] }),
      makeCompetition(OTHER, 700001, 3, { sources: ["auxpow:rsk"] }),
    ]),
  });
  await page.route("**/api/v1/sources", (route) => route.fulfill({
    status: 500,
    json: { schema_version: "v1", generated_at: GENERATED_AT, error: { code: "internal_error", message: "boom" } },
  }));
  await page.goto(`/?view=delta&selected=${STALE}&sources=auxpow:rsk`);
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator('#source-controls input[name="source"]')).toHaveCount(0);
  await expect(page.locator(".hidden-selection")).toBeVisible();

  await page.locator(".hidden-selection [data-action='reveal']").click();

  await expect(page.locator(".hidden-selection")).toHaveCount(0);
  expect(new URL(page.url()).searchParams.get("sources")).toBeNull();
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("a visible selection can be shown in the tree", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator(".hidden-selection")).toHaveCount(0);
  await expect(page.locator(".selection-note")).toBeVisible();

  await page.locator(".selection-note [data-action='tree']").click();

  await expect(page.locator(".tree-card")).toBeVisible();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await expect(page.locator('input[name="treeHeight"]')).toHaveValue("700000");
  await expect(page.locator('input[name="treeTime"]')).toHaveValue("");
});

test("a failed repair refetch stays retryable", async ({ page }) => {
  let rows = [makeCompetition(OTHER, 700001, 3)];
  let fail = false;
  const requests = [];
  await stubApi(page, [], scenario());
  await page.route("**/api/v1/competitions", async (route) => {
    requests.push(route.request().url());
    if (fail) {
      await route.fulfill({
        status: 500,
        json: { schema_version: "v1", generated_at: GENERATED_AT, error: { code: "internal_error", message: "boom" } },
      });
      return;
    }
    await route.fulfill({ json: competitionsPayload(rows) });
  });

  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect.poll(() => requests.length).toBe(1);

  fail = true;
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.evaluate(async ([hash, api_url]) => {
    const api = await import(api_url);
    await api.loadBlock(hash);
  }, [STALE, moduleUrl("api-client.js")]);
  await page.locator('#drawer .kv-link[data-action="delta"]').click();
  await expect.poll(() => requests.length).toBe(2);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(0);

  fail = false;
  rows = [makeCompetition(STALE, 700000, 90000), makeCompetition(OTHER, 700001, 3)];
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.locator('.view-tab[data-view="delta"]').click();

  await expect.poll(() => requests.length).toBe(3);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("a snapshot repair does not wait on the block detail request", async ({ page }) => {
  let rows = [makeCompetition(OTHER, 700001, 3)];
  const requests = [];
  await stubApi(page, [], {
    ...scenario(),
    competitionsPayload: () => competitionsPayload(rows),
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) requests.push(request.url());
  });
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect.poll(() => requests.length).toBe(1);

  rows = [makeCompetition(STALE, 700000, 90000), makeCompetition(OTHER, 700001, 3)];
  let releaseBlock;
  const heldBlock = new Promise((resolve) => { releaseBlock = resolve; });
  await page.route(`**/api/v1/block/${STALE}`, async (route) => {
    await heldBlock;
    await route.fallback();
  });

  await page.locator('.view-tab[data-view="tree"]').click();
  await page.evaluate(([hash, api_url]) => {
    import(api_url).then((api) => api.loadBlock(hash));
  }, [STALE, moduleUrl("api-client.js")]);
  await page.locator('.view-tab[data-view="delta"]').click();

  await expect.poll(() => requests.length).toBe(2);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  releaseBlock();
});

test("a render that paints off-screen does not consume the owed scroll", async ({ page }) => {
  const rows = [
    makeCompetition(STALE, 700000, 3600),
    ...Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  let first = true;
  await stubApi(page, [], scenario({ rows }));
  await page.route("**/api/v1/competitions", async (route) => {
    if (first) { first = false; await held; }
    await route.fulfill({ json: competitionsPayload(rows) });
  });

  await page.goto(`/?selected=${STALE}`);
  await expect(page.locator(".tree-card")).toBeVisible();
  await page.locator('.view-tab[data-view="delta"]').click();
  await page.locator('.view-tab[data-view="tree"]').click();
  release();
  await expect.poll(
    () => page.locator("#delta-outlier-body .outlier-row").count(),
  ).toBe(41);

  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);
});

test("a selection with no competition does not refetch on every activation", async ({ page }) => {
  const requests = [];
  await stubApi(page, [], {
    ...scenario(),
    blockPayload: (hash) => blockPayload(hash, { competition: null }),
    competitionsPayload: () => competitionsPayload([makeCompetition(OTHER, 700001, 3)]),
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) requests.push(request.url());
  });

  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator("#drawer")).toContainText("Parent block");
  await expect(page.locator("#drawer")).not.toContainText("Competition");
  const afterLoad = requests.length;
  expect(afterLoad).toBeGreaterThan(0);

  await page.locator('.view-tab[data-view="tree"]').click();
  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await page.locator('input[name="source"][value="auxpow:rsk"]').uncheck();

  expect(requests.length).toBe(afterLoad);
});

test("following the cross-link again scrolls to the row again", async ({ page }) => {
  const rows = [
    makeCompetition(STALE, 700000, 3600),
    ...Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?selected=${STALE}`);
  await expect(page.locator("#drawer")).toContainText("Competition");

  await page.locator('#drawer .kv-link[data-action="delta"]').click();
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);

  await page.locator("#delta-outlier-body").evaluate((n) => { n.scrollTop = 0; });
  await page.locator('.view-tab[data-view="tree"]').click();

  await page.locator('#drawer .kv-link[data-action="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);
});

test("a 200 that still lacks the row is retried, not given up on", async ({ page }) => {
  let rows = [makeCompetition(OTHER, 700001, 3)];
  const requests = [];
  await stubApi(page, [], {
    ...scenario(),
    competitionsPayload: () => competitionsPayload(rows),
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) requests.push(request.url());
  });

  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#drawer")).toContainText("Competition");
  await expect.poll(() => requests.length).toBeGreaterThan(0);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(0);
  const afterFirst = requests.length;

  rows = [makeCompetition(STALE, 700000, 90000), makeCompetition(OTHER, 700001, 3)];
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.locator('.view-tab[data-view="delta"]').click();

  await expect.poll(() => requests.length).toBeGreaterThan(afterFirst);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("the cross-link opens the panel it focuses into", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?selected=${STALE}`);
  await expect(page.locator("#drawer")).toContainText("Competition");

  await page.locator('.view-tab[data-view="delta"]').click();
  await page.locator("#delta-outliers-toggle").click();
  await expect(page.locator("#delta-outlier-body")).toBeHidden();
  await page.locator('.view-tab[data-view="tree"]').click();

  await page.locator('#drawer .kv-link[data-action="delta"]').click();

  await expect(page.locator("#delta-outlier-body")).toBeVisible();
  await expect(page.locator("#delta-outliers-toggle")).toHaveAttribute("aria-expanded", "true");
  await expect(page.locator(".selection-note")).toBeVisible();
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("an uppercase selected hash still matches the competition", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?view=delta&selected=${STALE.toUpperCase()}`);
  await expect(page.locator("#delta-main")).toBeVisible();

  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  await expect(page.locator(".selection-note")).toBeVisible();
});

test("collapsing the list gives its width to the plot", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  const open = await page.locator("#delta-canvas").evaluate((n) => n.getBoundingClientRect().width);

  await page.locator("#delta-outliers-toggle").click();
  await expect(page.locator("#delta-outlier-body")).toBeHidden();

  const collapsed = await page.locator("#delta-canvas").evaluate((n) => n.getBoundingClientRect().width);
  expect(collapsed).toBeGreaterThan(open + 50);
});
