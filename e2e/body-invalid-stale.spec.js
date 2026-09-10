const { expect, test } = require("@playwright/test");
const { blockPayload, makeNode, stubApi, treeEnvelope } = require("./support/api-stubs");

const STALE_HASH = "bd".repeat(32);
const PLAIN_STALE_HASH = "ab".repeat(32);
const CANONICAL_HASH = "cc".repeat(32);
const EVIDENCE_URL = "https://b10c.me/observations/11-invalid-blocks-783426-and-784121/";

function annotatedBlockPayload(hash) {
  const payload = blockPayload(hash);
  payload.block.kind = "stale";
  payload.block.body_invalid = { rule: "bad-blk-sigops", evidence_url: EVIDENCE_URL };
  return payload;
}

test("the drawer surfaces a body-invalid annotation on a stale block without changing its kind", async ({
  page,
}) => {
  const nodes = [
    makeNode(CANONICAL_HASH, 700000, null, "canonical", { id: 1, prev_id: null }),
    makeNode(STALE_HASH, 700000, null, "stale", {
      id: 2,
      prev_id: null,
      body_invalid_rule: "bad-blk-sigops",
    }),
  ];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => annotatedBlockPayload(hash),
  });

  await page.goto(`/?selected=${STALE_HASH}`);
  const drawer = page.locator("#drawer");
  await expect(drawer).toContainText("Body validity");
  await expect(drawer).toContainText("Too many sigops (bad-blk-sigops)");
  // The kind row still reports an ordinary stale: the annotation never promotes.
  await expect(drawer.locator(".state-pill.kind-stale")).toHaveText("stale");
  const evidence = drawer.locator(`a[href="${EVIDENCE_URL}"]`);
  await expect(evidence).toHaveText("evidence");
  await expect(evidence).toHaveAttribute("target", "_blank");
  // The rule's info dialog opens from the row's help control.
  await drawer.locator('[data-consensus-rule-info="bad-blk-sigops"]').click();
  await expect(page.locator("#consensus-rule-dialog")).toContainText(
    "signature-operation cost",
  );
});

test("an unannotated stale drawer has no body-validity row", async ({ page }) => {
  const nodes = [
    makeNode(PLAIN_STALE_HASH, 700000, null, "stale", { id: 1, prev_id: null }),
  ];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => {
      const payload = blockPayload(hash);
      payload.block.kind = "stale";
      return payload;
    },
  });

  await page.goto(`/?selected=${PLAIN_STALE_HASH}`);
  const drawer = page.locator("#drawer");
  await expect(drawer).toContainText("Parent block (Bitcoin)");
  await expect(drawer).not.toContainText("Body validity");
});

test("the tree hover title carries the body-invalid annotation", async ({ page }) => {
  const nodes = [
    makeNode(CANONICAL_HASH, 700000, null, "canonical", { id: 1, prev_id: null }),
    makeNode(STALE_HASH, 700000, null, "stale", {
      id: 2,
      prev_id: null,
      body_invalid_rule: "bad-blk-sigops",
    }),
  ];
  await stubApi(page, [], {
    treePayload: (params) => treeEnvelope(params, { nodes }),
    blockPayload: (hash) => annotatedBlockPayload(hash),
  });

  await page.goto("/");
  // The hover affordance is the node group's SVG <title>; only the annotated
  // stale carries the body-invalid label.
  const annotatedTitle = page.locator("svg title", {
    hasText: `stale (body-invalid: bad-blk-sigops) 700000 ${STALE_HASH}`,
  });
  await expect(annotatedTitle).toHaveCount(1);
  await expect(page.locator("svg title", { hasText: "body-invalid" })).toHaveCount(1);
});
