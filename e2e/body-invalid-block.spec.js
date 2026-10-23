const { expect, test } = require("@playwright/test");
const { blockPayload, makeNode, stubApi, treeEnvelope } = require("./support/api-stubs");

const HASH = "bd".repeat(32);
const cases = [
  ["consensus_invalid_parent", "Invalid predecessor", "authenticated predecessor path"],
  ["bad-blk-sigops", "Too many sigops", "signature-operation cost"],
  ["block-script-verify-flag-failed", "Script verification failed", "redeem scripts"],
  ["bad-cb-amount", "Coinbase overpayment", "subsidy plus its transaction fees"],
  ["bad-txns-inputs-missingorspent", "Missing or spent input", "missing or already-spent input"],
];

for (const [reason, label, explanation] of cases) {
  test(`catalogued ${reason} is an invalid block in the drawer and tree`, async ({ page }) => {
    const node = makeNode(HASH, 700000, null, "invalid_block", { id: 1, prev_id: null });
    await stubApi(page, [], {
      treePayload: (params) => treeEnvelope(params, { nodes: [node] }),
      blockPayload: () => {
        const payload = blockPayload(HASH);
        payload.block = { ...payload.block, kind: "invalid_block", invalid_block_reason: reason };
        return payload;
      },
    });
    await page.goto("/?tree_height=700000");
    const invalidNode = page.locator('g.tree-node[aria-label*="invalid_block 700000"]');
    await expect(invalidNode).toHaveCount(1);
    await invalidNode.click();
    const drawer = page.locator("#drawer");
    await expect(drawer.locator(".state-pill.kind-invalid_block")).toHaveText("invalid_block");
    await expect(drawer).toContainText("Consensus rejection");
    await expect(drawer).toContainText(label);
    await expect(drawer).not.toContainText("Body validity");
    await expect(drawer).toContainText("never raced");
    await drawer.locator(`[data-consensus-rule-info="${reason}"]`).click();
    await expect(page.locator("#consensus-rule-dialog")).toContainText(explanation);
  });
}
