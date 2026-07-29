const { expect, test } = require("@playwright/test");
const { makeFinding, stubApi, stubFindings } = require("./support/api-stubs");

// The findings view: feed/article canvas states, the finding= URL contract,
// evidence-anchor cross-links, and the no-drawer layout. Shell mechanics
// (switcher, scope swap, fallback) live in view-shell.spec.js.

// Three findings across two months so month grouping, ordering, and the
// newer/older bounds all have branches to observe. Newest first, like the
// generated module.
const CORPUS = [
  makeFinding({
    slug: "newest-incident",
    title: "Newest incident",
    category: "chain-incident",
    status: "ongoing",
    observed_at: "2026-07-20",
    affected_sources: ["auxpow:namecoin"],
    anchors: [
      { kind: "btc-height", value: "700000", label: "the parent" },
      { kind: "source", value: "auxpow:namecoin" },
      { kind: "pool", value: "TestPool" },
    ],
    figures: [
      {
        kind: "line-series",
        caption: "Weekly captures",
        y_label: "events / week",
        points: [
          { t: "2026-07-06", v: 480 },
          { t: "2026-07-13", v: 538 },
          { t: "2026-07-20", v: 68 },
        ],
        markers: [{ t: "2026-07-20", label: "halt" }],
        note: "test data",
      },
    ],
  }),
  makeFinding({
    slug: "mid-shift",
    title: "Mid shift",
    category: "hashrate-shift",
    status: "monitoring",
    observed_at: "2026-06-15",
    affected_sources: ["auxpow:syscoin"],
  }),
  makeFinding({
    slug: "oldest-note",
    title: "Oldest note",
    category: "dataset-note",
    status: "concluded",
    observed_at: "2026-06-01",
    affected_sources: ["auxpow:namecoin"],
  }),
];

async function openFindings(page, { corpus = CORPUS, path = "/?view=findings" } = {}) {
  await stubFindings(page, corpus);
  await stubApi(page, []);
  await page.goto(path);
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "findings");
}

test("the feed renders month groups and cards newest-first", async ({ page }) => {
  await openFindings(page);

  await expect(page.locator(".findings-month")).toHaveText(["July 2026", "June 2026"]);
  await expect(page.locator(".finding-card-title")).toHaveText([
    "Newest incident",
    "Mid shift",
    "Oldest note",
  ]);
  // Citation markers in summaries render as superscript links, not raw [^1].
  await expect(page.locator(".finding-card-summary sup.sd-cite").first()).toBeVisible();
});

test("the drawer column is absent on findings and intact on return", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings" });

  await expect(page.locator(".detail-drawer")).toBeHidden();
  // The grid reserves no drawer track: the canvas reaches the drawer's edge.
  const overlap = await page.evaluate(() => {
    const main = document.querySelector("#findings-main").getBoundingClientRect();
    const workspace = document.querySelector(".workspace").getBoundingClientRect();
    return workspace.right - main.right;
  });
  expect(overlap).toBeLessThan(20);

  await page.locator('.view-tab[data-view="tree"]').click();
  await expect(page.locator(".detail-drawer")).toBeVisible();
});

test("a card click opens the article and writes finding=", async ({ page }) => {
  await openFindings(page);

  await page.locator('article.finding-card[data-finding="mid-shift"]').click();
  await expect(page.locator(".finding-article-title")).toHaveText("Mid shift");
  expect(new URL(page.url()).searchParams.get("finding")).toBe("mid-shift");
  // The feed is replaced, not stacked beside or below the article.
  await expect(page.locator(".finding-card")).toHaveCount(0);
});

test("the back control returns to the feed and clears finding=", async ({ page }) => {
  await openFindings(page);

  await page.locator('article.finding-card[data-finding="newest-incident"]').click();
  await page.locator('[data-action="findings-back"]').click();
  await expect(page.locator(".finding-card")).toHaveCount(3);
  expect(new URL(page.url()).searchParams.get("finding")).toBeNull();
});

