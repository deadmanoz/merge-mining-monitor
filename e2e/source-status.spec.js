const { expect, test } = require("@playwright/test");
const {
  GENERATED_AT,
  sourcesPayload,
  stubApi,
  treeEnvelope,
  versionPayload,
} = require("./support/api-stubs");

function sourceFixture() {
  const base = {
    kind: "auxpow",
    instance: null,
    created_at: 1700000000,
    counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
  };
  return sourcesPayload([
      {
        ...base,
        id: 1,
        code: "auxpow:namecoin",
        chain: "namecoin",
        last_seen_at: GENERATED_AT - 60,
        status: "fresh",
        sync: {
          mode: "live",
          state: "live",
          progress_height: 700000,
          progress_updated_at: GENERATED_AT - 60,
          target_height: null,
          latest_evidence_at: GENERATED_AT - 60,
          error_code: null,
          error_height: null,
        },
        counts: { events: 4, near: 1, unknown: 5, canonical: 1, stale: 1, strict_orphan: 3, weak_orphan: 2 },
      },
      {
        ...base,
        id: 2,
        code: "auxpow:rsk",
        chain: "rsk",
        last_seen_at: GENERATED_AT - 604801,
        status: "stale",
        sync: {
          mode: "live",
          state: "stale",
          progress_height: 500000,
          progress_updated_at: GENERATED_AT - 604801,
          target_height: null,
          latest_evidence_at: GENERATED_AT - 604801,
          error_code: null,
          error_height: null,
        },
        counts: { events: 2, near: 1, unknown: 1, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        id: 3,
        code: "live-chaintip:bitcoin:core",
        kind: "live-chaintip",
        chain: "bitcoin",
        instance: "core",
        created_at: 1700000000,
        last_seen_at: null,
        status: "not_started",
        sync: {
          mode: "bitcoin-core-backbone",
          state: "catching_up",
          progress_height: 699998,
          progress_updated_at: GENERATED_AT - 120,
          target_height: 700000,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 4,
        code: "auxpow:syscoin",
        chain: "syscoin",
        last_seen_at: GENERATED_AT - 300,
        status: "stale",
        sync: {
          mode: "live",
          state: "error",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: "rpc_failed",
          error_height: 600000,
        },
        counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 5,
        code: "auxpow:fractal",
        chain: "fractal",
        last_seen_at: GENERATED_AT - 90,
        status: "fresh",
        sync: {
          mode: "live",
          state: "catching_up",
          progress_height: 800000,
          progress_updated_at: GENERATED_AT - 90,
          target_height: 800050,
          latest_evidence_at: GENERATED_AT - 90,
          error_code: null,
          error_height: null,
        },
        counts: { events: 6, near: 1, unknown: 1, canonical: 3, stale: 1, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 8,
        code: "auxpow:argentum",
        chain: "argentum",
        last_seen_at: null,
        status: "not_started",
        sync: {
          mode: "historical",
          state: "historical",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 21,
        code: "auxpow:terracoin",
        chain: "terracoin",
        last_seen_at: null,
        status: "not_started",
        sync: {
          mode: "historical",
          state: "historical",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 24,
        code: "auxpow:vcash",
        chain: "vcash",
        last_seen_at: GENERATED_AT - 604801,
        status: "stale",
        sync: {
          mode: "partial",
          state: "partial",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 68, near: 0, unknown: 0, canonical: 68, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 25,
        code: "auxpow:lyncoin",
        chain: "lyncoin",
        last_seen_at: 1721667253,
        status: "stale",
        sync: {
          mode: "historical",
          state: "historical",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 11, near: 0, unknown: 0, canonical: 11, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 27,
        code: "auxpow:sixeleven",
        chain: "sixeleven",
        last_seen_at: 1536793971,
        status: "stale",
        sync: {
          mode: "historical",
          state: "historical",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 7, near: 0, unknown: 0, canonical: 7, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 29,
        code: "auxpow:doichain",
        chain: "doichain",
        last_seen_at: null,
        status: "not_started",
        sync: {
          mode: "surveyed",
          state: "surveyed",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
      {
        ...base,
        id: 33,
        code: "auxpow:bitcoin-stash",
        chain: "bitcoin-stash",
        last_seen_at: null,
        status: "not_started",
        sync: {
          mode: "catalogued",
          state: "catalogued",
          progress_height: null,
          progress_updated_at: null,
          target_height: null,
          latest_evidence_at: null,
          error_code: null,
          error_height: null,
        },
        counts: { events: 0, near: 0, unknown: 0, canonical: 0, stale: 0, strict_orphan: 0, weak_orphan: 0 },
      },
    ]);
}

function versionFixture() {
  return versionPayload({
    release_notes: {
      source: "RELEASE_NOTES.md",
      release_count: 2,
      truncated: false,
      releases: [
        {
          version: "Unreleased",
          items: ["Shows the running monitor version in the About dialog."],
          item_count: 1,
          truncated: false,
        },
        {
          version: "0.1.0",
          date: "2026-06-23",
          items: ["Released the first monitor build."],
          item_count: 1,
          truncated: false,
        },
      ],
    },
  });
}

async function stubCommonApi(page) {
  await stubApi(page, [], {
    treePayload: () => treeEnvelope(new URLSearchParams(), {
      query: {
        from_height: null,
        to_height: null,
        at_height: null,
        at_time: null,
        window_mode: "tip",
        context: "exact",
      },
      window: {
        btc_height_min: 700000,
        btc_height_max: 700000,
        tip_height: 700000,
        defaulted_to_tip: true,
      },
      nodes: [],
    }),
    versionPayload: versionFixture(),
  });
}

function sourceOptionByName(page, name) {
  return page.locator(".source-option").filter({
    has: page.locator(".source-name-text", { hasText: new RegExp(`^${name}$`) }),
  });
}

test("renders source capture progress in the topbar, popover, and source rail", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: sourceFixture() });
  });

  await page.goto("/");

  const button = page.locator("#source-status-button");
  await expect(button).toContainText("Sources 1 error");
  await expect(button).toContainText("1 error");
  await expect(button).toContainText("2 catching up");
  await expect(button).toContainText("1 stale");
  await expect(button).toContainText("1 live");

  await expect(sourceOptionByName(page, "Namecoin").locator(".source-sync-state-live")).toBeVisible();
  await expect(sourceOptionByName(page, "RSK").locator(".source-sync-state-stale")).toBeVisible();
  await expect(sourceOptionByName(page, "Bitcoin").locator(".source-sync-state-catching-up")).toBeVisible();
  await expect(sourceOptionByName(page, "Fractal Bitcoin").locator(".source-sync-state-catching-up")).toBeVisible();
  await expect(sourceOptionByName(page, "Syscoin").locator(".source-sync-state-error")).toBeVisible();
  const historicalOption = sourceOptionByName(page, "Argentum");
  await expect(historicalOption).toHaveCount(1);
  await expect(historicalOption.locator(".source-sync-mark")).toHaveCount(0);

  await button.click();
  const popover = page.getByRole("dialog", { name: "Source capture status" });
  await expect(popover).toBeVisible();
  await expect(popover).toContainText("Height");
  await expect(popover).toContainText("Cursor Updated");
  await expect(popover).toContainText("Latest Evidence");
  await expect(popover.locator(".source-status-header-help").nth(0)).toHaveAttribute(
    "title",
    "When the progress cursor was last seeded or advanced. This is not a process heartbeat.",
  );
  await expect(popover.locator(".source-status-header-help").nth(1)).toHaveAttribute(
    "title",
    "Newest accepted AuxPoW evidence timestamp. This can be older than the live cursor when recent blocks are near shares or non-BTC parents.",
  );
  await expect(popover).not.toContainText("Capture Cursor");
  await expect(popover).not.toContainText("live capture: live");
  await expect(popover).not.toContainText("auxpow:namecoin");

  const namecoinRow = popover.getByRole("row", { name: /Namecoin/ });
  await expect(namecoinRow).toContainText("Live");
  await expect(namecoinRow).toContainText("700,000");

  const rskRow = popover.getByRole("row", { name: /RSK/ });
  await expect(rskRow).toContainText("Stale");

  const fractalRow = popover.getByRole("row", { name: /Fractal/ });
  await expect(fractalRow).toContainText("Catching up");
  await expect(fractalRow.locator(".source-status-height")).toHaveText("800,000 / 800,050");

  const bitcoinRow = popover.getByRole("row", { name: /^Bitcoin\b/ });
  await expect(bitcoinRow).toContainText("Catching up");
  await expect(bitcoinRow).toContainText("699,998");
  await expect(bitcoinRow).toContainText("N/A");
  await expect(bitcoinRow.locator(".source-status-height")).toHaveText("699,998");
  await expect(bitcoinRow.locator(".source-status-height")).toHaveAttribute("title", /Target height: 700,000/);

  const syscoinRow = popover.getByRole("row", { name: /Syscoin/ });
  await expect(syscoinRow).toContainText("Error");
  await expect(syscoinRow).toContainText("No height");
  await expect(syscoinRow).toContainText("No update");
  await expect(popover).toContainText("error rpc_failed at 600,000");
  await expect(syscoinRow).toContainText("No evidence");
  await expect(popover).not.toContainText("Argentum");
});

test("renders version metadata and release notes in the about dialog", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: { schema_version: "v1", generated_at: GENERATED_AT, sources: [] } });
  });

  await page.goto("/");
  await expect(page.locator('link[href^="/css/about-version.css"]')).toHaveAttribute(
    "href",
    "/css/about-version.css?v=20260702-footer-links",
  );
  await page.getByRole("button", { name: "About this monitor" }).click();

  const dialog = page.getByRole("dialog", { name: "About This Monitor" });
  await expect(dialog).toBeVisible();
  // The modal opens on the Overview tab: version and credit are shown there.
  const credit = dialog.locator(".about-credit-by");
  const creditLines = credit.locator(".about-credit-line");
  await expect(creditLines).toHaveCount(2);
  await expect(creditLines.nth(0).locator("#about-version")).toHaveText("v0.1.0");
  await expect(creditLines.nth(0)).toContainText("Source code");
  await expect(creditLines.nth(1)).toContainText("Built by deadmanoz");
  const identityTextBox = await creditLines.nth(1).getByText("Built by deadmanoz").boundingBox();
  const socialLinks = creditLines.nth(1).locator(".about-social-link");
  await expect(socialLinks).toHaveCount(3);
  expect(identityTextBox).not.toBeNull();
  for (let index = 0; index < 3; index += 1) {
    const linkBox = await socialLinks.nth(index).boundingBox();
    expect(linkBox).not.toBeNull();
    const identityCenterY = identityTextBox.y + identityTextBox.height / 2;
    const linkCenterY = linkBox.y + linkBox.height / 2;
    expect(Math.abs(linkCenterY - identityCenterY)).toBeLessThan(6);
  }
  await expect(dialog.getByRole("link", { name: "Source code" })).toHaveAttribute(
    "href",
    "https://github.com/deadmanoz/merge-mining-monitor",
  );
  await expect(dialog.getByRole("link", { name: "deadmanoz website" })).toHaveAttribute(
    "href",
    "https://deadmanoz.xyz",
  );
  await expect(dialog.getByRole("link", { name: "deadmanoz on Nostr" })).toHaveAttribute(
    "href",
    "https://primal.net/deadmanoz",
  );
  await expect(dialog.getByRole("link", { name: "deadmanoz on X" })).toHaveAttribute(
    "href",
    "https://x.com/ozdeadman",
  );

  // Release notes live in their own tab as a per-release collapsible accordion.
  await dialog.getByRole("tab", { name: "Release notes" }).click();
  const notes = dialog.locator("#about-release-notes-body");
  await expect(notes).toContainText("Unreleased");
  await expect(notes).toContainText("v0.1.0");

  // The newest section is expanded by default; older sections start collapsed.
  await expect(notes.getByText("Shows the running monitor version")).toBeVisible();
  const olderEntry = notes.getByText("Released the first monitor build.");
  await expect(olderEntry).toBeHidden();

  await dialog.getByRole("button", { name: /v0\.1\.0/ }).click();
  await expect(olderEntry).toBeVisible();
});

