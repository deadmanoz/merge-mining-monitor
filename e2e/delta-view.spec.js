const { expect, test } = require("@playwright/test");
const {
  GENERATED_AT,
  competitionsPayload,
  liveSourcesPayload,
  makeCompetition,
  makeNode,
  moduleUrl,
  stubApi,
  treeEnvelope,
} = require("./support/api-stubs");

const STATE_MODULE = moduleUrl("frontend-state.js");

// Two eras so the Era control has something to narrow to, and two sources so
// the shared Source filter has something to select.
const Y2013 = 1_370_000_000;
const Y2024 = 1_710_000_000;

// A fixed set covering every branch the view has to get right: in-window rows
// both sides of zero, one far outlier, and one with no derivable delta.
const COMPETITIONS = [
  makeCompetition("a".repeat(64), 700000, -5, { stale_header_time: Y2013 }),
  makeCompetition("b".repeat(64), 700001, 12, { stale_header_time: Y2013 }),
  makeCompetition("c".repeat(64), 700002, 0, { stale_header_time: Y2024 }),
  makeCompetition("d".repeat(64), 700003, 90000, { stale_header_time: Y2024, sources: ["auxpow:rsk"] }),
  makeCompetition("e".repeat(64), 700004, null, { stale_header_time: Y2024 }),
];

const withCompetitions = (rows = COMPETITIONS) => ({
  competitionsPayload: () => competitionsPayload(rows),
  sourcesPayload: liveSourcesPayload,
});

async function openDelta(page, rows) {
  await stubApi(page, [], withCompetitions(rows));
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(page.locator("#delta-stats .stat-tile").first()).toBeVisible();
}

const statValues = (page) => page.locator("#delta-stats .stat-value").allTextContents();

test("the distribution renders from the competitions endpoint", async ({ page }) => {
  await openDelta(page);

  // Four of the five rows have a usable delta; the fifth is reported, never
  // counted.
  const values = await statValues(page);
  expect(values[0]).toBe("4");
  await expect(page.locator("#delta-stats")).toContainText("1 delta unavailable");

  await expect(page.locator("#delta-chart .bar-mark").first()).toBeVisible();
  await expect(page.locator("#delta-context .bar-mark").first()).toBeVisible();
});

test("a null delta never lands in the zero bin", async ({ page }) => {
  // One genuine zero and one null. JavaScript coerces null to 0, so a naive
  // port would report two tied competitions here.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 0),
    makeCompetition("b".repeat(64), 700001, null),
  ]);

  await page.locator('.delta-tab[data-tab="table"]').click();
  const zeroRow = page.locator("#delta-table-body tr", { hasText: "0s" }).first();
  await expect(zeroRow).toContainText("1");
  await expect(page.locator("#delta-stats")).toContainText("1 delta unavailable");
});

test("the off-scale gutter and outlier list carry what the window excludes", async ({ page }) => {
  await openDelta(page);

  // The 90000s row sits far outside the default window.
  await expect(page.locator("#delta-outlier-count")).toHaveText("1");
  const outlier = page.locator("#delta-outlier-body .outlier-row");
  await expect(outlier).toHaveCount(1);
  await expect(outlier).toContainText("+1d");
  // It is also ticked on the full-range strip.
  await expect(page.locator("#delta-context .rug-tick")).toHaveCount(1);
});

test("selecting an outlier opens the shared block drawer", async ({ page }) => {
  await openDelta(page);

  await page.locator("#delta-outlier-body .outlier-row").first().click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");
  await expect(page.locator("#drawer")).toContainText("Parent block");
  await expect(page.locator("#delta-outlier-body [aria-current='true']")).toHaveCount(1);
  // The strip marks the selection, and the URL carries it for sharing.
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(1);
  expect(new URL(page.url()).searchParams.get("selected")).toBe("d".repeat(64));
});

test("the shared Source filter re-renders the distribution", async ({ page }) => {
  await openDelta(page);
  expect((await statValues(page))[0]).toBe("4");

  // Only the far outlier carries auxpow:rsk, so selecting it should leave one
  // row: the same filter that merely dims nodes in the tree.
  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await expect(page.locator("#delta-stats .stat-value").first()).toHaveText("1");
  await expect(page.locator("#delta-meta")).toContainText("1 source");

  await page.locator('input[name="source"][value="auxpow:rsk"]').uncheck();
  await expect(page.locator("#delta-stats .stat-value").first()).toHaveText("4");
});

test("the era filter narrows the set and round-trips through the URL", async ({ page }) => {
  await openDelta(page);
  expect((await statValues(page))[0]).toBe("4");

  // 2024 onward drops the two 2013 rows, leaving the zero and the outlier (the
  // null-delta row is never counted).
  await page.locator("#delta-year-from").selectOption("2024");
  await expect(page.locator("#delta-stats .stat-value").first()).toHaveText("2");
  expect(new URL(page.url()).searchParams.get("era")).toBe("2024-2024");

  // Back to the full range: the parameter clears rather than pinning defaults.
  await page.locator("#delta-year-from").selectOption("2013");
  await expect(page.locator("#delta-stats .stat-value").first()).toHaveText("4");
  expect(new URL(page.url()).searchParams.get("era")).toBeNull();
});

