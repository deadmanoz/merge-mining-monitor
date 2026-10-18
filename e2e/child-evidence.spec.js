const { expect, test } = require("@playwright/test");
const {
  GENERATED_AT,
  blockPayload,
  makeNode,
  stubApi,
  treeEnvelope,
} = require("./support/api-stubs");

const HASH = "a".repeat(64);

function event(overrides = {}) {
  return {
    id: 1,
    source: "auxpow:elastos",
    child_chain: "elastos",
    child_height: 2_000_000,
    child_block_hash: null,
    child_header_hex: null,
    child_block_time: null,
    child_nbits: null,
    pow_validates_btc_target: true,
    pow_validates_child_target: null,
    slot_index: null,
    chain_id: null,
    child_miner_pool: null,
    rsk: null,
    aux_proof: null,
    aux_merkle_proof_hex: null,
    ...overrides,
  };
}

test("the block drawer renders authenticated and unavailable child evidence honestly", async ({
  page,
}) => {
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) =>
      blockPayload(hash, {
        generated_at: GENERATED_AT,
        event_details: [
          event(),
          event({
            id: 2,
            source: "auxpow:i0coin",
            child_chain: "i0coin",
            child_height: null,
            child_block_hash: "1".repeat(64),
            child_header_hex: "00".repeat(80),
            child_block_time: 1_700_000_000,
            child_nbits: "1d00ffff",
          }),
        ],
      }),
  });

  await page.goto(`/?selected=${HASH}`);
  await expect(page.locator("#drawer")).toContainText("Auxiliary blocks");

  const events = page.locator("#drawer details.event-block");
  await expect(events).toHaveCount(2);
  await expect(events.nth(1).locator("summary")).toContainText("height unavailable");

  await events.nth(0).locator("summary").click();
  await expect(events.nth(0)).toContainText("Child Header");
  await expect(events.nth(0)).toContainText("Child nBits");
  for (const label of ["Child Hash", "Child Header", "Child Time", "Child nBits"]) {
    await expect(
      events.nth(0).locator(`dt:has-text("${label}") + dd .null-value`),
    ).toHaveText("unavailable");
  }

  await events.nth(1).locator("summary").click();
  await expect(events.nth(1)).toContainText("1d00ffff");
  await expect(events.nth(1)).toContainText("2023-11-14T22:13:20Z");
});

test("Qbit auxiliary summaries show child pools and leave the shared wallet unlabelled", async ({ page }) => {
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  const childPools = [
    [78058, { id: 8, slug: "f2pool", name: "F2Pool", known: true }],
    [8197, { id: 9, slug: "secpool", name: "SecPool", known: true }],
    [2450, { id: 10, slug: "luxor", name: "Luxor", known: true }],
    [7887, { id: null, slug: null, name: "Unknown", known: false }],
  ];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => blockPayload(hash, {
      bitcoin_miner_pool: { id: 7, slug: "antpool", name: "AntPool", known: true },
      event_details: childPools.map(([height, pool], index) => event({
        id: index + 1, source: "auxpow:qbit", child_chain: "qbit",
        child_height: height, child_miner_pool: pool,
      })),
    }),
  });
  await page.goto(`/?selected=${HASH}`);
  const summaries = page.locator("#drawer details.event-block > summary");
  await expect(summaries).toHaveCount(4);
  for (let index = 0; index < 3; index += 1) {
    await expect(summaries.nth(index)).toHaveText(`Qbit · ${childPools[index][0]} · ${childPools[index][1].name}`);
  }
  await expect(summaries.nth(3)).toHaveText("Qbit · 7887");
});

// -607s rounds to -10m under fmtDelta, proving the exact second figure survives alongside the compact unit.
test("an auxiliary block's offset from the Bitcoin header time is shown only when both stamps exist", async ({
  page,
}) => {
  const PARENT_TIME = 1_700_000_600;
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => {
      const payload = blockPayload(hash, {
        generated_at: GENERATED_AT,
        event_details: [
          event({ child_block_time: PARENT_TIME - 607 }),
          event({ id: 2, child_chain: "namecoin", child_block_time: PARENT_TIME + 7 }),
          event({ id: 3, child_chain: "fractal", child_block_time: PARENT_TIME }),
          event({ id: 4, child_chain: "syscoin", child_block_time: null }),
        ],
      });
      payload.block.header = { time: PARENT_TIME };
      return payload;
    },
  });

  await page.goto(`/?selected=${HASH}`);
  const events = page.locator("#drawer details.event-block");
  await expect(events).toHaveCount(4);
  for (const index of [0, 1, 2, 3]) {
    await events.nth(index).locator("summary").click();
  }

  const offset = (index) => events.nth(index).locator(".child-time-offset");

  await expect(offset(0)).toBeVisible();
  await expect(offset(0)).toHaveText("-10m (-607s) vs Bitcoin");
  await expect(offset(0)).toHaveAttribute("title", "-607s from the Bitcoin header time");
  await expect(events.nth(0).locator(`dt:has-text("Child Time") + dd`)).toContainText(
    "2023-11-14T22:13:13Z",
  );

  await expect(offset(1)).toBeVisible();
  await expect(offset(1)).toHaveText("+7s vs Bitcoin");
  await expect(offset(1)).toHaveAttribute("title", "+7s from the Bitcoin header time");

  await expect(offset(2)).toBeVisible();
  await expect(offset(2)).toHaveText("0s vs Bitcoin");
  await expect(offset(2)).toHaveAttribute("title", "0s from the Bitcoin header time");

  await expect(offset(3)).toHaveCount(0);
  await expect(
    events.nth(3).locator(`dt:has-text("Child Time") + dd .null-value`),
  ).toHaveText("unavailable");
});

