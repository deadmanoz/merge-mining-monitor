# Changelog

All notable public changes to this project will be documented in this file.

This changelog starts with the initial release.

## [Unreleased]

## [0.7.5] - 2026-08-29

- Determine `import-all` work from normalized publication-owned database state
  instead of cached artifact SHAs. Skip matching event files and retained error
  observations before taking the Bitcoin Core lock, while still completing any
  pending reconciliation, source-health, or published-stale work. Remove the
  receipt seed, CLI flag, code paths, store APIs, and receipt table.

## [0.7.4] - 2026-08-28

- Pin the historical publication and compact error-block catalogue to
  merge-mining-research `c26e86c`. Error observations stay 78 rows across 35
  parents; the aggregate is now the 34-column monitor union (27 evidence
  columns plus the seven RSK sidecar columns).

- `import-all` records each artifact's content SHA after a successful
  publication finalize and skips classify, write, and authoritative reconcile
  when that SHA has not changed. `--seed-imported-receipts` loads the last
  imported pin (`091e01a`) only after matching event-scope provenance counts
  prove that pin is already present. `import-dataset` does not write receipts;
  a successful single-chain import deletes that chain's receipt so the next
  `import-all` re-runs authoritative reconcile.

## [0.7.3] - 2026-08-28

- Show which item of the current Go-to index the stepper is on (`n of N`)
  while walking stales, stale branches, error blocks, orphans, and orphan
  branches.

- Remove the Auto refresh interval picker and dedicated refresh icon from the
  topbar. Data still refreshes every 60s; click the Updated stamp to reload the
  current view, or Retry when that view has not loaded yet.

- Pin the compact error-block catalogue and historical publication to the same
  merge-mining-research commit. Error observations are now 78 rows across 35
  parents, including Hathor-witnessed 649674 and the 2026 F2Pool
  `time_below_mtp` twin at 957780. That research commit already publishes
  Hathor's 3,664-row event file, so the historical event total is 580,320.

## [0.7.2] - 2026-08-27

- Clear stale Bitcoin Core link-error telemetry after Core revalidates the
  already-complete cached row and its predecessor link, including zero-work
  recovery batches.

## [0.7.1] - 2026-08-27

- Recover a shallow Bitcoin Core fork after a long follow-mode outage by
  repairing only the bounded divergent suffix ending at the persisted cursor,
  then resuming ordinary paged catch-up to the live tip. Continue to fail
  closed when that cursor-centred lookback has no complete matching ancestor.

## [0.7.0] - 2026-08-25

- Import complete historical error-block child witnesses through a separate,
  Core-required publication aggregate. Preserve their source-row provenance
  and RSK sidecars outside ordinary authoritative snapshot deletion. Reclassify
  any previously stored stale or unknown parent to `error_block`, replay known
  archive coordinates idempotently, and reject changed evidence at the store
  boundary.

- Derive `time_below_mtp` error blocks from a required Bitcoin Core node by
  checking the exact eleven linked predecessor headers of an otherwise
  Core-absent, proof-of-work-valid parent. Preserve the Core-derived predecessor
  height source and rejection token in the read model, retain the pinned
  catalogue as a fallback and consistency check, and add
  `reclassify-parent` for a narrow, cascade-safe repair of existing evidence.

- Repair bounded near-tip Bitcoin reorgs in follow mode by capturing one
  tip-pinned Core view, atomically replacing the divergent canonical suffix,
  and retaining displaced blocks as stale evidence. Persist dependent
  reconciliation and dependent expansion as restart-safe queue phases so a
  process exit cannot lose a deeper cascade frontier. Serialize Core-backed
  classification and ordinary backbone writes against the suffix switch, drain
  pending suffix work before a Core-header-cache refresh reclassifies rows, and
  rebind existing same-height stale blocks to the replacement canonical
  competitor.
  Preserve unrelated producer failures and reconcile-pending status when
  recording or clearing repair telemetry, including during concurrent repairs,
  temporarily suspend and later restore unrelated producer error details while
  durable reconciliation is pending, consume structural conflicts covered by
  the committed replacement suffix, and continue to fail closed when the common
  ancestor lies outside the configured live window. Re-plan once when an
  in-flight classifier commits a same-height conflict after the initial repair
  scan. Replay durable parents with strict live Core classification so newly
  inferable stale children are not stranded, and retain Core-derived pool
  attribution when displaced AuxPoW-backed rows are replayed.