test("the focus window presets re-bin the histogram", async ({ page }) => {
  await openDelta(page);
  const wide = await page.locator("#delta-chart .bar-mark").count();

  await page.locator('#delta-presets [data-half="10"]').click();
  await expect(page.locator("#delta-meta")).toContainText("±10s");
  await expect(page.locator('#delta-presets [data-half="10"]')).toHaveAttribute("aria-pressed", "true");
  const narrow = await page.locator("#delta-chart .bar-mark").count();
  expect(narrow).not.toBe(wide);
});

test("the tabs switch between chart and table", async ({ page }) => {
  await openDelta(page);

  await page.locator('.delta-tab[data-tab="coverage"]').click();
  await expect(page.locator("#delta-chart .coverage-line")).toHaveCount(1);
  await expect(page.locator("#delta-legend")).toBeHidden();

  await page.locator('.delta-tab[data-tab="table"]').click();
  await expect(page.locator("#delta-canvas")).toBeHidden();
  await expect(page.locator("#delta-table-body tr").first()).toBeVisible();

  await page.locator('.delta-tab[data-tab="histogram"]').click();
  await expect(page.locator("#delta-canvas")).toBeVisible();
  await expect(page.locator("#delta-legend")).toBeVisible();
});

test("the metric explainer opens from the view title", async ({ page }) => {
  await openDelta(page);

  await page.locator("#delta-about").click();
  await expect(page.locator("#delta-dialog")).toBeVisible();
  await expect(page.locator("#delta-dialog-title")).toHaveText("Header time delta");
  await expect(page.locator("#delta-dialog-body p")).not.toHaveCount(0);
});

test("the auto-refresh timer never refetches competitions", async ({ page }) => {
  // The timer is a fixed 60s and the whole test timeout is 30s, so a real tick
  // cannot complete; drive a fake clock instead. Both endpoints are counted,
  // because a scheduled tick must still refresh sources.
  await page.clock.install();
  let competitions = 0;
  let sources = 0;
  await stubApi(page, [], withCompetitions());
  page.on("request", (request) => {
    if (request.url().includes("/api/v1/competitions")) competitions += 1;
    if (request.url().includes("/api/v1/sources")) sources += 1;
  });

  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  const afterLoad = { competitions, sources };

  await page.clock.runFor("01:01");
  // Wait for the tick to actually reach the network before judging it: the
  // sources request proves the scheduled refresh ran at all, so a competitions
  // count that has not moved by then is a real absence, not a race.
  await expect.poll(() => sources).toBeGreaterThan(afterLoad.sources);
  expect(competitions).toBe(afterLoad.competitions);

  // The Updated stamp, by contrast, does refetch the active view.
  await page.locator("#last-updated").click();
  await expect.poll(() => competitions).toBeGreaterThan(afterLoad.competitions);
});

test("a fine bin width over a wide window stays bounded", async ({ page }) => {
  // Full range at one-second bins is over thirteen million bins if taken
  // literally, which allocates for long enough to hang the tab. The width must
  // widen to fit, and the meta line must report what was actually applied.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 0),
    makeCompetition("b".repeat(64), 700001, 6_000_000),
  ]);

  await page.locator('#delta-presets [data-half=""]').click();
  await page.locator("#delta-bin-width").selectOption("1");

  await expect(page.locator("#delta-chart .bar-mark")).not.toHaveCount(0);
  const bins = await page.locator("#delta-chart .bar-hit").count();
  expect(bins).toBeLessThan(500);
  // The applied width, not the requested 1s.
  await expect(page.locator("#delta-meta")).not.toContainText("1s bins");
});

test("the labelled window is the window that decides membership", async ({ page }) => {
  // Bin edges snap to the grid, so a width that does not divide the requested
  // half moves the real edge. Labelling with the request would shade a counted
  // record as outside. +20s must be inside whatever ±N the meta line claims.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 0),
    makeCompetition("b".repeat(64), 700001, 20),
  ]);

  await page.locator('#delta-presets [data-half="10"]').click();
  await page.locator("#delta-bin-width").selectOption("60");

  const meta = await page.locator("#delta-meta").textContent();
  const labelled = /window ±(\d+)s/.exec(meta);
  expect(labelled).not.toBeNull();
  const half = Number(labelled[1]);
  const outside = Number(await page.locator("#delta-outlier-count").textContent() || 0);
  // Exactly the records beyond the labelled window are listed as outside.
  expect(outside).toBe(20 > half ? 1 : 0);
});

