const { expect, test } = require("@playwright/test");
const { blockPayload, makeNode, stubApi, treeEnvelope } = require("./support/api-stubs");
const fixture = require("../fixtures/api/block-relationship-signals.json");

// The fixture's Binance Pool block, whose RSK reward address and exSat account name AntPool, plus
// a first signal naming no known pool, whose group still comes last.
const signals = {
  ...fixture.relationship_signals,
  entries: [
    {
      kind: "child_coinbase_tag",
      source: "auxpow:elastos",
      namespace: "elastos_minerinfo",
      value: "solo",
      pool: { id: null, slug: null, name: "Unknown", known: false },
      basis: "child_registry",
    },
    ...fixture.relationship_signals.entries,
  ],
};
const MIXED = "ce".repeat(32);
const PLAIN = "cf".repeat(32);

async function openTree(page, url = "/") {
  const nodes = [
    makeNode(MIXED, 700000, null, "canonical", { relationship: { mixed: true, pools: signals.pools } }),
    makeNode(PLAIN, 700001, MIXED, "canonical", { relationship: { mixed: false, pools: [signals.reference_pool] } }),
  ];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => blockPayload(hash, { relationship_signals: signals }),
  });
  await page.goto(url);
}

test("the drawer groups the signals by pool, collapsed, accents the group naming another pool and scrolls to them from the chip", async ({ page }) => {
  await openTree(page, `/?selected=${MIXED}`);
  const sections = page.locator("#drawer .detail-section");
  await expect(sections.nth(1).locator("h3")).toContainText("Relationship signals");

  const section = page.locator('#drawer .detail-section[data-topic="relationship_signals"]');
  await expect(section.locator(".signal-summary")).toHaveText("Mixed signals: they name 2 pools, Binance Pool and AntPool.");
  const groups = section.locator("details.event-block");
  await expect(groups.locator("summary")).toHaveText(["Binance Pool 1 signal", "AntPool 2 signals differs", "No known pool 1 signal"]);
  await expect(section.locator("details[open]")).toHaveCount(0);
  await expect(section.locator(".signal-differs .signal-mixed-pill")).toHaveText(["differs"]);

  await groups.nth(1).locator("summary").click();
  await expect(groups.nth(1).getByText("RSK reward address")).toBeVisible();
  await expect(groups.nth(1).locator("dd")).toContainText(["5555555555555555555555555555555555555555", "antpoole.sat"]);

  const chip = sections.nth(0).locator(".signal-mixed-pill");
  await expect(chip).toHaveText("Mixed signals · 2 pools");
  const url = page.url();
  await chip.click();
  await expect(section.locator("h3")).toBeInViewport();
  expect(page.url()).toBe(url);

  await section.getByRole("button", { name: "About Relationship signals" }).click();
  await expect(page.locator("#auxpow-dialog-body")).toContainText("A signal is an association, not proof of who mined the block");
});

test("a mixed block gets a ring, a legend row and its pools on the selection card", async ({ page }) => {
  await openTree(page, `/?selected=${MIXED}`);
  await expect(page.locator("g.tree-node")).toHaveCount(2);
  await expect(page.locator("g.tree-node .tree-mixed-ring")).toHaveCount(1);
  await expect(page.locator('g.tree-node[aria-label*="700000"] .tree-mixed-ring')).toHaveCount(1);
  await expect(page.locator("#tree-legend-body")).toContainText("mixed signals");

  const card = page.locator(".tree-chain-card");
  await expect(card.locator(".tree-chain-card-title")).toHaveText(["mixed signals (2 pools)"]);
  await expect(card.locator(".tree-chain-card-name")).toHaveText(["Binance Pool", "AntPool"]);
  await expect(card.locator(".tree-mixed-swatch")).toHaveCount(2);
});

test("the mixed signals toggle dims the other blocks through a filter change, a forced reload and a page reload", async ({ page }) => {
  await openTree(page);
  const dimmed = page.locator("g.tree-node.tree-node--dim");
  await expect(page.locator("g.tree-node")).toHaveCount(2);
  await expect(dimmed).toHaveCount(0);

  await page.locator('input[name="mixed"]').check();
  await expect(dimmed).toHaveCount(1);
  await expect(page.locator('g.tree-node[aria-label*="700001"]')).toHaveClass(/tree-node--dim/);
  expect(new URL(page.url()).searchParams.get("mixed")).toBe("1");

  await page.locator('input[name="kind"][value="stale"]').uncheck();
  await expect(dimmed).toHaveCount(1);

  const treeReloaded = page.waitForResponse((response) => response.url().includes("/api/v1/tree"));
  await page.locator("#last-updated").click();
  await treeReloaded;
  await expect(page.locator('g.tree-node[aria-label*="700001"]')).toHaveClass(/tree-node--dim/);

  await page.reload();
  await expect(page.locator('input[name="mixed"]')).toBeChecked();
  await expect(dimmed).toHaveCount(1);

  await page.locator('input[name="mixed"]').uncheck();
  await expect(dimmed).toHaveCount(0);
  expect(new URL(page.url()).searchParams.get("mixed")).toBeNull();
});