test("surfaces source registry request failures", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({
      status: 500,
      json: {
        error: {
          code: "internal_error",
          message: "source registry unavailable",
          details: {},
        },
      },
    });
  });

  await page.goto("/");

  const button = page.locator("#source-status-button");
  await expect(button).toContainText("Sources unavailable");
  await expect(page.locator("#source-controls")).toContainText("No source registry");

  await button.click();
  await expect(page.getByRole("dialog", { name: "Source capture status" })).toContainText("Source registry unavailable");
});

test("source info dialog Capture tab shows derivation, provenance, operations, and counts", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: sourceFixture() });
  });

  await page.goto("/");

  await page.locator('.source-info-button[data-source-info="auxpow:namecoin"]').click();

  const dialog = page.locator("#source-dialog");
  await expect(dialog).toBeVisible();
  await dialog.locator('.modal-tab[data-tab="capture"]').click();
  const capture = page.locator("#sd-panel-capture");
  await expect(capture).toBeVisible();
  // Derivation + provenance moved here from the Technical tab.
  await expect(capture).toContainText("How this monitor derives it");
  await expect(capture).toContainText("Provenance & coverage");
  await expect(capture).toContainText("Operational status");
  await expect(capture).toContainText("Current Evidence Counts");
  await expect(capture.locator('dt:text-is("Evidence")')).toHaveCount(1);
  await expect(capture.locator('dt:text-is("Pool attribution")')).toHaveCount(1);
  await expect(capture.locator('dt:text-is("Status")')).toHaveCount(1);
  await expect(capture.locator('dt:text-is("Status") + dd')).toHaveText("Live");
  await expect(capture.locator('dt:text-is("Progress")')).toHaveCount(1);
  await expect(capture.locator('dt:text-is("Progress") + dd')).toHaveText("700,000");
  await expect(capture.locator('dt:text-is("Updated")')).toHaveCount(1);

  // The Current Evidence Counts rows are Events / Canonical / Stale / Strict
  // orphans / Weak orphans.
  for (const label of ["Events", "Canonical", "Stale", "Strict orphans", "Weak orphans"]) {
    await expect(capture.locator("dt").filter({ hasText: new RegExp(`^${label}$`) })).toHaveCount(1);
  }
  // Strict/weak values map straight from counts.strict_orphan / counts.weak_orphan.
  await expect(capture.locator('dt:text-is("Strict orphans") + dd')).toHaveText("3");
  await expect(capture.locator('dt:text-is("Weak orphans") + dd')).toHaveText("2");

  await expect(capture.locator('dt:text-is("Unknown")')).toHaveCount(0);
  await expect(capture.locator('dt:text-is("Near")')).toHaveCount(0);

  for (const removedLabel of [
    "Source Code",
    "Role",
    "Trust",
    "Capability",
    "Data Used",
    "Miner Attribution",
    "Capture progress",
    "Sync Mode",
    "Sync State",
    "Progress Height",
    "Target Height",
  ]) {
    await expect(capture.locator("dt").filter({ hasText: new RegExp(`^${removedLabel}$`) })).toHaveCount(0);
  }

  await page.locator("#source-dialog-close").click();
  await page.locator('.source-info-button[data-source-info="live-chaintip:bitcoin:core"]').click();
  await page.locator('.modal-tab[data-tab="capture"]').click();
  const bitcoinCapture = page.locator("#sd-panel-capture");
  await expect(bitcoinCapture).toContainText("Placement");
  await expect(bitcoinCapture).toContainText("Operational status");
  await expect(bitcoinCapture).not.toContainText("Current Evidence Counts");
  await expect(bitcoinCapture.locator('dt:text-is("Status") + dd')).toHaveText("Catching up");
  await expect(bitcoinCapture.locator('dt:text-is("Progress") + dd')).toHaveText("699,998 / 700,000");
});