- Replace the compiled Bitcoin nBits epoch table with a sparse Postgres cache
  populated from a required Bitcoin mainnet Core node. Capture, import, and
  reconciliation refresh Core headers through the synced tip before classifying
  evidence, while live pollers and backbone follow refresh a stable Core
  snapshot on every tick. Historical imports retain one table through their
  derived rebuild. Shallow replacements and timestamp-overlapping retarget
  boundaries reclassify existing orphan rows while other expanded coverage
  revisits pending rows, with durable retry markers for an interrupted sweep.
  An advancing horizon verifies the old shallow horizon
  before retaining timestamp coverage. Retarget boundaries are marked final at
  100 blocks deep. Timestamp coverage does not regress when a valid later Core
  header has an older timestamp. The API remains Core-RPC-free by reading the
  persisted cache. A cache-driven recheck requires fresh Core evidence, so an
  RPC failure leaves its durable retry marker set. Cache headers and timestamp
  coverage are read from one database snapshot. The `--allow-unclassified`
  import bypass is removed. A strict BIP34 claim above Core's cached horizon
  remains pending even when its difficulty epoch is cached. The first Core-cache
  population conservatively revisits existing orphan classifications. Cache
  refresh waits for an in-flight classification transaction, so its completed
  sweep cannot miss a later commit made from an old cache snapshot. Cache
  readers acquire that shared lock before parent locks, and a non-mainnet Core
  tip holds rather than revoking a claimed mainnet height. A fresh Core tip
  rejects a claimed BIP34 height more than 144 blocks beyond it even if a stale
  cache already covers that height.

- Atomically repair existing strict/weak classifications with an
  `import-known-stales` membership update.

- Retry transient Core-header-cache refresh failures in `sync-bitcoin-core --follow`
  without masking sync progress or a typed cache/backbone integrity failure.

## [0.6.0] - 2026-08-15

- Turn findings figures into claim-led evidence panels with compact metrics,
  accessible summaries, multi-series charts, bars, discrete lollipops,
  annotations, highlighted periods, and event timelines. Show Foundry's RSK
  and Fractal exit across the full paired window with separate weekly series
  and an explicit zero tail, and give every other published finding a visual
  form suited to its evidence.

- Remove the `reclassify-pools` RSK skip, so each run scans the active corpus
  instead of usually paying for two fingerprint scans without short-circuiting.
  The skip required an unchanged active set, which continuous RSK capture
  normally invalidates every block. Migration `0009` drops the obsolete
  `rsk_reclassify_watermark` singleton.

## [0.5.0] - 2026-08-09

- Navigate catalogued error blocks as a first-class Go to target, backed by
  `/api/v1/navigator/error-block`. Ordering is Bitcoin height descending then
  stored hash bytes ascending, because the catalogue carries more than one
  block at some heights and paging on height alone would skip or repeat members
  of a group. Selecting one directly, by click or shared link, hydrates the
  target so stepping continues from it.
- Render an error block's consensus rejection as prose with per-rule help
  rather than the catalogue's raw token, fall back to the raw value without a
  help control for a rule the frontend has not mapped, and replace the absent
  competition panel with an explicit note that the block never raced.
- Count catalogued error blocks against the tree-window node budget, and only
  offer blocks the tree will render. The budget previously counted stale rows
  alone, so a window containing an error block could be advertised and then
  rejected by `/tree` as too large; sourceless catalogue rows are now excluded
  from both the budget and the navigator, matching what the tree shows.
- Reject height-axis navigator cursors whose bounds exceed a 32-bit height.
  Such a cursor previously wrapped to a negative height and returned an
  incorrect page as a success.