test("the Child Time row opens its own help topic", async ({ page }) => {
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) =>
      blockPayload(hash, {
        generated_at: GENERATED_AT,
        event_details: [event({ child_block_time: 1_700_000_000 })],
      }),
  });

  await page.goto(`/?selected=${HASH}`);
  const events = page.locator("#drawer details.event-block");
  await events.nth(0).locator("summary").click();
  await events.nth(0).getByRole("button", { name: "About Child Time" }).click();

  const dialog = page.locator("#auxpow-dialog");
  await expect(dialog).toBeVisible();
  await expect(page.locator("#auxpow-dialog-title")).toHaveText("Child Time");
  await expect(page.locator("#auxpow-dialog-kicker")).toHaveText(
    "The auxiliary block's own claimed timestamp, and its offset from Bitcoin",
  );
  await expect(page.locator("#auxpow-dialog-body")).toContainText(
    "the monitor observes no build and records only the claim",
  );
  await expect(page.locator("#auxpow-dialog-body")).toContainText(
    "narrower than whether the Bitcoin block committed to it",
  );
});

test("a malformed block detail with no Bitcoin header time renders the stamp without an offset", async ({
  page,
}) => {
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) =>
      blockPayload(hash, {
        generated_at: GENERATED_AT,
        event_details: [event({ child_block_time: 1_700_000_000 })],
      }),
  });

  await page.goto(`/?selected=${HASH}`);
  const events = page.locator("#drawer details.event-block");
  await events.nth(0).locator("summary").click();
  await expect(events.nth(0)).toContainText("2023-11-14T22:13:20Z");
  await expect(events.nth(0).locator(".child-time-offset")).toHaveCount(0);
});

// Event 1 tests the subtraction guard (exact operand, inexact difference);
// Event 2 tests the operand guard (raw JSON lexeme 9007199254740993 rounds on parse).
test("an offset JavaScript cannot compute exactly is not rendered", async ({ page }) => {
  const PARENT_TIME = 1_700_000_000;
  const ROUNDED_STAMP = "__ROUNDED_CHILD_TIME__";
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
  });
  await page.route("**/api/v1/block/**", async (route) => {
    const hash = route.request().url().split("/").at(-1);
    const payload = blockPayload(hash, {
      generated_at: GENERATED_AT,
      event_details: [
        event({ child_block_time: -Number.MAX_SAFE_INTEGER }),
        event({ id: 3, child_chain: "syscoin", child_block_time: ROUNDED_STAMP }),
        event({
          id: 2,
          child_chain: "namecoin",
          child_block_time: -Number.MAX_SAFE_INTEGER + PARENT_TIME,
        }),
      ],
    });
    payload.block.header = { time: PARENT_TIME };
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(payload).replace(`"${ROUNDED_STAMP}"`, "9007199254740993"),
    });
  });

  await page.goto(`/?selected=${HASH}`);
  const events = page.locator("#drawer details.event-block");
  await expect(events).toHaveCount(3);
  await events.nth(0).locator("summary").click();
  await events.nth(1).locator("summary").click();
  await events.nth(2).locator("summary").click();

  await expect(events.nth(0).locator(".child-time-offset")).toHaveCount(0);
  await expect(events.nth(0).locator(`dt:has-text("Child Time") + dd`)).toContainText(
    String(-Number.MAX_SAFE_INTEGER),
  );

  await expect(events.nth(1).locator(".child-time-offset")).toHaveCount(0);
  await expect(events.nth(1).locator(`dt:has-text("Child Time") + dd`)).toContainText(
    "9007199254740992",
  );

  await expect(events.nth(2).locator(".child-time-offset")).toHaveAttribute(
    "title",
    `-${Number.MAX_SAFE_INTEGER}s from the Bitcoin header time`,
  );
});
