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

// The offset is the only place the drawer subtracts two stamps. fmtDelta rounds
// to the largest fitting unit, which is exactly what loses the distinction a
// reader comparing two auxiliaries on one block needs, so the exact second count
// has to survive alongside it and be visible without a pointer. -607s is the
// case that proves it: it renders as -10m, so an assertion built on a round
// -600 would pass whether or not the exact figure was kept at all.
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
          // Committed at job start: the ordinary case, well behind the block,
          // and by a span the compact form cannot express exactly.
          event({ child_block_time: PARENT_TIME - 607 }),
          // Stamped after the block it is committed to: the ordering disagreement.
          event({ id: 2, child_chain: "namecoin", child_block_time: PARENT_TIME + 7 }),
          // Tied to the second. Neither side gets a sign.
          event({ id: 3, child_chain: "fractal", child_block_time: PARENT_TIME }),
          // No child stamp to subtract.
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

  // Lossy compaction keeps the exact figure VISIBLE, not just in the title: a
  // reader with no pointer must still be able to tell -607s from -600s.
  // toHaveText alone reads textContent and passes on a hidden element, which is
  // the exact regression this is guarding against, so assert visibility too.
  await expect(offset(0)).toBeVisible();
  await expect(offset(0)).toHaveText("-10m (-607s) vs Bitcoin");
  await expect(offset(0)).toHaveAttribute("title", "-607s from the Bitcoin header time");
  // The offset annotates the stamp; it must not displace it.
  await expect(events.nth(0).locator(`dt:has-text("Child Time") + dd`)).toContainText(
    "2023-11-14T22:13:13Z",
  );

  // Already exact, so no redundant "+7s (+7s)".
  await expect(offset(1)).toBeVisible();
  await expect(offset(1)).toHaveText("+7s vs Bitcoin");
  await expect(offset(1)).toHaveAttribute("title", "+7s from the Bitcoin header time");

  // Equal stamps are neutral in both forms; a "+0s" would imply a direction.
  await expect(offset(2)).toBeVisible();
  await expect(offset(2)).toHaveText("0s vs Bitcoin");
  await expect(offset(2)).toHaveAttribute("title", "0s from the Bitcoin header time");

  await expect(offset(3)).toHaveCount(0);
  await expect(
    events.nth(3).locator(`dt:has-text("Child Time") + dd .null-value`),
  ).toHaveText("unavailable");
});

// auxpowHelpFor falls back to an empty "AuxPoW" topic for an unknown key, so a
// mis-keyed button or an emptied body would leave every other assertion here
// green while the row's explanation silently vanished. Open it and check the
// identity plus one load-bearing sentence.
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
  // The mechanism the whole topic exists to convey.
  await expect(page.locator("#auxpow-dialog-body")).toContainText(
    "the monitor observes no build and records only the claim",
  );
});

// Contract-wise `block.header.time` is a required non-null u32, so this stubs a
// response the API does not produce. It is kept deliberately, as resilience
// cover for the parent-side guard: the drawer reads `block.header?.time`, and a
// malformed or future-shape payload must degrade to the child stamp alone rather
// than subtract from undefined and print an offset from nothing.
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

// child_block_time is an unbounded i64 by contract, so the guard has to cover
// the operand AND the subtraction, which fail independently and so need fixtures
// that isolate each. Event 1 is exact on its own and inexact once the parent
// time is taken off it, which only the offset guard catches. Event 2 is the
// mirror image: it arrives just past the exactly-representable range, so it is
// already approximate, yet its difference lands back inside the safe range and
// would render as a measured figure unless the OPERAND guard rejects it. Event 3
// is exact throughout and must still render.
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
            id: 3,
            child_chain: "syscoin",
            // One past MAX_SAFE_INTEGER, so the value itself is already
            // approximate, while the difference (about 9.0072e15) is comfortably
            // back inside the safe range. Anything larger would be rejected by
            // the offset guard instead and would prove nothing about this one.
            child_block_time: Number.MAX_SAFE_INTEGER + 1,
          }),
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
  await expect(events).toHaveCount(3);
  await events.nth(0).locator("summary").click();
  await events.nth(1).locator("summary").click();
  await events.nth(2).locator("summary").click();

  // Inexact subtraction, exact operands.
  await expect(events.nth(0).locator(".child-time-offset")).toHaveCount(0);
  // The fallback is the stamp ALONE, not an empty cell: asserting only the
  // missing offset would pass on a regression that dropped the time with it.
  await expect(events.nth(0).locator(`dt:has-text("Child Time") + dd`)).toContainText(
    String(-Number.MAX_SAFE_INTEGER),
  );

  // Inexact operand, safe-looking subtraction.
  await expect(events.nth(1).locator(".child-time-offset")).toHaveCount(0);

  // Exact throughout, so the offset renders.
  await expect(events.nth(2).locator(".child-time-offset")).toHaveAttribute(
    "title",
    `-${Number.MAX_SAFE_INTEGER}s from the Bitcoin header time`,
  );
});