test("deselecting clears the block detail, not just the URL", async ({ page }) => {
  await openDelta(page);
  const row = page.locator("#delta-outlier-body .outlier-row").first();

  await row.click();
  await expect(page.locator("#drawer")).toContainText("Parent block");
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");

  await page.locator("#delta-outlier-body .outlier-row").first().click();
  expect(new URL(page.url()).searchParams.get("selected")).toBeNull();
  // Stale detail must not survive the deselection.
  await expect(page.locator("#drawer")).not.toContainText("Parent block");
  await expect(page.locator("#delta-outlier-body [aria-current='true']")).toHaveCount(0);
  await expect(page.locator("#delta-context .rug-tick.is-selected")).toHaveCount(0);
  // And the width selecting took has to come back, rather than leaving the plot
  // narrowed by a column reading "No block selected".
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "true");
});

test("an endpoint failure is not reported as an empty filter", async ({ page }) => {
  await stubApi(page, [], withCompetitions());
  await page.route("**/api/v1/competitions", (route) => route.fulfill({
    status: 500,
    json: { schema_version: "v1", generated_at: GENERATED_AT, error: { code: "internal_error", message: "boom" } },
  }));
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();

  // Blaming the user's filters for an outage is the failure mode here.
  await expect(page.locator("#delta-status")).toContainText("could not be loaded");
  await expect(page.locator("#delta-status")).not.toContainText("No competitions match");
});

test("a filter matching only unavailable deltas says so", async ({ page }) => {
  await openDelta(page, [makeCompetition("a".repeat(64), 700000, null)]);

  await expect(page.locator("#delta-status")).toContainText("no");
  await expect(page.locator("#delta-status")).toContainText("derivable header time delta");
});

test("the strip stays usable when a filter leaves a single one-sided record", async ({ page }) => {
  // With one positive row the data-only domain collapses: both ends pad the
  // same way, putting zero and both brush handles off-canvas.
  await openDelta(page, [makeCompetition("a".repeat(64), 700000, 45, { sources: ["auxpow:rsk"] })]);
  await page.locator('input[name="source"][value="auxpow:rsk"]').check();

  const box = await page.locator("#delta-context").boundingBox();
  const handles = page.locator("#delta-context .brush-handle");
  await expect(handles).toHaveCount(2);
  for (const handle of await handles.all()) {
    const hb = await handle.boundingBox();
    expect(hb.x).toBeGreaterThanOrEqual(box.x - 1);
    expect(hb.x).toBeLessThanOrEqual(box.x + box.width + 1);
  }
});

test("an unrepresentable timestamp does not abort the render", async ({ page }) => {
  // stale_header_time is unbounded in the contract, and toISOString throws
  // outside the Date range. One such row reaching an outlier label used to take
  // the whole view down.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 5),
    makeCompetition("b".repeat(64), 700001, 90000, { stale_header_time: 9_000_000_000_000 }),
  ]);

  await expect(page.locator("#delta-chart .bar-mark").first()).toBeVisible();
  const outlier = page.locator("#delta-outlier-body .outlier-row");
  await expect(outlier).toHaveCount(1);
  await expect(outlier).toContainText("date unavailable");
});

test("no bin is rendered outside the declared window", async ({ page }) => {
  // half=30 with 60s bins used to centre a bin on +60: outside the window, drawn
  // off-plot, and reported as a degenerate 30 … 30 range in the table.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 0),
    makeCompetition("b".repeat(64), 700001, 30),
  ]);
  await page.locator('#delta-presets [data-half="30"]').click();
  await page.locator("#delta-bin-width").selectOption("60");

  await page.locator('.delta-tab[data-tab="table"]').click();
  const ranges = await page.locator("#delta-table-body tr td:nth-child(2)").allTextContents();
  for (const range of ranges) {
    const [lo, hi] = range.split("…").map((part) => Number(part.replace(/[^\d.-]/g, "")));
    expect(lo).toBeGreaterThanOrEqual(-30);
    expect(hi).toBeLessThanOrEqual(30);
    expect(lo).not.toBe(hi);
  }
  // The +30 record is inside the window, so nothing is listed as an outlier.
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(0);
});

test("a hidden unavailable-delta selection is not shown as a tie", async ({ page }) => {
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 5, { sources: ["auxpow:namecoin"] }),
    makeCompetition("b".repeat(64), 700001, null, { sources: ["auxpow:rsk"] }),
  ]);

  // Select the unavailable-delta row, then filter it out.
  await page.evaluate(async (stateModule) => {
    const state = (await import(stateModule)).state;
    state.selectedHash = "b".repeat(64);
  }, STATE_MODULE);
  await page.locator('input[name="source"][value="auxpow:namecoin"]').check();

  const notice = page.locator(".hidden-selection");
  await expect(notice).toContainText("delta unavailable");
  await expect(notice).not.toContainText("0s");
});

