# Changelog

All notable public changes to this project will be documented in this file.

This changelog starts with the initial release.

## [Unreleased]

- Attribute Qbit payouts to F2Pool at high confidence and SECPOOL/Luxor at medium confidence, retaining separate child tags and leaving the shared wallet unmapped.
- Enrich stored Qbit payout evidence through source-scoped pool replay with progress reporting and no additional node calls.

## [0.9.3] - 2026-10-18

- Attribute Qbit child miners from coinbase tags authenticated against the child block header.
- Decode Qbit child coinbase heights without Bitcoin's acquisition ceiling.

## [0.9.2] - 2026-10-07

- Refresh the historical manifest and error catalogue from the Kraft-based
  Research publication, including expanded Huntercoin and Xaya evidence with
  authenticated native Xaya block heights.
- Replace superseded Research claims for Live chains without removing their
  observations, proofs, or attribution, and queue changed publication
  attestation gates without reconciling unchanged parents.
- Preserve separately imported operator evidence during authoritative historical
  publication cleanup, and skip matching reruns without replaying that evidence.
- Explain error blocks whose authenticated ancestry extends an invalid predecessor.

## [0.9.1] - 2026-09-29

- Refuse non-Bitcoin AuxPoW parents at a single lineage gate in the shared
  capture seam before classification or any Bitcoin Core call, counting refusals
  on poll ticks and backfills (`non_bitcoin`, `non_bitcoin_parent`). Parents
  needing an uncached epoch hold the height, and any event an earlier capture
  held for that child block is deleted.
- Delete derived `block` rows and their AuxPoW proofs once no evidence attests
  them, instead of keeping empty `unknown` blocks.
- Classify BTC orphans with the same shared lineage rule, falling back to the
  header timestamp rule when a coinbase height lies outside the timestamp-selected
  epoch.
- Remove the private Elastos and Hathor lineage checks, fresh-tip horizon gates,
  and reversible non-BTC/conflict revocations, and drop the obsolete Hathor
  `expected_btc_nbits` sidecar column (`0031`).
- Restrict `difficulty_epoch_ok` to `true` or `null` so a difficulty mismatch
  against a would-be competitor leaves the parent `unknown` without recording
  `false` or a Core-absence verdict.

## [0.9.0] - 2026-09-29

- Add Terracoin live capture (source 21, chain ID 50) through the shared AuxPoW
  runner, authenticating raw proofs against the observed block hash and recording
  `height_capture_failed` errors (`0030`) until the same height succeeds.
  Block detail projects Terracoin's chain ID, slot, classic proof, and
  `namecoin-aux` commitment on Terracoin-only parents.
- Separate each AuxPoW family's RPC dialect and capture-failure policy from
  offline classic AuxPoW authentication while preserving incumbent evidence
  semantics.

## [0.8.2] - 2026-09-22

- Import error witnesses before authoritative snapshot cleanup to preserve event
  identities and enrichment when observations move from stale inventories to the
  error catalogue, and reject cleanup that would delete a catalogued witness
  without error provenance.
- Classify reviewed body-invalid Bitcoin parents through the shared error
  catalogue (`kind=error_block`, `error_block_reason`), retiring the separate
  `body_invalid_stale` annotation table (`0029`), importer, and API fields.

## [0.8.1] - 2026-09-21

- Record the block hash and finality outcome each producer last observed at a
  child height in `child_chain_head` (`0026`) so trailing rescans skip the proof
  fetch and capture when the chain still carries a block with a final verdict.
  The lenient live classifier keeps a candidate `unknown` instead of attesting
  Core absence when a predecessor or competitor lookup fails.
- Track dispatched HTTP requests, retries, failures, and mean latency per RPC
  client via `RpcMetrics` in `mmm-rpc`, logging per-tick deltas on `poll tick`
  lines and periodic progress plus transport summaries on batch jobs through
  `ProgressReporter`. Pin the production classifier's Core request counts in a
  scripted RPC test fixture.
- Move scheduled unknown-parent rechecks out of producer startup and per-tick
  cache refreshes into a resumable `reclassify-unknown-parents --scheduled` job
  backed by generation-tracked cursor state (`0027`) and bounded cascade
  expansion (`0028`).
- Codify the remote round-trip budget, bounded batching, cursor resumability,
  and production-latency verification rules in `AGENTS.md` and `docs/testing.md`.

## [0.8.0] - 2026-09-16

- Register Qbit (source 36, AuxPoW chain ID 47) as a live source (`0002`, `0020`)
  and add its native extended-header merge-mining proof decoder (`poll-qbit`,
  `backfill-qbit`), authenticating native child placement against `getblockhash`
  and omitting classic-only `aux_proof.hash_block` in block detail.
