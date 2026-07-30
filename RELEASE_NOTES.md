# Release Notes

## [0.4.2] - 2026-07-30

- Make the RSK pool-reclassification pass complete in minutes instead of days.

## [0.4.1] - 2026-07-29

- Bound and center the Findings feed and article on wide screens.

## [0.4.0] - 2026-07-29

- Add the Findings view: evidence-backed stories the monitor's own data
  surfaces, presented as a month-grouped feed of cited articles with
  data-drawn evidence figures and anchors that jump to the header tree or
  open source details.
- Seed six findings: the September 2025 Foundry stale cluster that no
  observer recorded (20 full-difficulty blocks, roughly 46.9 BTC foregone),
  Hathor's June 2026 merge-mining hashrate collapse, Foundry USA's
  single-block entry and exit from RSK and Fractal merge mining, the
  Elastos July 2026 exploit halt, and Terracoin's May 2026 full-difficulty
  win.

## [0.3.0] - 2026-07-28

- Add the Header Time Delta view: how far apart each stale block and the
  canonical block that beat it timestamped their headers, with a focus
  window, outlier gutters, coverage and table tabs, and block-detail links.
- Never label a catalogued known stale as a strict or weak BTC orphan: the
  monitor imports the `bitcoin-data/stale-blocks` membership, excludes its
  members from orphan classification, and retroactively corrects rows
  classified before the membership existed.
- Match the published research vocabulary: the broad evidence state is
  `unknown`, and the excluded orphan-class token is now `excluded`.
- Accept recovered historical evidence for the six live chains, with exact
  child identity required and RSK sidecar support.
- Add Open Graph social cards for link sharing.

## [0.2.1] - 2026-07-13

- Correct the source rail and source profiles for nine catalogued or recovered
  chains, adding historical date ranges, concise key facts, and citations in
  the source dialog.

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

- Release the first source distribution of `merge-mining-monitor`.
- Include the Rust workspace, Postgres schema baseline, capture/reconciliation
  pipeline, read API, static frontend, fixtures, provenance manifests, and local
  operator tooling needed to build, test, and run the monitor.
