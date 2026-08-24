# Changelog

All notable public changes to this project will be documented in this file.

This changelog starts with the initial release.

## [Unreleased]

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
  persisted cache.
  Remove the `--allow-unclassified` import bypass.

- Repair existing strict/weak classifications immediately after an
  `import-known-stales` membership update.

- Retry transient Core-header-cache refresh failures in `sync-bitcoin-core --follow`
  without masking a typed backbone integrity failure.

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