test("an outermost bin reports the range it actually counts", async ({ page }) => {
  // half=25 with a 30s width leaves one bin. A +25 record is inside the window
  // and clamped into it, so that bin must report reaching the window edge
  // rather than the nominal -15 … 15.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 0),
    makeCompetition("b".repeat(64), 700001, 25),
  ]);
  await page.locator('#delta-presets [data-half="30"]').click();
  await page.locator("#delta-bin-width").selectOption("30");
  await page.locator('.delta-tab[data-tab="table"]').click();

  const ranges = await page.locator("#delta-table-body tr td:nth-child(2)").allTextContents();
  const bounds = ranges.map((range) => range.split("…").map((part) => Number(part.replace(/[^\d.-]/g, ""))));
  // Every counted record falls inside some reported range.
  expect(bounds.some(([lo, hi]) => lo <= 25 && hi >= 25)).toBe(true);
  await expect(page.locator("#delta-outlier-body .outlier-row")).toHaveCount(0);
});

test("a share short of the whole is never shown as 100%", async ({ page }) => {
  // 200 in-window records and one outlier: 99.5% rounds to "100%" under a naive
  // formatter while the outlier panel simultaneously lists the exclusion.
  const rows = Array.from({ length: 200 }, (_, i) =>
    makeCompetition(String(i).padStart(64, "0"), 700000 + i, (i % 20) - 10));
  rows.push(makeCompetition("f".repeat(64), 800000, 500000));
  await openDelta(page, rows);

  const inWindow = (await statValues(page))[3];
  expect(inWindow).not.toBe("100%");
  await expect(page.locator("#delta-outlier-count")).toHaveText("1");
});

test("the median of an even sample is the midpoint", async ({ page }) => {
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, -5),
    makeCompetition("b".repeat(64), 700001, 12),
  ]);
  // Nearest-index would report +12 here, biasing the headline to the upper
  // observation; the conventional median is 3.5, which formats as +4s.
  expect((await statValues(page))[1]).toBe("+4s");
});

test("deselecting clears selection-derived navigator state", async ({ page }) => {
  await openDelta(page);

  await page.locator("#delta-outlier-body .outlier-row").first().click();
  await expect(page.locator("#drawer")).toContainText("Parent block");
  const derived = await page.evaluate(
    async (stateModule) => (await import(stateModule)).state.nav,
    STATE_MODULE,
  );

  await page.locator("#delta-outlier-body .outlier-row").first().click();
  const cleared = await page.evaluate(
    async (stateModule) => (await import(stateModule)).state.nav,
    STATE_MODULE,
  );
  // A target the selection produced must not outlive it.
  if (derived.source === "selection") expect(cleared.target).toBe("tip");

  // And the tree it returns to shows no stale stepping readout.
  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".tree-card")).toBeVisible();
  expect(await page.locator("#nav-readout").textContent()).toBe("");
});

test("an absurd timestamp cannot enumerate the era control", async ({ page }) => {
  // A representable but implausible year (255,000 is only 8e12 seconds out)
  // would otherwise render hundreds of thousands of option nodes.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 5, { stale_header_time: Y2024 }),
    makeCompetition("b".repeat(64), 700001, 7, { stale_header_time: 8_000_000_000_000 }),
  ]);

  const options = await page.locator("#delta-year-from option").count();
  expect(options).toBeLessThan(50);
  // The implausible row is still counted; only its era is unknown.
  expect((await statValues(page))[0]).toBe("2");
});

test("a clamped era is written back to the URL", async ({ page }) => {
  await stubApi(page, [], withCompetitions());
  // Asks for an era entirely before the data.
  await page.goto("/?view=delta&era=2001-2002");
  await expect(page.locator("#delta-main")).toBeVisible();

  // The address bar must describe what is on screen, not what was requested.
  const era = new URL(page.url()).searchParams.get("era");
  const from = await page.locator("#delta-year-from").inputValue();
  const to = await page.locator("#delta-year-to").inputValue();
  expect(era === null || era === `${from}-${to}`).toBe(true);
  expect(era).not.toBe("2001-2002");
});

test("Show in tree leaves only the Height lookup populated", async ({ page }) => {
  await stubApi(page, [], withCompetitions());
  // Arrive with a Date/Time lookup already committed, so there is a stale
  // sibling value for the cross-link to clear.
  await page.goto("/?tree_time=2021-09-01T00%3A00%3A00Z");
  await expect(page.locator(".tree-card")).toBeVisible();
  await expect(page.locator('input[name="treeTime"]')).not.toHaveValue("");

  // "Show in tree" lives in the hidden-selection notice, so build that state:
  // select the rsk-only row, then filter to namecoin so it is excluded.
  await page.locator('.view-tab[data-view="delta"]').click();
  await page.locator("#delta-outlier-body .outlier-row").first().click();
  await page.locator('input[name="source"][value="auxpow:namecoin"]').check();
  await expect(page.locator(".hidden-selection")).toBeVisible();

  await page.locator(".hidden-selection [data-action='tree']").click();
  await expect(page.locator(".tree-card")).toBeVisible();

  // Both fields populated would block a later Height commit, which refuses when
  // the sibling lookup is non-empty.
  await expect(page.locator('input[name="treeHeight"]')).not.toHaveValue("");
  await expect(page.locator('input[name="treeTime"]')).toHaveValue("");
});

