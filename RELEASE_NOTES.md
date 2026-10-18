# Release Notes

## [0.9.3] - 2026-10-18

- Show Qbit miner attribution from authenticated child coinbase tags, independently of the Bitcoin parent miner.

## [0.9.2] - 2026-10-07

- Use the expanded Huntercoin and Xaya evidence recovered from Daniel Kraft's native blockchain dumps, including authenticated native Xaya block heights.
- Refresh historical Research claims while preserving independent live observations and separately imported operator evidence.
- Update the error catalogue to 53 Bitcoin parents and 121 child-chain observations, and explain errors inherited from an invalid predecessor.
- Keep the About dialog's release-note rows readable when the list scrolls.

## [0.9.1] - 2026-09-29

- Refuse merge-mined parents from other SHA-256 chains (mostly Bitcoin Cash) by
  checking each parent against Bitcoin's difficulty history before storing it,
  and remove over 52,000 non-Bitcoin `unknown` blocks stored by earlier captures.
- Remove Bitcoin blocks and AuxPoW proofs once no evidence attests them, and
  apply the same Bitcoin difficulty-history check to orphan classification.

## [0.9.0] - 2026-09-29

- Track Terracoin as a live source with continuous AuxPoW block authentication and per-height capture-error tracking.
- Show Terracoin's chain ID, slot, and proof in block details, and surface the merge-mining commitment on Bitcoin blocks seen only through Terracoin.

## [0.8.2] - 2026-09-22

- Surface ten reviewed body-invalid Bitcoin parents as error blocks with their
  rejection reasons instead of stales, expanding the unified catalogue to 49
  invalid parents and 107 child-chain observations.
- Preserve observation identities when parents move into the error catalogue, and
  unify body-invalid reporting under `kind=error_block` and `error_block_reason`.

## [0.8.1] - 2026-09-21

- Rescan trailing child-chain heights with a single block-hash lookup when the height's recorded block and final verdict are unchanged.
- Run scheduled unknown-parent rechecks in a resumable background pass instead of blocking producer startup or per-tick cache refreshes.
- Log per-tick and batch-job progress with RPC request counts and latency summaries across every client.

## [0.8.0] - 2026-09-16

- Track Qbit as a live source and surface unresolved child-height decode errors on `/api/v1/sources`.
- Preserve Bitcoin-side evidence when a child-chain reorg replaces a block, surfacing displacement details on block events instead of revoking them.
- Capture complete Bitcoin parent-header evidence during bounded RSK backfills below the live acquisition floor, evaluating each uncle independently.

## [0.7.13] - 2026-09-09

- Import the expanded Research publication, including SpaceXpanse ROD's authenticated evidence and external-target validation for Xaya.
- Mark i0coin's current chain status as Unknown because snapshot timestamps establish an evidence boundary rather than live network availability.

## [0.7.12] - 2026-09-03

- Annotate the two F2Pool `bad-blk-sigops` stale blocks (heights 783,426 and 784,121) as body-invalid with rule help and an evidence link.

## [0.7.11] - 2026-09-02

- Bulk-reconcile historical parents whose canonical status and event evidence already agree with Bitcoin Core.

## [0.7.10] - 2026-09-02

- Preserve existing observation times during historical publication refreshes and skip parent reconciliation when only provenance or presentation text changed.

## [0.7.9] - 2026-09-02

- Refresh canonical parent coinbase-output text during historical imports while keeping binary outputs and full coinbase transactions immutable.

## [0.7.8] - 2026-09-02

- Reuse compatible, proven parent classifications during historical imports to avoid redundant Bitcoin Core RPCs.

## [0.7.7] - 2026-09-01

- Import Research's canonical publication: 1,037,005 ordinary events, 21
  stale-descendant summaries, and 86 authenticated error observations covering
  39 consensus-invalid Bitcoin parents.
- Verify all 29 publication artifacts before database mutation and refresh the
  historical manifest and error catalogue from the same Research commit.

## [0.7.6] - 2026-08-29

- Accept pinned error observations using the legacy `median_time_past_violation` reason when Bitcoin Core reports the equivalent `time_below_mtp` verdict.

## [0.7.5] - 2026-08-29

- Detect already-loaded historical publications by comparing normalized database state against stored provenance and events instead of cached receipt hashes.

## [0.7.4] - 2026-08-28

- Pin the historical publication and error-block catalogue to Research `c26e86c` (78 observations across 35 parents in the 34-column monitor union) and skip unchanged artifacts during full imports.

## [0.7.3] - 2026-08-28

- Show the active Go-to item position (`n of N`) while stepping through stales, stale branches, error blocks, orphans, and orphan branches.
- Reload the active view from the Updated stamp while keeping automatic 60s refresh.
- Expand the pinned error-block catalogue and historical publication to 35 parents, 78 error observations, and 580,320 historical events.

## [0.7.2] - 2026-08-27

- Clear recovered Bitcoin Core link errors after revalidating the cached block and its predecessor link.

## [0.7.1] - 2026-08-27

- Recover shallow Bitcoin Core forks after long outages by repairing the divergent suffix at the persisted cursor before resuming paged catch-up.

## [0.7.0] - 2026-08-25

- Evaluate difficulty and timestamp rules from a sparse canonical-header cache populated by Bitcoin Core instead of a compiled epoch table.
- Repair bounded near-tip Bitcoin reorgs atomically, retaining displaced blocks as stale evidence and completing dependent reconciliation across restarts.
- Import 73 recovered child-chain witnesses for 33 catalogued consensus-invalid Bitcoin error blocks and surface them in the error-block navigator.

## [0.6.0] - 2026-08-15

- Present each finding with evidence figures tailored to its claim, including paired 14-month weekly RSK and Fractal series for Foundry USA's entry and exit.
- Rescan the active RSK attribution corpus on every pool-resolution run.

## [0.5.0] - 2026-08-09

- Step through catalogued error blocks from the Go to menu and explain each broken Bitcoin consensus rule in plain language.

## [0.4.2] - 2026-07-30

- Complete the RSK pool-reclassification pass in minutes instead of days.

## [0.4.1] - 2026-07-29

- Bound and center the Findings feed and article on wide screens.

## [0.4.0] - 2026-07-29

- Add the Findings view: a month-grouped feed of cited articles with evidence figures and links to the header tree and source details.
- Seed six findings covering the September 2025 Foundry stale cluster, Hathor's hashrate collapse, Foundry USA's RSK and Fractal entry and exit, the Elastos exploit halt, and Terracoin's full-difficulty win.

## [0.3.0] - 2026-07-28

- Add the Header Time Delta view comparing header timestamps between stale blocks and their canonical competitors across histogram, coverage, and table tabs.
- Exclude catalogued `bitcoin-data/stale-blocks` members from strict and weak BTC-orphan classification (`excluded`), and align evidence state vocabulary to `unknown`.
- Support historical dataset imports for the six live chains (including RSK sidecar evidence) and add Open Graph social cards.

## [0.2.1] - 2026-07-13

- Correct source-rail bylines and source profiles for nine catalogued or recovered chains with historical date ranges, key facts, and citations.

## [0.2.0] - 2026-07-11

- Add complete Lyncoin (11 canonical parents) and SixEleven (7 canonical parents) recoveries, VCash's 68 canonical explorer mappings, and Doichain's completed zero-row survey.
- Make source IDs permanent and remove Mazacoin after verifying its source contains no AuxPoW implementation.

## [0.1.0] - 2026-07-02

- Initial source release of `merge-mining-monitor`, including the Rust
  workspace, Postgres schema, capture and reconciliation pipeline, read API,
  and static frontend.
