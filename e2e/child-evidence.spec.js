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