- Persist per-height `capture_error` rows (`0021`) when a chain's
  `malformed_policy` holds a failed capture interval (enabled for Qbit) and
  surface the earliest unresolved child height as `auxpow_capture_error` on
  `/api/v1/sources`.
- Refresh the Research publication pin to `e3dc6d6`, adding Qbit's 2,540
  monitor-evidence rows across 29 chain artifacts and enforcing manifest-to-registry
  source equality during preflight.
- Record child-chain reorg displacement (`child_displaced_at` and
  `child_displaced_by`, `0022`–`0023`) via `mmm-store::record_child_chain_block`
  across the bitcoind-family, Elastos, and Hathor producers without revoking
  valid Bitcoin-side evidence, and project both fields on block `event_details[]`.
  Restore Hathor events previously revoked as superseded or voided (`0024`),
  retire the `supersede` reconcile marker (`0025`), and expose `<PREFIX>_REORG_DEPTH`
  across live producers.
- Verify bounded RSK backfills below the 139,999 live acquisition floor with a
  full-header fixture, evaluating uncles independently against the Bitcoin
  proof-of-work target.

## [0.7.13] - 2026-09-09

- Refresh the Research publication pin to `e09f52b` (1,283,863 ordinary events
  across 28 chain artifacts, 21 stale-descendant rows, and 88 error witnesses),
  adding I0coin, RSK, and SpaceXpanse ROD (`auxpow:rod`, source 35, `0018`) and
  marking I0coin's current chain status as unknown.
- Validate and retain Hathor's full parent coinbase transaction for strict BIP34
  eligibility across live capture, historical import, and scheduled orphan
  rechecks (`0019`).
- Validate historical `PowData` child targets (Xaya and ROD) from shared source
  registry metadata, requiring zero pure-header `nBits` and checking parent work
  against the publication's external target.

## [0.7.12] - 2026-09-03

- Surface body-invalid stale block annotations (`body_invalid_stale`, `0017`) for
  the two F2Pool `bad-blk-sigops` stale blocks at heights 783,426 and 784,121 in
  the API and UI without altering their `stale` classification.

## [0.7.11] - 2026-09-02

- Bulk-reconcile historical parents whose canonical classification is already
  proven by the local Bitcoin Core-backed block, retaining strict per-parent
  handling for stale, error, unknown, or inconsistent evidence.

## [0.7.10] - 2026-09-02

- Preserve observation timestamps when refreshing historical rows, and skip
  durable parent reconciliation when only publication provenance or
  presentation text changed.

## [0.7.9] - 2026-09-02

- Refresh stored parent coinbase-output text projections when the canonical
  Research publication updates its claim format, keeping binary outputs and full
  coinbase transactions immutable.

## [0.7.8] - 2026-09-02

- Reuse compatible, proven parent classifications from the derived `block`
  table when importing changed historical artifacts, avoiding redundant Bitcoin
  Core RPCs for Core-attested canonical and complete stale parents.

## [0.7.7] - 2026-09-01

- Pin Research's 1,037,005 ordinary events, 21-row stale-descendant aggregate,
  and 86 error observations covering 39 parents, skipping parent-only Namecoin
  rows until child identity is authenticated.
- Derive publication totals, parent-only counts, and observation-chain
  inventories from Research's manifest and refresh both Monitor pins from one
  revision via `just gen-research-publication-pins`.

## [0.7.6] - 2026-08-29

- Accept the research catalogue's legacy `median_time_past_violation` token as
  equivalent to a live `time_below_mtp` verdict while preserving the pinned
  token in imported evidence.

## [0.7.5] - 2026-08-29

- Determine `import-all` work by comparing normalized publication-owned state
  against stored provenance and events instead of cached artifact SHAs, and drop
  the `historical_import_artifact` receipt table (`0016`).

## [0.7.4] - 2026-08-28

- Pin the historical publication and compact error-block catalogue to
  `merge-mining-research` `c26e86c` (34-column monitor union including RSK
  sidecar columns) and add initial artifact-SHA import receipts (`0015`).

## [0.7.3] - 2026-08-28

- Show the active Go-to index position (`n of N`) in the navigator stepper across
  stales, stale branches, error blocks, orphans, and orphan branches.
- Replace the topbar Auto refresh interval picker and refresh icon with click-to-reload
  on the Updated stamp (retaining automatic 60s background refresh).