test("coverage dots sit on the curve when deltas repeat", async ({ page }) => {
  // Every delta equal: the curve reaches 100% at that x, so dots pinned to the
  // nominal 50/90/99% would float below their own line.
  await openDelta(page, Array.from({ length: 6 }, (_, i) =>
    makeCompetition(String(i).padStart(64, "0"), 700000 + i, 8)));
  await page.locator('.delta-tab[data-tab="coverage"]').click();

  const dots = await page.locator("#delta-chart .coverage-dot").evaluateAll(
    (nodes) => nodes.map((node) => Number(node.getAttribute("cy"))),
  );
  expect(dots.length).toBe(3);
  // All three thresholds reach the same coverage, so all three dots coincide.
  expect(Math.max(...dots) - Math.min(...dots)).toBeLessThan(1);
});

test("bin ties fall the same way on both sides of zero", async ({ page }) => {
  // Math.round breaks ties toward +infinity, so at a 10s width it put -5 in the
  // zero bin and +5 in the positive one: the same magnitude counted as a tie on
  // one side and a lead on the other.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, -5),
    makeCompetition("b".repeat(64), 700001, 5),
  ]);
  await page.locator('#delta-presets [data-half="30"]').click();
  await page.locator("#delta-bin-width").selectOption("10");
  await page.locator('.delta-tab[data-tab="table"]').click();

  const rows = await page.locator("#delta-table-body tr").evaluateAll((nodes) => nodes.map((node) => ({
    centre: node.children[0].textContent,
    count: Number(node.children[2].textContent),
  })));
  expect(rows.map((row) => row.count)).toEqual([1, 1]);
  // Mirrored centres, and neither of them zero.
  const centres = rows.map((row) => Number(row.centre.replace(/[^\d.-]/g, "").replace(/^−/, "-")));
  expect(centres[0]).toBeLessThan(0);
  expect(centres[1]).toBe(-centres[0]);
});

const UNIT_SECONDS = { s: 1, m: 60, h: 3600, d: 86400 };

/// The half-width the meta line reports, in seconds.
async function windowHalf(page) {
  const meta = await page.locator("#delta-meta").textContent();
  const [, value, unit] = meta.match(/window ±([\d.]+)([smhd])/);
  return Number(value) * UNIT_SECONDS[unit];
}

test("widening past the data extent does not collapse the window", async ({ page }) => {
  // The ceiling used to be the data extent alone, so a widening keypress on a
  // window already wider than the data snapped it inward: one +45s record under
  // a ±2m window collapsed to ±45s, and an empty filter collapsed to ±1s.
  // Widening past the extent is a no-op (Full already reaches it); collapsing
  // is not.
  await openDelta(page, [makeCompetition("a".repeat(64), 700000, 45)]);
  await page.locator('#delta-presets [data-half="120"]').click();
  expect(await windowHalf(page)).toBe(120);

  const handle = page.locator("#delta-context .brush-handle").last();
  await handle.focus();
  await page.keyboard.press("ArrowRight");
  expect(await windowHalf(page)).toBe(120);

  // The same handle still narrows, so the assertion above is not vacuous.
  await handle.focus();
  await page.keyboard.press("ArrowLeft");
  expect(await windowHalf(page)).toBeLessThan(120);
});

test("the table reports an outage as an outage", async ({ page }) => {
  await stubApi(page, [], withCompetitions());
  await page.route("**/api/v1/competitions", (route) => route.fulfill({
    status: 500,
    json: { schema_version: "v1", generated_at: GENERATED_AT, error: { code: "internal_error", message: "boom" } },
  }));
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator('.delta-tab[data-tab="table"]').click();

  // The accessible twin of the chart has to say the same thing the status line
  // says, rather than blaming the user's filters for a failed load.
  await expect(page.locator("#delta-table-body")).toContainText("could not be loaded");
  await expect(page.locator("#delta-table-body")).not.toContainText("No competitions match");
});

test("returning to the tree after a skipped refresh reloads it", async ({ page }) => {
  // The scheduled refresh skips the tree while another view is active, and the
  // tree loader skips when a tree is already cached and not dirty. Together
  // they froze the tree for as long as the tab stayed open.
  const treeRequests = [];
  await stubApi(page, treeRequests, withCompetitions());
  await page.clock.install();
  await page.goto("/");
  await expect(page.locator(".tree-card")).toBeVisible();

  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  const before = treeRequests.length;

  await page.clock.runFor(65_000);
  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".tree-card")).toBeVisible();

  await expect.poll(() => treeRequests.length).toBeGreaterThan(before);
});