test("source info dialog renders three tabs and switches panels", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: sourceFixture() });
  });

  await page.goto("/");
  await page.locator('.source-info-button[data-source-info="auxpow:namecoin"]').click();

  const dialog = page.locator("#source-dialog");
  await expect(dialog).toBeVisible();
  for (const label of ["General", "Technical", "Capture"]) {
    await expect(dialog.locator(".modal-tab").filter({ hasText: label })).toHaveCount(1);
  }
  // General is the default panel; the others start hidden.
  await expect(page.locator("#sd-panel-history")).toBeVisible();
  await expect(page.locator("#sd-panel-technical")).toBeHidden();
  await expect(page.locator("#sd-panel-capture")).toBeHidden();
  // Real generated CHAIN_PROFILES content is present (not a blank section).
  await expect(page.locator("#sd-panel-history")).toContainText("Live AuxPoW producer");
  await expect(page.locator("#sd-panel-history")).toContainText(/\S/);

  await dialog.locator('.modal-tab[data-tab="technical"]').click();
  await expect(page.locator("#sd-panel-technical")).toBeVisible();
  await expect(page.locator("#sd-panel-history")).toBeHidden();
  await expect(page.locator("#sd-panel-technical")).toContainText("What's distinctive");
  // Derivation, provenance, and the recovery-yield "why it matters" content now
  // live in the Capture tab; Technical is pure chain-science.
  await expect(page.locator("#sd-panel-technical")).not.toContainText("Provenance & coverage");
  await expect(page.locator("#sd-panel-technical")).not.toContainText("Why it matters for Bitcoin");
  await dialog.locator('.modal-tab[data-tab="capture"]').click();
  await expect(page.locator("#sd-panel-capture")).toContainText("How this monitor derives it");
  await expect(page.locator("#sd-panel-capture")).toContainText("Provenance & coverage");
  await expect(page.locator("#sd-panel-capture")).toContainText("Recovery");
});

