const { expect, test } = require("@playwright/test");
const {
  GENERATED_AT,
  blockCompetition,
  blockPayload,
  competitionsPayload,
  makeCompetition,
  makeNode,
  moduleUrl,
  sourcesPayload,
  stubApi,
  treeEnvelope,
} = require("./support/api-stubs");

// The two directions between the tree and the distribution: a block's Header
// Time Delta opens the distribution focused on that competition, and a
// competition the filters hide can still be revealed or opened in the tree.

const STALE = "a".repeat(64);
const OTHER = "b".repeat(64);

const SOURCE_BASE = {
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
  counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
};

const sources = () => sourcesPayload([
  { ...SOURCE_BASE, id: 1, code: "auxpow:namecoin", chain: "namecoin" },
  { ...SOURCE_BASE, id: 2, code: "auxpow:rsk", chain: "rsk" },
]);

/// A tree holding the stale block, plus the competitions and block payloads that
/// go with it. `delta` is the stale block's own header time delta.
function scenario({ delta = 90000, rows = null, competitionOverrides = {} } = {}) {
  const nodes = [
    makeNode(STALE, 700000, null, "stale", { id: 1, prev_id: null }),
    makeNode(OTHER, 700001, STALE, "canonical", { id: 2, prev_id: 1 }),
  ];
  return {
    sourcesPayload: sources,
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
  // The selection has to survive the crossing, or the focused state has nothing
  // to focus on.
  expect(params.get("selected")).toBe(STALE);
  await expect.poll(() => competitionRequests.length).toBeGreaterThan(0);

  // Focused state: marked on the strip, and named in the outlier panel because
  // +90000s sits far outside the default window.
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("a null delta is not a cross-link", async ({ page }) => {
  // A null delta has no symlog position, no bin and no outlier row, so linking
  // would land on a view that cannot place it.
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

  // Two bars are populated: the other row's +3s in the zero bin, and the
  // selection's +12s in the bin centred on +10. Exactly the latter is marked.
  const bars = await page.locator("#delta-chart .bar-mark").evaluateAll(
    (nodes) => nodes.map((node) => node.getAttribute("class")),
  );
  expect(bars.length).toBe(2);
  expect(bars.filter((cls) => cls.includes("is-selected"))).toEqual([
    expect.stringContaining("bar-pos"),
  ]);
  expect(bars.find((cls) => !cls.includes("is-selected"))).toContain("bar-zero");
  // And the marker is announced, not merely drawn.
  await page.locator("#delta-chart .bar-hit").last().hover();
  await expect(page.locator("#delta-tooltip")).toContainText("holds the selected competition");
  // The strip marks the selection REGARDLESS of the window, so an in-window one
  // is marked there too. Its per-record rug covers only excluded rows, so an
  // in-window selection has no tick of its own and needs the explicit marker.
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
});

test("a selection outside the window marks no bin", async ({ page }) => {
  // Past the edge the selection belongs to a gutter and an outlier row, so
  // marking the nearest bin would claim a membership it does not have. The third
  // row exists to POPULATE the outermost bin: without a bar there, a clamped
  // lookup would have nothing to mis-mark and this would pass vacuously.
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
  // Hidden means excluded from every count, not merely unmarked.
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(0);

  await page.locator(".hidden-selection [data-action='reveal']").click();

  await expect(page.locator(".hidden-selection")).toHaveCount(0);
  await expect(page.locator('input[name="source"][value="auxpow:rsk"]')).not.toBeChecked();
  expect(new URL(page.url()).searchParams.get("sources")).toBeNull();
  // And it is counted again: +90000s is an outlier, so it lands in the panel.
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("revealing an era-hidden selection leaves the Source filter alone", async ({ page }) => {
  // Clearing both filters would throw away a narrowing the user made for their
  // own reasons; only the filter actually hiding the row is cleared.
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
  // The Source filter was not the culprit, so it survives.
  await expect(page.locator('input[name="source"][value="auxpow:rsk"]')).toBeChecked();
  expect(new URL(page.url()).searchParams.get("sources")).toBe("auxpow:rsk");
});

test("a cold link restores the view, the selection and the open drawer", async ({ page }) => {
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?view=delta&selected=${STALE}`);

  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "delta");
  // The drawer opens for a selection regardless of the stored collapse state.
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");
  await expect(page.locator("#drawer")).toContainText("Competition");
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("the focused outlier row is scrolled into view", async ({ page }) => {
  // The list is sorted by magnitude, so a cross-linked selection that is only
  // mildly extreme sits well below the fold; aria-current alone is invisible to
  // a sighted user there.
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

  // Assert against the SCROLLER's visible box, and prove it is a scroller: the
  // earlier version of this test compared the row to the list element's own box,
  // which is trivially true when that element grows to its content instead of
  // scrolling. That is exactly the layout defect this panel used to have.
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
  // The panel used to be a <details>, whose content Chromium slots into an
  // anonymous box that the row template never reached: the list grew to its full
  // content height and the panel clipped it, so most rows were unreachable.
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
  // The panel itself must not overflow: that is the clipping this replaced.
  expect(open.panelClips).toBe(false);

  const toggle = page.locator("#delta-outliers-toggle");
  await expect(toggle).toHaveAttribute("aria-expanded", "true");
  await toggle.click();
  await expect(toggle).toHaveAttribute("aria-expanded", "false");
  await expect(page.locator("#delta-outlier-body")).toBeHidden();
  // Collapsing has to return the height, not leave a gap where the list was.
  const collapsedRows = await page.locator("#delta-outliers").evaluate(
    (panel) => getComputedStyle(panel).gridTemplateRows,
  );
  expect(collapsedRows.split(" ").length).toBe(1);
});

test("the outlier cap never drops the focused competition", async ({ page }) => {
  // The cap is by magnitude and a cross-linked selection need not be among the
  // largest. Dropped, the focused state would have no row to flag or scroll to.
  const rows = [
    makeCompetition(STALE, 700000, 3600),
    ...Array.from({ length: 260 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();

  // 260 rows outrank it, so the 250-row cap would have excluded it entirely.
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(251);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  // And the note says what was actually shown, rather than implying 251 largest.
  await expect(page.locator("#delta-outlier-body .sort-note"))
    .toContainText("250 largest of 261, plus the selected competition");
});

test("a selection that becomes an outlier is scrolled to then", async ({ page }) => {
  // The selection starts in-window, so there is no row to scroll to and nothing
  // to remember. Narrowing the window makes it an outlier, and that transition
  // must still scroll rather than being suppressed by a scroll that never was.
  const rows = [
    makeCompetition(STALE, 700000, 45),
    ...Array.from({ length: 40 }, (_, i) => makeCompetition(
      String(i).padStart(64, "1"), 700100 + i, 100000 + i * 1000,
    )),
  ];
  await stubApi(page, [], scenario({ rows }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  // In-window at the default +/-2m: flagged nowhere in the list.
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
  // Competitions load once while the tree refreshes on a timer, so a stale block
  // discovered after the first load is in the drawer but not in the snapshot.
  // Crossing over on that cached snapshot would show nothing focused.
  let competitionRows = [makeCompetition(OTHER, 700001, 3)];
  const requests = [];
  await stubApi(page, [], {
    ...scenario(),
    competitionsPayload: () => competitionsPayload(competitionRows),
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) requests.push(request.url());
  });

  // Warm the snapshot without the stale block in it.
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect.poll(() => requests.length).toBe(1);

  // It shows up later, and the drawer can render its cross-link.
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
  // The first scroll must not suppress the second: hiding the row and bringing
  // it back rebuilds the list at scroll position zero.
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

  // Hide it, then bring it back.
  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await expect(page.locator(".hidden-selection")).toBeVisible();
  await page.locator('input[name="source"][value="auxpow:rsk"]').uncheck();

  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);
});

test("opening the collapsed list scrolls to the focused row", async ({ page }) => {
  // A collapsed list cannot scroll, and remembering that no-op would suppress
  // the scroll owed once it opens. The selection starts in-window, so no row
  // exists either; narrowing the window puts it in the hidden list.
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
  // Now it becomes an outlier, with the list hidden and unable to scroll.
  await page.locator('#delta-presets [data-half="30"]').click();

  await page.locator("#delta-outliers-toggle").click();
  await expect(page.locator("#delta-outlier-body")).toBeVisible();
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);
});

test("reveal still clears a Source filter when the rail failed to load", async ({ page }) => {
  // A shared link can carry sources= while /api/v1/sources fails, so the rail
  // renders no checkboxes. The advertised button must not be inert.
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
  // The route back to the tree is the counterpart of the drawer's link into this
  // view, so an ordinary visible outlier needs it too, not only one the filters
  // hide. It must retarget the tree window, not merely switch tabs.
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  // No filter is active, so this is the plain visible case.
  await expect(page.locator(".hidden-selection")).toHaveCount(0);
  await expect(page.locator(".selection-note")).toBeVisible();

  await page.locator(".selection-note [data-action='tree']").click();

  await expect(page.locator(".tree-card")).toBeVisible();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  // Retargeted at the competition's height, with the sibling lookup left clear.
  await expect(page.locator('input[name="treeHeight"]')).toHaveValue("700000");
  await expect(page.locator('input[name="treeTime"]')).toHaveValue("");
});

test("a failed repair refetch stays retryable", async ({ page }) => {
  // Recording the attempt before it succeeds would leave the competition
  // unfocused until a manual Refresh, because every later activation would take
  // the early return on a hash that never actually arrived.
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

  // Select a competition the snapshot lacks, with the repair request failing.
  fail = true;
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.evaluate(async ([hash, api_url]) => {
    const api = await import(api_url);
    await api.loadBlock(hash);
  }, [STALE, moduleUrl("api-client.js")]);
  await page.locator('#drawer .kv-link[data-action="delta"]').click();
  await expect.poll(() => requests.length).toBe(2);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(0);

  // Going back and returning must try again rather than give up on the hash.
  fail = false;
  rows = [makeCompetition(STALE, 700000, 90000), makeCompetition(OTHER, 700001, 3)];
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.locator('.view-tab[data-view="delta"]').click();

  await expect.poll(() => requests.length).toBe(3);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("a snapshot repair does not wait on the block detail request", async ({ page }) => {
  // Selecting a newly discovered stale block and crossing over before its
  // /block request lands used to read as "this block has no competition" and
  // skip the repair, with nothing to retry once the detail arrived.
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
  // Hold the block detail open, so the cross-over happens with it still in
  // flight and state.selectedBlock still null.
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
  // A delayed load can finish rendering the distribution after the user has gone
  // back to the tree. The list is not hidden then, but an ancestor is, so
  // scrollIntoView is a no-op that must not be remembered as done.
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
  // Cross over, then leave again before the competitions request resolves.
  await page.locator('.view-tab[data-view="delta"]').click();
  await page.locator('.view-tab[data-view="tree"]').click();
  release();
  // The delta render now lands while the tree owns the workspace.
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
  // activateView also runs on Source-filter changes, so treating "loaded, and it
  // has no competition" as unknown would re-request the whole unpaginated
  // endpoint every time a canonical block happened to be selected.
  const requests = [];
  await stubApi(page, [], {
    ...scenario(),
    // A canonical selection: present in the tree, absent from competitions.
    blockPayload: (hash) => blockPayload(hash, { competition: null }),
    competitionsPayload: () => competitionsPayload([makeCompetition(OTHER, 700001, 3)]),
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) requests.push(request.url());
  });

  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#delta-main")).toBeVisible();
  // Wait for the block detail to actually land before counting: recording while
  // it is still unknown would let the guard alone carry the test and never
  // exercise the conclusive no-competition branch. "Parent block" proves the
  // payload arrived, and the absent Competition section proves it has none.
  await expect(page.locator("#drawer")).toContainText("Parent block");
  await expect(page.locator("#drawer")).not.toContainText("Competition");
  const afterLoad = requests.length;
  expect(afterLoad).toBeGreaterThan(0);

  // Now the detail has confirmed there is no competition. Neither leaving and
  // returning nor a filter change may refetch.
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await page.locator('input[name="source"][value="auxpow:rsk"]').uncheck();

  expect(requests.length).toBe(afterLoad);
});

test("following the cross-link again scrolls to the row again", async ({ page }) => {
  // The cache stops the panel fighting a user reading down the list, but an
  // explicit cross-link is a fresh gesture: after scrolling away, following it
  // again must bring the advertised row back into view.
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

  // The user scrolls back to the top and leaves.
  await page.locator("#delta-outlier-body").evaluate((n) => { n.scrollTop = 0; });
  await page.locator('.view-tab[data-view="tree"]').click();

  await page.locator('#drawer .kv-link[data-action="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect.poll(
    () => page.locator("#delta-outlier-body").evaluate((n) => n.scrollTop),
  ).toBeGreaterThan(0);
});

test("a 200 that still lacks the row is retried, not given up on", async ({ page }) => {
  // The read model can lag the block detail: the endpoint answers 200 without
  // the competition yet. Retiring the repair on a bare successful response would
  // leave the focused state missing until a manual Refresh.
  let rows = [makeCompetition(OTHER, 700001, 3)];
  const requests = [];
  await stubApi(page, [], {
    ...scenario(),
    competitionsPayload: () => competitionsPayload(rows),
  });
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) requests.push(request.url());
  });

  // The drawer knows about the competition; the snapshot does not yet.
  await page.goto(`/?view=delta&selected=${STALE}`);
  await expect(page.locator("#drawer")).toContainText("Competition");
  await expect.poll(() => requests.length).toBeGreaterThan(0);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(0);
  const afterFirst = requests.length;

  // The read model catches up. Returning must try again for the same hash.
  rows = [makeCompetition(STALE, 700000, 90000), makeCompetition(OTHER, 700001, 3)];
  await page.locator('.view-tab[data-view="tree"]').click();
  await page.locator('.view-tab[data-view="delta"]').click();

  await expect.poll(() => requests.length).toBeGreaterThan(afterFirst);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
});

test("the cross-link opens the panel it focuses into", async ({ page }) => {
  // Following the link promises to show the competition. With the disclosure
  // left collapsed, the focused row, the selection notice, the reveal action and
  // the tree link are all behind it.
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?selected=${STALE}`);
  await expect(page.locator("#drawer")).toContainText("Competition");

  // Collapse it while in the view, then leave. No reload, which would reset the
  // disclosure and hide what this is testing.
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
  // The API accepts uppercase hex but every payload comes back lowercase, and
  // the selection is compared by string equality throughout. Keeping the URL's
  // spelling loaded the drawer and matched nothing else.
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto(`/?view=delta&selected=${STALE.toUpperCase()}`);
  await expect(page.locator("#delta-main")).toBeVisible();

  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  await expect(page.locator("#delta-outlier-body .outlier-row[aria-current='true']")).toHaveCount(1);
  await expect(page.locator(".selection-note")).toBeVisible();
});

test("collapsing the list gives its width to the plot", async ({ page }) => {
  // The toggle re-renders the charts so they can use the width, which is wasted
  // if the grid still reserves the outlier column.
  await stubApi(page, [], scenario({ delta: 90000 }));
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  const open = await page.locator("#delta-canvas").evaluate((n) => n.getBoundingClientRect().width);

  await page.locator("#delta-outliers-toggle").click();
  await expect(page.locator("#delta-outlier-body")).toBeHidden();

  const collapsed = await page.locator("#delta-canvas").evaluate((n) => n.getBoundingClientRect().width);
  // The collapsed panel keeps its header strip, so the plot gains the list's
  // width rather than the whole column.
  expect(collapsed).toBeGreaterThan(open + 50);
});