test("a clipped outermost bin is drawn at the width it counts", async ({ page }) => {
  // The outer bins are clipped to the window, commonly to half width. A uniform
  // bar around each centre overhung the declared window and overlapped its
  // neighbour while the table reported the clipped range.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 0),
    makeCompetition("b".repeat(64), 700001, 28),
  ]);
  await page.locator('#delta-presets [data-half="30"]').click();
  await page.locator("#delta-bin-width").selectOption("10");

  const bars = await page.locator("#delta-chart .bar-mark").evaluateAll(
    (nodes) => nodes.map((node) => ({
      x: Number(node.getAttribute("x")),
      w: Number(node.getAttribute("width")),
      cls: node.getAttribute("class"),
    })),
  );
  expect(bars.length).toBe(2);
  const zero = bars.find((bar) => bar.cls.includes("bar-zero"));
  const outer = bars.find((bar) => bar.cls.includes("bar-pos"));
  // -30 … 30 with 10s bins: the +30 bin spans 25 … 30, half the zero bin's 10s.
  expect(outer.w).toBeLessThan(zero.w * 0.75);
  expect(outer.x + outer.w).toBeLessThanOrEqual(
    await page.locator("#delta-chart").evaluate((svg) => svg.getBoundingClientRect().width),
  );
});

test("a zero-second percentile is read at the coordinate it is drawn at", async ({ page }) => {
  // Three 0s and three 1s: on a log axis both plot at the same x, but the curve
  // there is sampled from 1s. Reading the 50% threshold at 0s put its dot at
  // half height under a curve already at 100%.
  await openDelta(page, [
    ...Array.from({ length: 3 }, (_, i) => makeCompetition(String(i).padStart(64, "0"), 700000 + i, 0)),
    ...Array.from({ length: 3 }, (_, i) => makeCompetition(String(i + 3).padStart(64, "0"), 700003 + i, 1)),
  ]);
  await page.locator('.delta-tab[data-tab="coverage"]').click();

  const dots = await page.locator("#delta-chart .coverage-dot").evaluateAll(
    (nodes) => nodes.map((node) => Number(node.getAttribute("cy"))),
  );
  expect(dots.length).toBe(3);
  expect(Math.max(...dots) - Math.min(...dots)).toBeLessThan(1);
  const labels = await page.locator("#delta-chart .annot-text").allTextContents();
  for (const label of labels.slice(0, 3)) expect(label).toContain("within ±1s");
});

test("a keyboard-focused bin positions its tooltip", async ({ page }) => {
  // A FocusEvent has no clientX/clientY, so forwarding it straight to the
  // tooltip wrote `NaNpx` and the panel stayed wherever the pointer left it.
  await openDelta(page, [makeCompetition("a".repeat(64), 700000, 5)]);
  await page.locator("#delta-chart .bar-hit").first().focus();

  const tooltip = page.locator("#delta-tooltip");
  await expect(tooltip).toBeVisible();
  const box = await tooltip.evaluate((node) => ({
    left: Number.parseFloat(node.style.left),
    top: Number.parseFloat(node.style.top),
  }));
  expect(Number.isFinite(box.left)).toBe(true);
  expect(Number.isFinite(box.top)).toBe(true);
});

test("the narrow layout leaves the context strip its own height", async ({ page }) => {
  // The context canvas carries .delta-canvas too, and the breakpoint's height
  // rule sits after .context-canvas, so it was stretching the strip to the main
  // chart's height on every viewport below 1120px.
  await page.setViewportSize({ width: 1000, height: 900 });
  await openDelta(page);

  const heights = await page.evaluate(() => ({
    main: document.querySelector("#delta-canvas").getBoundingClientRect().height,
    strip: document.querySelector("#delta-context-canvas").getBoundingClientRect().height,
  }));
  expect(Math.round(heights.main)).toBe(380);
  expect(Math.round(heights.strip)).toBe(118);
});

test("bounds that move after a failed load rewrite the era in the URL", async ({ page }) => {
  // The link named an era before there was data to validate it. When the retry
  // succeeds with a narrower range the era is clamped, and a clamp the URL
  // never hears about leaves the shared link disagreeing with the selects.
  await stubApi(page, [], withCompetitions());
  await page.route("**/api/v1/competitions", (route) => route.fulfill({
    status: 500,
    json: { schema_version: "v1", generated_at: GENERATED_AT, error: { code: "internal_error", message: "boom" } },
  }));
  await page.goto("/?view=delta&era=2013-2024");
  await expect(page.locator("#delta-main")).toBeVisible();
  expect(new URL(page.url()).searchParams.get("era")).toBe("2013-2024");

  await page.route("**/api/v1/competitions", (route) => route.fulfill({
    json: competitionsPayload([makeCompetition("a".repeat(64), 700000, 5, { stale_header_time: Y2024 })]),
  }));
  await page.locator("#last-updated").click();
  await expect(page.locator("#delta-chart .bar-mark").first()).toBeVisible();

  await expect(page.locator("#delta-year-from")).toHaveValue("2024");
  await expect(page.locator("#delta-year-to")).toHaveValue("2024");
  expect(new URL(page.url()).searchParams.get("era")).toBeNull();
});