test("relationship chip distinguishes every public source lifecycle", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: sourceFixture() });
  });
  await page.goto("/");

  const history = page.locator("#sd-panel-history");
  const closeDialog = () => page.locator("#source-dialog-close").click();

  // Live AuxPoW producer.
  await page.locator('.source-info-button[data-source-info="auxpow:namecoin"]').click();
  await expect(history).toContainText("Live AuxPoW producer");
  await closeDialog();

  // Bitcoin Core parent chain (not a producer).
  await page.locator('.source-info-button[data-source-info="live-chaintip:bitcoin:core"]').click();
  await expect(history).toContainText("Bitcoin Core parent chain");
  await closeDialog();

  // Recovered dataset (the recovered/historical group is collapsed by default;
  // the group key is still "historical", only its label changed).
  await page.locator('details[data-source-group="historical"] > summary').click();
  await page.locator('.source-info-button[data-source-info="auxpow:lyncoin"]').click();
  await expect(history).toContainText("Recovered dataset");
  await expect(history).toContainText(/complete Bitcoin-merge-mined era/i);
  await page.locator("#sd-tab-capture").click();
  const lyncoinCapture = page.locator("#sd-panel-capture");
  await expect(lyncoinCapture.locator('dt:text-is("Events") + dd')).toHaveText("11");
  await expect(lyncoinCapture.locator('dt:text-is("Canonical") + dd')).toHaveText("11");
  await expect(lyncoinCapture.locator('dt:text-is("Stale") + dd')).toHaveText("0");
  await closeDialog();

  await page.locator('.source-info-button[data-source-info="auxpow:sixeleven"]').click();
  await expect(history).toContainText("Recovered dataset");
  await expect(history).toContainText("full-chain recovery");
  await page.locator("#sd-tab-capture").click();
  const sixelevenCapture = page.locator("#sd-panel-capture");
  await expect(sixelevenCapture.locator('dt:text-is("Events") + dd')).toHaveText("7");
  await expect(sixelevenCapture.locator('dt:text-is("Canonical") + dd')).toHaveText("7");
  await expect(sixelevenCapture.locator('dt:text-is("Stale") + dd')).toHaveText("0");
  await closeDialog();

  // Partial recovered subset: selectable, with explicit partial scope and the
  // 68-row evidence count rather than a full-chain recovery claim.
  await page.locator('details[data-source-group="partial"] > summary').click();
  await page.locator('.source-info-button[data-source-info="auxpow:vcash"]').click();
  await expect(history).toContainText("Recovered subset");
  await page.locator("#sd-tab-capture").click();
  const partialCapture = page.locator("#sd-panel-capture");
  await expect(partialCapture).toContainText("Operational status");
  await expect(partialCapture).toContainText("Current Evidence Counts");
  await expect(partialCapture.locator('dt:text-is("Events") + dd')).toHaveText("68");
  await closeDialog();

  // Surveyed recovery: chain data was reviewed, but no admissible evidence
  // exists, so the row is disabled and Capture omits status/count blocks.
  await page.locator('details[data-source-group="surveyed"] > summary').click();
  const doichainOption = sourceOptionByName(page, "Doichain");
  await expect(doichainOption.locator('input[name="source"]')).toBeDisabled();
  await page.locator('.source-info-button[data-source-info="auxpow:doichain"]').click();
  await expect(history).toContainText("Recovered survey");
  await page.locator("#sd-tab-capture").click();
  const surveyedCapture = page.locator("#sd-panel-capture");
  await expect(surveyedCapture).not.toContainText("Operational status");
  await expect(surveyedCapture).not.toContainText("Current Evidence Counts");
  await closeDialog();

  // Catalogued (not recovered): its own greyed group; chain status is a scoped
  // row (not a chip); the Capture tab shows no operational block and no counts.
  await page.locator('details[data-source-group="catalogued"] > summary').click();
  await page.locator('.source-info-button[data-source-info="auxpow:bitcoin-stash"]').click();
  await expect(history).toContainText("Catalogued (not recovered)");
  await expect(history).toContainText("Chain status");
  await page.locator("#sd-tab-capture").click();
  const capture = page.locator("#sd-panel-capture");
  await expect(capture).not.toContainText("Operational status");
  await expect(capture).not.toContainText("Current Evidence Counts");
  await closeDialog();

  // The About-sources explainer opens from the Source legend help icon.
  await page.locator(".filter-legend-help").click();
  await expect(page.locator("#sources-about-dialog-body")).toContainText("Recovered subset");
  await expect(page.locator("#sources-about-dialog-body")).toContainText("Recovered survey");
  await expect(page.locator("#sources-about-dialog-body")).toContainText("Catalogued (not recovered)");
});