- Pin the error-block catalogue and historical publication to 35 parents and 78
  observations (580,320 historical events, including Hathor's 3,664-row file).

## [0.7.2] - 2026-08-27

- Clear stale Bitcoin Core link-error telemetry after Core revalidates the
  cached row and its predecessor link, including zero-work recovery batches.

## [0.7.1] - 2026-08-27

- Recover shallow Bitcoin Core forks after long follow-mode outages by repairing
  the bounded divergent suffix ending at the persisted cursor before resuming
  paged catch-up.

## [0.7.0] - 2026-08-25

- Import historical error-block child witnesses through a dedicated Core-required
  publication aggregate (`0014`), preserving their provenance and RSK sidecars
  and reclassifying affected parents to `error_block`.
- Derive `time_below_mtp` error blocks directly from Bitcoin Core by checking the
  eleven linked predecessor headers of Core-absent, PoW-valid parents (`0013`),
  and add `reclassify-parent` for targeted repair.
- Repair bounded near-tip Bitcoin reorgs in `sync-bitcoin-core --follow` via
  atomic divergent-suffix replacement and a restart-safe two-phase reconciliation
  queue (`bitcoin_core_reconcile_queue`, `0012`), retaining displaced blocks as
  stale evidence.
- Replace the compiled Bitcoin `nBits` epoch table with the sparse
  `bitcoin_core_header` Postgres cache (`0010`–`0011`) populated from a required
  Bitcoin Core node, and atomically repair strict/weak classifications on
  `import-known-stales` updates.

## [0.6.0] - 2026-08-15

- Redesign Findings figures into claim-led evidence panels with compact metrics,
  multi-series charts, discrete lollipops, annotations, and event timelines.
- Remove the `reclassify-pools` RSK skip watermark (`0009`) so each run scans
  the active RSK corpus directly instead of paying for two fingerprint scans.

## [0.5.0] - 2026-08-09

- Add `/api/v1/navigator/error-block` and the Go-to error-block target with
  plain-language consensus rejection explanations, per-rule help, and tree-window
  node-budget accounting, while rejecting out-of-range 32-bit height cursors.
- Add a Child Time help topic and parent-header timestamp offset display in the
  block detail drawer.
- Classify headers in the pinned error-block catalogue as `error_block` (`0008`)
  across the read model, API, tree controls, and source-health counts.
- Import the 576,662-row normalized research publication across 27 chains with
  independently nullable child height/hash/header/time/`nBits` evidence,
  source-row historical provenance, and a durable parent/cascade reconciliation
  queue (`0007`).

## [0.4.2] - 2026-07-30

- Bound the RSK miner-identity keyset scan on both sides of the evidence-to-event
  join so `reclassify-pools` runs linearly in corpus size instead of quadratically.

## [0.4.1] - 2026-07-29

- Bound and center the Findings canvas in a single `minmax(0, 900px)` column so
  wide screens keep readable card widths and a shared left edge.

## [0.4.0] - 2026-07-29

- Add the `data/findings/` content pipeline and `www/js/findings.generated.js`
  generator with schema, date, source-code, and citation validation.
- Add the Findings view (`?view=findings`) with month-grouped feed filtering,
  cited articles (`finding=<slug>`), theme-aware evidence figures, and cross-links
  to the header tree and source modal.
- Seed the initial six findings covering the September 2025 Foundry stale
  cluster, Hathor's hashrate collapse, Foundry's RSK and Fractal entry and exit,
  the Elastos exploit halt, and Terracoin's full-difficulty win.

## [0.3.0] - 2026-07-28

- Add the Header Time Delta view and `GET /api/v1/competitions` endpoint showing
  the header-timestamp distribution between stale blocks and their canonical
  competitors, with focus window, outlier gutters, Coverage and Table tabs, and
  social cards.
- Add the operator-imported `known_stale_block` membership (`0006`,
  `import-known-stales`, `reclassify-known-stales`) to exclude known stales from
  strict/weak BTC-orphan classification (`excluded`, `0005`).
- Extend `import-dataset` to the six live chains (including RSK sidecars) and
  align historical ingest vocabulary and BIP34 strict-chain checks with
  `merge-mining-research`.

## [0.2.1] - 2026-07-13

- Correct source-rail bylines and source-modal documentation for Bitcoin Stash,
  BLAST, Doichain, Fusioncoin, Jax.Network, Jincoin, Lyncoin, SixEleven, and
  VCash.

## [0.2.0] - 2026-07-11

- Import complete Lyncoin (11 canonical parents) and SixEleven (7 canonical
  parents) recoveries, VCash's 68 canonical explorer mappings, and Doichain's
  completed zero-row survey.
- Make source IDs permanent and retire ID 32 (`0004`), removing Mazacoin because
  its consensus source contains no AuxPoW implementation.

## [0.1.0] - 2026-07-02

- Initial public release.
