# Changelog

All notable public changes to this project will be documented in this file.

This changelog starts with the initial release.

## [Unreleased]

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
