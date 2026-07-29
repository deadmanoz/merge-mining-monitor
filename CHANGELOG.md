# Changelog

All notable public changes to this project will be documented in this file.

This changelog starts with the initial release.

## [Unreleased]

- Add the findings content pipeline: hand-authored, evidence-backed findings
  live as one JSON file per finding in `data/findings/`, validated by the new
  feature-gated `mmm-capture::findings_registry` (content invariants, real
  calendar dates, registered source codes, and full `[^N]` citation
  integrity) and compiled deterministically into
  `www/js/findings.generated.js` by `just gen-source-artifacts`, with a
  drift gate in `cargo test`. Seeds the corpus with five findings: the
  2026-06-10 Hathor merge-mining hashrate collapse, Foundry USA's
  single-block exit from RSK+Fractal merge-mining (2026-06-24) and its
  matching single-block entry (2025-04-22), the 2026-07-20 Elastos
  exploit halt, and Terracoin's 2026-05-20 full-difficulty win. The
  frontend Findings view that renders this corpus lands separately.

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