- Explain `child_block_time` where it is read. The block drawer's Child Time row
  gains a help topic covering what the stamp is, why AuxPoW's child-first
  commitment settles it before the Bitcoin work exists, and how far the two
  asymmetric reading rules actually reach, and shows each auxiliary block's
  offset from that block's Bitcoin header time (omitted when either stamp is
  unavailable or the difference is not exactly representable). Document the same
  model in `docs/data-model.md`, including what a stored child header does and
  does not prove.

- Classify headers in the pinned research error-block catalogue as
  `error_block`, preserving their primary consensus rejection token through the
  read model, API, tree controls, and source-health counts. Keep this
  full-proof-of-work but consensus-invalid state distinct from stale and BTC
  orphan evidence.

- Import the pinned 576,662-row research publication through one normalized
  27-chain contract, with complete checksum/schema/count preflight, Git LFS
  diagnostics, deterministic `import-all`, shared Bitcoin-parent
  classification caching, and no legacy artifact fallbacks.
- Preserve authenticated child height, hash, header, time, and `nBits` as
  independent nullable evidence. Use exact child-hash identity or partial
  height-plus-parent identity, derive exact identity from a header when needed,
  promote partial observations in place, and reject ambiguous or contradictory
  refinement. Keep live state reads and child-target verdict refinement safe
  for hashless historical rows. Fail the upgrade before schema changes if
  legacy rows conflict with the stronger exact identity.
- Reconcile manifest-backed historical and partial sources as authoritative
  snapshots while keeping live-source and operator CSV imports additive.
  Retire manifest-backed provenance from superseded publication commits and
  commit each base/provenance snapshot atomically, then drain parent and
  dependent read-model work through a durable resumable queue without retaining
  one advisory lock per imported parent.
  Retain source taxonomy and all published parent-coinbase evidence without
  collapsing distinct source rows, and treat Doichain's zero-row survey as a
  database no-op.
- Keep incomplete `--allow-unclassified` diagnostics additive, reject a zero
  import limit before mutation, derive the child-target verdict from published
  `nBits`, and route the targeted stale-branch pass through the durable parent
  and dependent reconciliation queue.
- Serialize historical source-health invalidation against rebuilds, refuse to
  mark source health ready while durable historical work remains, preserve a
  Hathor row promoted from hashless identity, and reject contradictory immutable
  RSK sidecar evidence before provenance can commit.
- Expose nullable child evidence through block detail and render unavailable
  fields explicitly instead of zero or placeholder values.
- Report historical write outcomes from the store's exact/partial identity
  decision instead of re-querying identities in the importer, and combine
  candidate parsing, validation, and preclassification into one stream. Bind
  classification and mutation reads to the already-verified artifact handle,
  preflight the required aggregate, and fail source health closed until the
  final multi-chain rebuild succeeds.
- Fill the Bitcoin RPC client's existing bounded concurrency during historical
  preclassification, and defer predecessor read-model queries until Bitcoin
  Core proves a candidate header is absent.
- Retry transient Bitcoin Core transport and warmup failures with bounded
  exponential backoff, fail parent preclassification explicitly after
  exhaustion, and validate direct-stale and stale-descendant publication
  statuses against their distinct canonical tokens.
- Accept a publication's cross-chain strict BTC-orphan promotion when the
  current chain independently proves the weaker orphan verdict, and let a
  direct Bitcoin Core stale attestation supersede an archived canonical source
  label while preserving that source provenance.

## [0.4.2] - 2026-07-30

- Bound the RSK miner-identity keyset scan on both sides of the
  evidence-to-event join. The cursor previously constrained only
  `merge_mining_event`, leaving the evidence-side index scan unbounded so
  every page re-walked `rsk_evidence_event_unique` from the start and the
  pass ran quadratic in corpus size (production: ~11.6M index rows per page
  and a ~63-hour projection, against 500 rows per page and under an hour
  once bounded).

## [0.4.1] - 2026-07-29

- Bound and center the findings canvas in a single minmax(0, 900px)
  column so wide screens keep readable card widths and a shared left edge
  across the feed and article states.

## [0.4.0] - 2026-07-29