test("a finding deep link opens the article directly", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=oldest-note" });

  await expect(page.locator(".finding-article-title")).toHaveText("Oldest note");
});

test("an unknown finding slug degrades to the feed and clears the param", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=no-such-finding" });

  await expect(page.locator(".finding-card")).toHaveCount(3);
  expect(new URL(page.url()).searchParams.get("finding")).toBeNull();
});

test("finding= supplied with a non-findings view is ignored", async ({ page }) => {
  await stubFindings(page, CORPUS);
  await stubApi(page, []);
  await page.goto("/?view=delta&finding=newest-incident");

  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "delta");
  expect(new URL(page.url()).searchParams.get("finding")).toBeNull();

  // Entering findings later shows the feed: the dropped slug did not linger.
  await page.locator('.view-tab[data-view="findings"]').click();
  await expect(page.locator(".finding-card")).toHaveCount(3);
});

test("newer and older step through corpus order and disable at the ends", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=newest-incident" });

  await expect(page.locator('[data-action="findings-newer"]')).toBeDisabled();
  await page.locator('[data-action="findings-older"]').click();
  await expect(page.locator(".finding-article-title")).toHaveText("Mid shift");
  await page.locator('[data-action="findings-older"]').click();
  await expect(page.locator(".finding-article-title")).toHaveText("Oldest note");
  await expect(page.locator('[data-action="findings-older"]')).toBeDisabled();
  await expect(page.locator('[data-action="findings-newer"]')).toBeEnabled();
});

test("a btc-height anchor lands on the tree with no finding= in the URL", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=newest-incident" });

  await page.locator('[data-anchor-kind="btc-height"]').click();
  await expect(page.locator(".workspace")).toHaveAttribute("data-view", "tree");
  const params = new URL(page.url()).searchParams;
  expect(params.get("tree_height")).toBe("700000");
  expect(params.get("finding")).toBeNull();

  // Returning to findings restores the open article from memory.
  await page.locator('.view-tab[data-view="findings"]').click();
  await expect(page.locator(".finding-article-title")).toHaveText("Newest incident");
  expect(new URL(page.url()).searchParams.get("finding")).toBe("newest-incident");
});

test("a source anchor opens the source dialog", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=newest-incident" });

  await page.locator('[data-anchor-kind="source"]').click();
  await expect(page.locator("#source-dialog")).toBeVisible();
});

test("pool and child-height anchors are informational, not clickable", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=newest-incident" });

  const statics = page.locator(".finding-anchor-static");
  await expect(statics).toHaveCount(1);
  expect(await statics.first().evaluate((el) => el.tagName)).toBe("SPAN");
});

test("a line-series figure renders with its marker and caption", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=newest-incident" });

  const figure = page.locator(".finding-figure");
  await expect(figure.locator("svg path")).toHaveCount(1);
  const marker = figure.locator("svg text").filter({ hasText: "halt" });
  await expect(marker).toBeVisible();
  // The marker sits on the final sample: a center anchor would clip its label
  // outside the viewBox, so edge markers re-anchor inward.
  await expect(marker).toHaveAttribute("text-anchor", "end");
  await expect(figure.locator("figcaption")).toContainText("Weekly captures");
});

test("a citation click in a card summary does not open the article", async ({ page }) => {
  await openFindings(page);

  // Citation links open externally (target=_blank); the card must not also
  // activate. Nested-interactive markup is ruled out structurally: the card
  // is not a button, the title opener is.
  const popup = page.waitForEvent("popup");
  await page.locator(".finding-card-summary sup.sd-cite a").first().click();
  await (await popup).close();
  await expect(page.locator(".finding-article-title")).toHaveCount(0);
  await expect(page.locator(".finding-card")).toHaveCount(3);
});

test("the card title opener is a button and opens the article", async ({ page }) => {
  await openFindings(page);

  const opener = page.locator('.finding-card-open[data-finding="mid-shift"]');
  expect(await opener.evaluate((el) => el.tagName)).toBe("BUTTON");
  await opener.focus();
  await page.keyboard.press("Enter");
  await expect(page.locator(".finding-article-title")).toHaveText("Mid shift");
});

