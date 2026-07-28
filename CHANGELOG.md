# Changelog

All notable public changes to this project will be documented in this file.

This changelog starts with the initial release.

## [Unreleased]

- Add a Header Time Delta view, reachable from a new top-level view switcher,
  plotting how far apart a stale block and the canonical block that beat it
  timestamped their headers. One focus window drives a linear histogram of the
  core, hatched off-scale gutters for what it excludes, and a symmetric-log
  strip of the whole range; a Coverage tab answers "what share is within
  plus or minus T" and a Table tab is the accessible twin. Selecting an outlier
  opens the usual block detail, and the Source filter is shared with the tree.

- Add the read-only `GET /api/v1/competitions` endpoint, serving every
  derivable stale-vs-canonical competition with its header-time delta, both
  miner pools, and its active evidence sources. It backs a forthcoming
  header-time-delta distribution view, which needs the whole set client-side to
  re-bin and re-window without a request per interaction.

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