test("deep links retain partial sources and drop non-selectable lifecycles", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: sourceFixture() });
  });
  // Catalogued and surveyed sources are sanitized out, while the selectable
  // partial VCash subset remains checked.
  await page.goto("/?sources=auxpow:bitcoin-stash,auxpow:doichain,auxpow:vcash,auxpow:mazacoin");
  await page.locator('details[data-source-group="catalogued"] > summary').click();
  const catalogued = page.locator('input[name="source"][value="auxpow:bitcoin-stash"]');
  await expect(catalogued).toBeDisabled();
  await expect(catalogued).not.toBeChecked();
  await page.locator('details[data-source-group="surveyed"] > summary').click();
  const surveyed = page.locator('input[name="source"][value="auxpow:doichain"]');
  await expect(surveyed).toBeDisabled();
  await expect(surveyed).not.toBeChecked();
  await expect(page.locator('input[name="source"][value="auxpow:vcash"]')).toBeChecked();
  await expect(page.locator('input[name="source"][value="auxpow:mazacoin"]')).toHaveCount(0);
});

test("source modal renders code formatting, citations, and a Sources list", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: sourceFixture() });
  });
  await page.goto("/");

  // Terracoin (a Phase A authored chain) lives in the collapsed historical group.
  await page.locator('details[data-source-group="historical"] > summary').click();
  await page.locator('.source-info-button[data-source-info="auxpow:terracoin"]').click();

  const dialog = page.locator("#source-dialog");
  await expect(dialog).toBeVisible();
  await dialog.locator('.modal-tab[data-tab="technical"]').click();
  const technical = page.locator("#sd-panel-technical");
  await expect(technical).toBeVisible();

  // `backtick` markup renders as <code>.
  await expect(technical.locator("code").first()).toBeVisible();
  // Inline citation superscripts link out to the source in a new tab.
  const cite = technical.locator("sup.sd-cite a").first();
  await expect(cite).toHaveAttribute("target", "_blank");
  await expect(cite).toHaveAttribute("href", /^https?:\/\//);
  // The per-tab Sources list resolves the cited references to outbound links.
  await expect(technical.locator("ol.sd-sources a").first()).toHaveAttribute("href", /^https?:\/\//);
});

test("source modal renders authored history breaks as separate paragraphs", async ({ page }) => {
  await stubCommonApi(page);
  await page.route("**/api/v1/sources", async (route) => {
    await route.fulfill({ json: sourceFixture() });
  });
  await page.goto("/");

  await page.locator('.source-info-button[data-source-info="live-chaintip:bitcoin:core"]').click();
  await expect(page.locator("#sd-panel-history p:not(.sd-status-detail)")).toHaveCount(2);
  await page.locator("#source-dialog-close").click();

  await page.locator('.source-info-button[data-source-info="auxpow:namecoin"]').click();
  await expect(page.locator("#sd-panel-history p:not(.sd-status-detail)")).toHaveCount(3);
});