test("wide screens bound and center the findings column", async ({ page }) => {
  await page.setViewportSize({ width: 2400, height: 1100 });
  await openFindings(page);

  // Without the drawer track the canvas spans the viewport; cards must not
  // stretch with it, and the column must sit centered rather than hug the
  // rail edge.
  const feed = await page.evaluate(() => {
    const card = document.querySelector(".finding-card").getBoundingClientRect();
    const canvas = document.querySelector("#findings-main").getBoundingClientRect();
    return { cardWidth: card.width, leftGap: card.left - canvas.left, rightGap: canvas.right - card.right };
  });
  expect(feed.cardWidth).toBeLessThanOrEqual(920);
  expect(Math.abs(feed.leftGap - feed.rightGap)).toBeLessThan(40);
  expect(feed.leftGap).toBeGreaterThan(100);

  // The article shares the same centered column with a common left edge.
  await page.locator('article.finding-card[data-finding="newest-incident"]').click();
  const article = await page.evaluate(() => {
    const head = document.querySelector(".finding-article-head").getBoundingClientRect();
    const nav = document.querySelector(".finding-article-nav").getBoundingClientRect();
    const canvas = document.querySelector("#findings-main").getBoundingClientRect();
    return { headLeft: head.left, navLeft: nav.left, leftGap: head.left - canvas.left };
  });
  expect(article.headLeft).toBe(article.navLeft);
  expect(article.leftGap).toBeGreaterThan(100);
});

test("mobile widths collapse findings to a single column", async ({ page }) => {
  await page.setViewportSize({ width: 700, height: 900 });
  await openFindings(page);

  // The base findings grid override outranks the breakpoint's plain
  // .workspace rule, so the breakpoint must restate the columns; without
  // that, a phantom 250px filter track squeezes the stacked canvas.
  const layout = await page.evaluate(() => {
    const rail = document.querySelector(".filter-rail").getBoundingClientRect();
    const main = document.querySelector("#findings-main").getBoundingClientRect();
    const workspace = document.querySelector(".workspace").getBoundingClientRect();
    return { stacked: main.top >= rail.bottom, mainWidth: main.width, workspaceWidth: workspace.width };
  });
  expect(layout.stacked).toBe(true);
  expect(layout.mainWidth).toBeGreaterThan(layout.workspaceWidth * 0.9);
});

test("the article renders cited prose and a Sources list", async ({ page }) => {
  await openFindings(page, { path: "/?view=findings&finding=newest-incident" });

  await expect(page.locator(".finding-article-body sup.sd-cite a").first()).toHaveAttribute(
    "target",
    "_blank",
  );
  await expect(page.locator(".finding-article-body code")).toHaveText("code");
  await expect(page.locator(".finding-sources ol.sd-sources li")).toHaveCount(1);
});

test("category and status filters narrow the feed", async ({ page }) => {
  await openFindings(page);

  await page.locator('input[name="finding-category"][value="dataset-note"]').uncheck();
  await expect(page.locator(".finding-card")).toHaveCount(2);
  await page.locator('input[name="finding-status"][value="ongoing"]').uncheck();
  await expect(page.locator(".finding-card")).toHaveCount(1);
  await expect(page.locator(".finding-card-title")).toHaveText("Mid shift");
  await page.locator('input[name="finding-category"][value="dataset-note"]').check();
  await expect(page.locator(".finding-card")).toHaveCount(2);
});

test("the shared source filter narrows the feed by affected sources", async ({ page }) => {
  // Via the URL rather than the checkbox: the box lives inside a collapsed
  // <details> source group, and hydration exercises the same shared filter.
  await openFindings(page, { path: "/?view=findings&sources=auxpow:syscoin" });

  await expect(page.locator(".finding-card")).toHaveCount(1);
  await expect(page.locator(".finding-card-title")).toHaveText("Mid shift");
});