test("a layout change made while the tree is hidden is repainted on return", async ({ page }) => {
  // The rails are shared, so opening the drawer from the distribution fires the
  // tree's post-layout repaint while `.tree-card` is display:none. Measured at
  // 0, it relaid the cached tree out at the 780x420 fallback, and the return to
  // Tree did not correct it: a clean cached tree skips its load and its render
  // paints nothing.
  await stubApi(page, [], withCompetitions([
    makeCompetition("a".repeat(64), 700000, 5),
    makeCompetition("b".repeat(64), 700001, 90000),
  ]));
  await page.goto("/");
  await expect(page.locator(".tree-card")).toBeVisible();

  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator("#delta-outlier-body .outlier-row").first().click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-drawer-collapsed", "false");

  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".tree-card")).toBeVisible();

  await expect.poll(async () => page.locator("#tree-svg").evaluate((svg) => {
    const [, , width, height] = svg.getAttribute("viewBox").split(" ").map(Number);
    return width === svg.clientWidth && height === svg.clientHeight;
  })).toBe(true);
});

test("Show in tree does not center a superseded height", async ({ page }) => {
  // The activation is awaited, so a gesture made during it owns the camera. The
  // superseded jump used to center anyway, dragging the view off the user's
  // actual target and storing that transform.
  const nodes = Array.from({ length: 6 }, (_, i) => makeNode(
    String(i).padStart(64, "0"), 700000 + i, i ? String(i - 1).padStart(64, "0") : null,
    "canonical", { id: i + 1, prev_id: i || null },
  ));
  await stubApi(page, [], {
    ...withCompetitions([
      makeCompetition("a".repeat(64), 700000, 90000, { sources: ["auxpow:namecoin"] }),
      makeCompetition("b".repeat(64), 700005, 5, { sources: ["auxpow:rsk"] }),
    ]),
    treePayload: (params) => treeEnvelope(params, { nodes }),
  });
  // Select the height-700000 outlier, then filter it out, so the hidden
  // selection notice and its "Show in tree" control are on screen.
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator("#delta-outlier-body .outlier-row").first().click();
  await page.locator('input[name="source"][value="auxpow:rsk"]').check();
  await expect(page.locator(".hidden-selection")).toBeVisible();

  // Hold the jump's own tree load open so a competing lookup lands inside it.
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  let first = true;
  await page.route("**/api/v1/tree**", async (route) => {
    if (first) { first = false; await held; }
    await route.fallback();
  });

  await page.locator(".hidden-selection [data-action='tree']").click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  await page.evaluate(async (modules) => {
    const [api, query, shared] = await Promise.all(modules.map((url) => import(url)));
    query.activateHeightLookup(700005);
    shared.state.navEpoch += 1;
    await api.loadTree();
  }, [moduleUrl("api-client.js"), moduleUrl("tree-query-state.js"), STATE_MODULE]);
  release();
  await expect(page.locator("g.tree-node")).toHaveCount(6);

  // Give the superseded continuation its chance to run before measuring: a
  // poll would pass on its first sample, before the late centring lands.
  await page.waitForTimeout(400);

  // 700000 is not the tip, so a render that never centred leaves it well away
  // from the middle; the superseded jump would have parked it there.
  const offset = await page.evaluate(() => {
    const svg = document.querySelector("#tree-svg").getBoundingClientRect();
    const node = document.querySelector('g.tree-node[aria-label*="700000"]').getBoundingClientRect();
    return Math.abs((node.x + node.width / 2) - (svg.x + svg.width / 2));
  });
  expect(offset).toBeGreaterThan(40);
});

test("a tree load that lands while the tree is hidden is repainted on return", async ({ page }) => {
  // The scheduled refresh's own loadTree calls renderTreePanel directly, so it
  // can paint the hidden zero-width panel without the rail callback ever being
  // involved. It also clears treeDirty, so the return to Tree reloads nothing.
  await page.clock.install();
  await stubApi(page, [], withCompetitions());
  await page.goto("/");
  await expect(page.locator(".tree-card")).toBeVisible();

  let release;
  const held = new Promise((resolve) => { release = resolve; });
  let first = true;
  await page.route("**/api/v1/tree**", async (route) => {
    if (first) { first = false; await held; }
    await route.fallback();
  });

  await page.clock.runFor("01:01");
  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  release();

  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".tree-card")).toBeVisible();
  await expect.poll(async () => page.locator("#tree-svg").evaluate((svg) => {
    const [, , width, height] = svg.getAttribute("viewBox").split(" ").map(Number);
    return width === svg.clientWidth && height === svg.clientHeight;
  })).toBe(true);
});

