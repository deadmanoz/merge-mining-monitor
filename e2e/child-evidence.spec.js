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

// The offset is the only place the drawer subtracts two stamps, and both are
// independently optional. fmtDelta rounds to the largest fitting unit, so the
// exact second count on the title is what a reader comparing two auxiliaries
// on one block actually needs; assert both forms, and assert that a missing
// stamp on either side yields no offset rather than an offset from nothing.
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
          // Committed at job start: the ordinary case, well behind the block.
          event({ child_block_time: PARENT_TIME - 600 }),
          // Stamped after the block it is committed to: the inconsistency.
          event({ id: 2, child_chain: "namecoin", child_block_time: PARENT_TIME + 7 }),
          // No child stamp to subtract.
          event({ id: 3, child_chain: "syscoin", child_block_time: null }),
        ],
      });
      payload.block.header = { time: PARENT_TIME };
      return payload;
    },
  });

  await page.goto(`/?selected=${HASH}`);
  const events = page.locator("#drawer details.event-block");
  await expect(events).toHaveCount(3);
  for (const index of [0, 1, 2]) {
    await events.nth(index).locator("summary").click();
  }

  const offset = (index) => events.nth(index).locator(".child-time-offset");
  await expect(offset(0)).toHaveText("-10m vs Bitcoin");
  await expect(offset(0)).toHaveAttribute("title", "-600s from the Bitcoin header time");
  await expect(offset(1)).toHaveText("+7s vs Bitcoin");
  await expect(offset(1)).toHaveAttribute("title", "+7s from the Bitcoin header time");
  await expect(offset(2)).toHaveCount(0);
  await expect(
    events.nth(2).locator(`dt:has-text("Child Time") + dd .null-value`),
  ).toHaveText("unavailable");
});

// The parent side is optional too: a block detail with no header cannot be
// subtracted from, and must render the child stamp alone.
test("an auxiliary block shows no offset when the Bitcoin header time is unavailable", async ({
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

// child_block_time is an unbounded i64 by contract, so the two stamps can each
// be exact while their difference is not: the guard has to be on the
// subtraction, not only on the operands. The first stamp below is safe on its
// own and unsafe once the parent time is taken off it; the second differences
// back inside the range and must still render.
test("an offset JavaScript cannot compute exactly is not rendered", async ({ page }) => {
  const PARENT_TIME = 1_700_000_000;
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => {
      const payload = blockPayload(hash, {
        generated_at: GENERATED_AT,
        event_details: [
          event({ child_block_time: -Number.MAX_SAFE_INTEGER }),
          event({
            id: 2,
            child_chain: "namecoin",
            child_block_time: -Number.MAX_SAFE_INTEGER + PARENT_TIME,
          }),
        ],
      });
      payload.block.header = { time: PARENT_TIME };
      return payload;
    },
  });

  await page.goto(`/?selected=${HASH}`);
  const events = page.locator("#drawer details.event-block");
  await expect(events).toHaveCount(2);
  await events.nth(0).locator("summary").click();
  await events.nth(1).locator("summary").click();

  await expect(events.nth(0).locator(".child-time-offset")).toHaveCount(0);
  // The fallback is the stamp ALONE, not an empty cell: asserting only the
  // missing offset would pass on a regression that dropped the time with it.
  await expect(events.nth(0).locator(`dt:has-text("Child Time") + dd`)).toContainText(
    String(-Number.MAX_SAFE_INTEGER),
  );
  await expect(events.nth(1).locator(".child-time-offset")).toHaveAttribute(
    "title",
    `-${Number.MAX_SAFE_INTEGER}s from the Bitcoin header time`,
  );
});