- Add the findings content pipeline: one hand-authored JSON file per finding
  in `data/findings/`, validated by the feature-gated
  `mmm-capture::findings_registry` (content invariants, calendar dates,
  registered source codes, `[^N]` citation integrity) and compiled into
  `www/js/findings.generated.js` by `just gen-source-artifacts`, drift-gated
  in `cargo test`.
- Add the Findings view (`?view=findings`): the corpus as a month-grouped
  feed with category, status, and shared Source filtering. Opening a card
  replaces the feed with a cited article (`finding=<slug>`, serialized only
  while findings is active), with theme-aware line-series evidence figures
  and typed anchors that jump to the header tree or open source details.
  The drawer column collapses on this view; its state survives for return.
- Seed six findings: the September 2025 Foundry stale cluster (20
  full-difficulty blocks at 16 heights, ~46.9 BTC foregone, unseen by any
  observer; new `pool-incident` category), Hathor's ~3,500x hashrate
  collapse (2026-06-10), Foundry's single-block entry (2025-04-22) and exit
  (2026-06-24) from RSK and Fractal, the Elastos exploit halt (2026-07-20),
  and Terracoin's full-difficulty win (2026-05-20).

## [0.3.0] - 2026-07-28

- Add the Header Time Delta view: a distribution of how far apart each stale
  block and its canonical competitor timestamped their headers, with a focus
  window, off-scale gutters, a symmetric-log full-range strip, Coverage and
  Table tabs, and block-detail cross-links. Backed by the new read-only
  `GET /api/v1/competitions` endpoint.
- Exclude known stales from strict/weak BTC-orphan classification. Migration
  0006 adds the operator-imported `known_stale_block` membership, loaded by
  `import-known-stales` (atomic, strict by default) from the upstream
  `bitcoin-data/stale-blocks` dataset; the classifier excludes members
  outright, `reclassify-known-stales` retroactively demotes contaminated
  rows, `import-dataset` refuses an empty membership, and every producer
  entry path warns when it is empty.
- Align vocabulary and ingest with the published merge-mining-research
  history: the importer prefers the committed monitor-evidence exports, the
  broad evidence state is spelled `unknown` (legacy `orphan` still read on
  ingest), the excluded verdict token is renamed from `btc_stale_excluded` to
  `excluded` across DB, API, and frontend (migration 0005), and the
  historical-source manifest is re-pinned to the published research commit.
- Extend `import-dataset` to the six live chains, requiring exact child
  identity and constructing the RSK evidence sidecar during import; the
  non-live exact-child-field chains (VCash, Lyncoin, SixEleven) resolve to
  the research repo's committed canonical-blocks artifacts.
- Derive the historical importer's chain table from the shared source
  registry, add a cross-repo BIP34 drift guard against the research checkout
  (`doichain` joins the strict set), and document the ParentKind/BlockKind
  enum boundary.
- Add Open Graph and Twitter social cards.
- Consolidate the code landed since 0.2.0 into shared API, read-model, and
  frontend helpers: net 250 fewer lines with no behavior change.

## [0.2.1] - 2026-07-13

- Correct source-rail bylines and source-modal documentation for Bitcoin Stash,
  BLAST, Doichain, Fusioncoin, Jax.Network, Jincoin, Lyncoin, SixEleven, and
  VCash.

## [0.2.0] - 2026-07-11

- Recover every Lyncoin Bitcoin-merge-mined header through height 260,499 and
  all 999,407 available SixEleven blocks. Bitcoin Core classified 11 Lyncoin
  parents and 7 SixEleven parents as canonical; neither chain produced a stale
  winner.
- Keep the recovery limits visible: VCash contributes 68 explorer mappings
  confirmed as canonical and completed with block evidence by Bitcoin Core
  (not the VCash blockchain), while Doichain is a completed zero-row survey
  after 429,401 AuxPoW commitments produced no Bitcoin block winner.
- Make source IDs permanent and retire ID 32. Mazacoin is removed because its
  consensus source contains no AuxPoW implementation, so it is not a Bitcoin
  merge-mined source.

## [0.1.0] - 2026-07-02

- Initial public release.
