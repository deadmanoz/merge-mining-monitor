# Release Notes

## [0.11.0] - 2026-10-21

- Show the Core DAO and exSat declarations in each Bitcoin coinbase on the block page: the declared validator, reward address or account, whether Core DAO reads it, and the pool associated with the declared account, which is not necessarily the block's miner.
- Count each block's declarations in a second badge on the header tree, and list them below the merge-mined chains when a block is selected.

## [0.10.0] - 2026-10-20

- Leave near parents (merge-mined headers that missed Bitcoin's target) out of each source's event count.
- Reclassify the Namecoin-witnessed AntPool blocks 494,933 and 544,568 as error blocks: their coinbases claim more than the block reward.

## [0.9.4] - 2026-10-18

- Identify Qbit payouts to F2Pool, SecPool and Luxor, shown separately from the miner tags in Qbit coinbases.
- Correct the child-chain identities recorded for error block 957,780.

## [0.9.3] - 2026-10-18

- Show Qbit miner attribution from authenticated child coinbase tags, independently of the Bitcoin parent miner.

## [0.9.2] - 2026-10-07

- Use the expanded Huntercoin and Xaya evidence recovered from Daniel Kraft's native blockchain dumps, including authenticated native Xaya block heights.
- Update the error catalogue to 53 Bitcoin parents and 121 child-chain observations, and explain errors inherited from an invalid predecessor.
- Keep the About dialog's release-note rows readable when the list scrolls.

## [0.9.1] - 2026-09-29

- Refuse merge-mined parents from other SHA-256 chains (mostly Bitcoin Cash) by checking each against Bitcoin's difficulty history, and remove over 52,000 non-Bitcoin unknown blocks stored by earlier captures.
- Remove blocks once no evidence attests them, and apply the same difficulty-history check when classifying orphans.

## [0.9.0] - 2026-09-29

- Track Terracoin as a live source with continuous AuxPoW block authentication and per-height capture-error tracking.
- Show Terracoin's chain ID, slot, and proof in block details, and surface the merge-mining commitment on Bitcoin blocks seen only through Terracoin.

## [0.8.2] - 2026-09-22

- Surface ten reviewed body-invalid Bitcoin parents as error blocks with their rejection reasons instead of stales, expanding the catalogue to 49 invalid parents and 107 child-chain observations.

## [0.8.1] - 2026-09-21

- Keep live capture responsive: recent child-chain heights rescan faster, and unknown-parent rechecks run in the background instead of stalling capture.

## [0.8.0] - 2026-09-16

- Track Qbit as a live source.
- Keep Bitcoin-side evidence when a child-chain reorganisation replaces a block, and show the displacement on the block's events instead of removing them.
- Recover complete Bitcoin parent headers when backfilling older RSK blocks, checking each uncle on its own.

## [0.7.13] - 2026-09-09

- Add SpaceXpanse ROD's authenticated evidence, and check Xaya blocks against their external targets.
- Mark i0coin's current chain status as Unknown: its snapshot dates bound the evidence, not the network's availability.

## [0.7.12] - 2026-09-03

- Annotate the two F2Pool `bad-blk-sigops` stale blocks (heights 783,426 and 784,121) as body-invalid with rule help and an evidence link.

## [0.7.11] - 2026-09-02

- Speed up historical imports; no visible changes.

## [0.7.10] - 2026-09-02

- Keep blocks' original observation times when historical data is refreshed.

## [0.7.9] - 2026-09-02

- Refresh historical parents' coinbase output text when the recovered data's format changes; no other visible changes.

## [0.7.8] - 2026-09-02

- Speed up historical imports by reusing proven classifications; no visible changes.

## [0.7.7] - 2026-09-01

- Load the recovered historical data: 1,037,005 ordinary events, 21 stale-descendant summaries, and 86 authenticated error observations covering 39 consensus-invalid Bitcoin parents.

## [0.7.6] - 2026-08-29

- Import error observations recorded under an older name for the median-time-past rule.

## [0.7.5] - 2026-08-29

- Speed up historical imports by skipping data already loaded; no visible changes.

## [0.7.4] - 2026-08-28

- Speed up full historical imports by skipping unchanged data; no visible changes.

## [0.7.3] - 2026-08-28

- Show the active Go-to item position (`n of N`) while stepping through stales, stale branches, error blocks, orphans, and orphan branches.
- Reload the active view from the Updated stamp while keeping automatic 60s refresh.
- Expand the error-block catalogue to 35 parents and 78 error observations, and the historical data to 580,320 events.

## [0.7.2] - 2026-08-27

- Clear Bitcoin Core link errors once the link recovers.

## [0.7.1] - 2026-08-27

- Recover from shallow Bitcoin forks after long Bitcoin Core outages.

## [0.7.0] - 2026-08-25

- Check difficulty and timestamp rules against Bitcoin Core's own headers instead of a built-in table.
- Handle Bitcoin reorganisations near the tip, keeping displaced blocks as stale evidence.
- Import 73 recovered child-chain witnesses for 33 catalogued consensus-invalid Bitcoin error blocks and surface them in the error-block navigator.

## [0.6.0] - 2026-08-15

- Present each finding with evidence figures tailored to its claim, including paired 14-month weekly RSK and Fractal series for Foundry USA's entry and exit.

## [0.5.0] - 2026-08-09

- Step through catalogued error blocks from the Go to menu and explain each broken Bitcoin consensus rule in plain language.

## [0.4.2] - 2026-07-30

- Speed up RSK pool attribution; no visible changes.

## [0.4.1] - 2026-07-29

- Bound and center the Findings feed and article on wide screens.

## [0.4.0] - 2026-07-29

- Add the Findings view: a month-grouped feed of cited articles with evidence figures and links to the header tree and source details.
- Seed six findings covering the September 2025 Foundry stale cluster, Hathor's hashrate collapse, Foundry USA's RSK and Fractal entry and exit, the Elastos exploit halt, and Terracoin's full-difficulty win.

## [0.3.0] - 2026-07-28

- Add the Header Time Delta view comparing header timestamps between stale blocks and their canonical competitors across histogram, coverage, and table tabs.
- Stop classifying blocks listed in the bitcoin-data/stale-blocks catalogue as orphans, and mark them excluded instead.
- Add link previews when the site is shared.

## [0.2.1] - 2026-07-13

- Correct source-rail bylines and source profiles for nine catalogued or recovered chains with historical date ranges, key facts, and citations.

## [0.2.0] - 2026-07-11

- Add complete Lyncoin (11 canonical parents) and SixEleven (7 canonical parents) recoveries, VCash's 68 canonical explorer mappings, and Doichain's completed zero-row survey.
- Remove Mazacoin after confirming its source contains no AuxPoW implementation.

## [0.1.0] - 2026-07-02

- First public release of the monitor, with live capture, the read API and the web interface.
