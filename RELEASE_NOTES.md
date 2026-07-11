# Release Notes

## [0.2.0] - 2026-07-11

- Recover every Lyncoin Bitcoin-merge-mined header through height 260,499 and
  all 999,407 available SixEleven blocks. Bitcoin Core classified 11 Lyncoin
  parents and 7 SixEleven parents as canonical; neither chain produced a stale
  winner.
- Keep the recovery limits visible: VCash contributes 68 canonical mappings
  from archived explorer pages (not the VCash blockchain), while Doichain is a
  completed zero-row survey after 429,401 AuxPoW commitments produced no
  Bitcoin block winner.
- Make source IDs permanent and retire ID 32. Mazacoin is removed because its
  consensus source contains no AuxPoW implementation, so it is not a Bitcoin
  merge-mined source.

## [0.1.0] - 2026-07-02

- Release the first source distribution of `merge-mining-monitor`.
- Include the Rust workspace, Postgres schema baseline, capture/reconciliation
  pipeline, read API, static frontend, fixtures, provenance manifests, and local
  operator tooling needed to build, test, and run the monitor.