test("the freshness indicator follows the active view", async ({ page }) => {
  // Competitions load once and never auto-refresh, so showing the tree's
  // timestamp beside a distribution fetched hours earlier overstates it, and a
  // direct ?view=delta load used to leave the indicator blank entirely.
  await page.clock.install({ time: new Date("2026-07-28T10:00:00Z") });
  await stubApi(page, [], withCompetitions());
  await page.goto("/?view=delta");
  await expect(page.locator("#delta-main")).toBeVisible();

  const indicator = page.locator("#last-updated");
  await expect(indicator).not.toBeEmpty();
  const atLoad = await indicator.textContent();

  await page.clock.fastForward("02:00:00");
  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".tree-card")).toBeVisible();
  await expect.poll(() => indicator.textContent()).not.toBe(atLoad);

  // Back to a view whose data was never refetched: its own, older stamp.
  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await expect(indicator).toHaveText(atLoad);
});

test("an extreme window keeps the axis readable", async ({ page }) => {
  // Past the largest clock-shaped tick step the axis used to repeat the 90 day
  // entry, which at the i32 extreme is over 1,100 labels.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, 0),
    makeCompetition("b".repeat(64), 700001, 2147483647),
  ]);
  await page.locator('#delta-presets [data-half=""]').click();

  await expect(page.locator("#delta-chart .bar-mark").first()).toBeVisible();
  expect(await page.locator("#delta-chart .axis-text").count()).toBeLessThan(40);
});

test("a Source change made in the distribution reaches the tree", async ({ page }) => {
  // Source is the one filter shared across views. Changing it from the
  // distribution re-derives that view and never touches the tree's DOM, so the
  // cached nodes kept the dimming from whatever was selected on the way out.
  const nodes = [
    makeNode("a".repeat(64), 700000, null, "canonical", {
      id: 1, prev_id: null, source_summary: { sources: ["auxpow:namecoin"] },
    }),
    makeNode("b".repeat(64), 700001, "a".repeat(64), "canonical", {
      id: 2, prev_id: 1, source_summary: { sources: ["auxpow:rsk"] },
    }),
  ];
  await stubApi(page, [], { ...withCompetitions(), treePayload: (params) => treeEnvelope(params, { nodes }) });
  await page.goto("/");
  await expect(page.locator("g.tree-node")).toHaveCount(2);
  await expect(page.locator("g.tree-node.tree-node--dim")).toHaveCount(0);

  await page.locator('.view-tab[data-view="delta"]').click();
  await expect(page.locator("#delta-main")).toBeVisible();
  await page.locator('input[name="source"][value="auxpow:rsk"]').check();

  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".tree-card")).toBeVisible();
  // Only the Namecoin node is off the selection, so exactly it fades back.
  await expect(page.locator("g.tree-node.tree-node--dim")).toHaveCount(1);
  await expect(page.locator('g.tree-node[aria-label*="700000"]')).toHaveClass(/tree-node--dim/);
});

test("hovering a bin emphasises that bin", async ({ page }) => {
  // The hit rect is appended after its own mark so it paints above and takes the
  // pointer, which made an adjacent-sibling rule reach the NEXT bin's mark:
  // inspecting one bin brightened its neighbour and the last bin never lit up.
  await openDelta(page, [
    makeCompetition("a".repeat(64), 700000, -20),
    makeCompetition("b".repeat(64), 700001, 20),
  ]);
  await page.locator('#delta-presets [data-half="30"]').click();
  await expect(page.locator("#delta-chart .bar-mark")).toHaveCount(2);

  await page.locator("#delta-chart .bar-hit").last().hover();
  const filters = await page.locator("#delta-chart .bar-mark").evaluateAll(
    (nodes) => nodes.map((node) => getComputedStyle(node).filter),
  );
  expect(filters[1]).toContain("brightness");
  expect(filters[0]).not.toContain("brightness");
});

test("a bin clipped by a narrow window says so in the legend", async ({ page }) => {
  // A ±10s window with 60s bins is really −10s…+10s. Announcing ±30s there
  // contradicts both the table and what the view counts as an outlier.
  await openDelta(page, [makeCompetition("a".repeat(64), 700000, 0)]);
  await page.locator('#delta-presets [data-half="10"]').click();
  await page.locator("#delta-bin-width").selectOption("60");

  await expect(page.locator("#delta-legend")).toContainText("±10s");
  await expect(page.locator("#delta-legend")).not.toContainText("±30s");
});
