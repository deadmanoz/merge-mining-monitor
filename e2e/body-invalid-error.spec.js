const { expect, test } = require("@playwright/test");
const { blockPayload, makeNode, stubApi, treeEnvelope } = require("./support/api-stubs");

const HASH = "bd".repeat(32);
const cases = [
  ["bad-blk-sigops", "Too many sigops", "signature-operation cost"],
  ["block-script-verify-flag-failed", "Script verification failed", "redeem scripts"],
  ["bad-cb-amount", "Coinbase overpayment", "subsidy plus its transaction fees"],
  ["bad-txns-inputs-missingorspent", "Missing or spent input", "missing or already-spent input"],
];

for (const [reason, label, explanation] of cases) {
  test(`body-invalid ${reason} is an error block in the drawer and tree`, async ({ page }) => {
    const node = makeNode(HASH, 700000, null, "error_block", { id: 1, prev_id: null });
    await stubApi(page, [], {
      treePayload: (params) => treeEnvelope(params, { nodes: [node] }),
      blockPayload: () => {
        const payload = blockPayload(HASH);
        payload.block = { ...payload.block, kind: "error_block", error_block_reason: reason };
        return payload;
      },
    });
    await page.goto("/?tree_height=700000");
    const errorNode = page.locator('g.tree-node[aria-label*="error_block 700000"]');
    await expect(errorNode).toHaveCount(1);
    await errorNode.click();
    const drawer = page.locator("#drawer");
    await expect(drawer.locator(".state-pill.kind-error_block")).toHaveText("error_block");
    await expect(drawer).toContainText("Consensus rejection");
    await expect(drawer).toContainText(label);
    await expect(drawer).not.toContainText("Body validity");
    await expect(drawer).toContainText("never raced");
    await drawer.locator(`[data-consensus-rule-info="${reason}"]`).click();
    await expect(page.locator("#consensus-rule-dialog")).toContainText(explanation);
  });
}
