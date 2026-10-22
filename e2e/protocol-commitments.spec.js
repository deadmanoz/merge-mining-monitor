const { expect, test } = require("@playwright/test");
const { blockPayload, makeNode, stubApi, treeEnvelope } = require("./support/api-stubs");
const fixture = require("../fixtures/api/block-protocol-commitments.json");
const treeFixture = require("../fixtures/api/tree.json");

const HASH = "ce".repeat(32);

async function openDrawer(page, protocolCommitments) {
  const nodes = [makeNode(HASH, 700000, null, "canonical", { id: 1, prev_id: null })];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => blockPayload(hash, { protocol_commitments: protocolCommitments }),
  });
  await page.goto(`/?selected=${HASH}`);
  return page.locator("#drawer .detail-section").filter({ hasText: "Protocol commitments" });
}

test("decoded declarations collapse and show each protocol account association after the merge-mining sections", async ({ page }) => {
  const section = await openDrawer(page, fixture.protocol_commitments);
  await expect(page.locator("#drawer .detail-section").last()).toContainText("Protocol commitments");
  await expect(section).toContainText("2 declarations, decoded from the Bitcoin Core coinbase");

  const items = section.locator(".protocol-commitment");
  const [core, exsat] = [items.nth(0), items.nth(1)];
  await expect(core).toContainText("CORE");
  await expect(core).toContainText("read by Core DAO");
  await expect(core).toContainText("Protocol account association");
  await expect(core).toContainText("SecPool");
  await expect(core).toContainText("0x942c01262fa046fdb8ba0dfce3753405c74aed0e");
  await expect(exsat).toContainText("selection undetermined");
  await expect(exsat).toContainText("antpoole.sat");
  await expect(exsat).toContainText("AntPool");
  await expect(core.locator("details.collapse")).not.toHaveAttribute("open", "");
  await expect(core).not.toHaveAttribute("open", "");
  await core.locator(":scope > summary").click();
  await expect(core.getByText("Reward address")).toBeVisible();

  await section.getByRole("button", { name: "About Protocol commitments" }).click();
  await expect(page.locator("#auxpow-dialog-title")).toHaveText("Protocol commitments");
  await expect(page.locator("#auxpow-dialog-body")).toContainText("A declaration names a protocol account.");
});

test("a malformed declaration names no account and shows its raw script", async ({ page }) => {
  const section = await openDrawer(page, {
    outputs: "decoded",
    entries: [{
      protocol: "core",
      vout: 3,
      decode_status: "malformed",
      malformed_reason: "nested_header",
      version: 1,
      script_hex: "6a2d434f5245016a2d434f524501",
      evidence_source: "bitcoin_core",
      upstream_selected: true,
      core_validator: null,
      core_reward_address: null,
      core_binding_hash: null,
      exsat_account: null,
      declared_account_pool: { id: null, slug: null, name: "Unknown", known: false },
    }],
  });
  await expect(section).toContainText("malformed");
  await expect(section).toContainText("Nested header");
  await expect(section).toContainText("none: not a valid declaration");
  await expect(section.locator("details.collapse")).toHaveAttribute("open", "");
});

test("a CORE declaration Core DAO does not read says so", async ({ page }) => {
  const [core] = fixture.protocol_commitments.entries;
  const section = await openDrawer(page, { outputs: "decoded", entries: [{ ...core, upstream_selected: false }] });
  await expect(section).toContainText("not read by Core DAO");
});

const states = [
  ["not derived yet", null, "Not derived yet."],
  ["no declarations", { outputs: "decoded", entries: [] }, "No CORE or exSat declaration in this coinbase."],
  ["outputs not stored", { outputs: "unavailable", entries: [] }, "coinbase outputs are not stored"],
  ["undecodable outputs", { outputs: "undecodable", entries: [] }, "could not be decoded"],
];

for (const [name, protocolCommitments, text] of states) {
  test(`the protocol commitments section says when ${name}`, async ({ page }) => {
    const section = await openDrawer(page, protocolCommitments);
    await expect(section.locator(".empty")).toContainText(text);
  });
}

test("a tree block counts its declarations and its card lists them below the chains", async ({ page }) => {
  // F2Pool's CORE and exSat declarations.
  const declarations = treeFixture.nodes.find((node) => node.protocol_commitments?.length === 2).protocol_commitments;
  const nodes = [
    makeNode(HASH, 700000, null, "canonical", {
      id: 1,
      prev_id: null,
      child_chain_evidence: [{ source: "auxpow:namecoin", child_chain: "namecoin", event_count: 1, child_height_min: 600000, child_height_max: 600000 }],
      protocol_commitments: declarations,
    }),
    makeNode("cf".repeat(32), 700001, HASH, "canonical", { id: 2, prev_id: 1, protocol_commitments: [] }),
  ];
  await stubApi(page, [], { treePayload: (params) => treeEnvelope(params, { nodes }) });
  await page.goto(`/?selected=${HASH}`);

  const badges = page.locator("g.tree-node .tree-badge-protocols");
  await expect(badges).toHaveCount(1);
  await expect(badges).toHaveText("2");
  const card = page.locator(".tree-chain-card");
  await expect(card.locator(".tree-chain-card-title")).toHaveText(["merge-mined chains (1)", "protocol commitments (2)"]);
  await expect(card).toContainText("F2Pool");
  await expect(card).toContainText("associated pool, not the block's miner");
  await expect(card.locator(".tree-protocol-swatch-core")).toHaveCount(1);
  await expect(card.locator(".tree-protocol-swatch-exsat")).toHaveCount(1);
});
