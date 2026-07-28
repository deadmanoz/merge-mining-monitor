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

- Cross-link the block detail and the Header Time Delta view through the shared
  selection: a stale block's Header Time Delta opens the distribution focused on
  that competition, and the distribution names the current selection and can
  show it in the tree, retargeting the tree window on its height. A selection
  the filters hide can be revealed by clearing only the filters that hide it.

- Fix the Header Time Delta view's outlier list growing to its full content
  height and being clipped by its panel instead of scrolling, which made most
  rows unreachable whenever the window excluded more than a screenful.

- Add the read-only `GET /api/v1/competitions` endpoint, serving every
  derivable stale-vs-canonical competition with its header-time delta, both
  miner pools, and its active evidence sources. It backs a forthcoming
  header-time-delta distribution view, which needs the whole set client-side to
  re-bin and re-window without a request per interaction.

- Follow the research repo's published public history: the committed
  historical-source manifest is re-pinned from research commit `39cb7af` to
  `e1e72d4` (the published tip), picking up the nested `data/validated-stales/`
  layout and the release-prep data refresh (total declared stale rows 1482 to
  1461; every imported row is still re-proven against Bitcoin Core on ingest).
- Consult a first-class known-stale membership before assigning
  `block.btc_orphan_class`, so a catalogued stale is never mislabelled a
  strict/weak BTC orphan. A known stale is absent from Bitcoin Core's active
  chain by definition, so it passes the reconciler's PoW + BIP34 + nBits +
  Core-absence checks and, without a membership check, the offline classifier
  refined it into `strict_btc_orphan` / `weak_btc_orphan` (production served
  header `000000000000000013fe26675faa8f7dccd55ce5485bb6d0373fa66345901436`,
  height 363736, catalogued in the upstream `bitcoin-data/stale-blocks` dataset
  for months, as a strict orphan). New migration
  `0006_add_known_stale_block.sql` adds a minimal `known_stale_block` table
  (hash PK in internal byte order, advisory height, source label, imported_at),
  loaded by a new `import-known-stales` subcommand from the upstream
  stale-blocks.csv-shaped dataset with a recorded provenance label (exposed
  as `just import-known-stales`; the importer fails rather than record an
  empty membership from a wrong or headerless file). The membership consulted
  at classification time is the operator-imported table alone: a proven-stale
  `block` row is deliberately not unioned in, since for a given hash it could
  only match the row under classification itself, and a stale-to-unknown
  re-derivation would then consult the very state it is replacing.
  `mmm_read_model`'s
  `compute_block_orphan_class` checks it before the strict/weak resolution and
  returns `excluded`, mirroring the research classifier's
  `known_stale_hash -> excluded` verdict. Ingest-time re-classification stays
  (defense in depth), no DB values or columns are renamed, and the legacy
  orphan-spelling acceptance is untouched. A new `reclassify-known-stales`
  subcommand retroactively demotes any strict/weak `unknown` block already in
  the membership to `excluded`, idempotently and counting demotions loudly,
  maintaining `source_health` through the reconciler's before/after snapshot
  diff. Following the research repo's lesson, `import-dataset` refuses to run
  against an empty `known_stale_block` (pass `--allow-empty-known-stales` to
  opt out), and `reclassify-unknown-parents` / `reclassify-known-stales` warn
  prominently when the membership is empty rather than silently proceeding as
  if it were consulted. Live pollers, bounded backfills, and the Hathor
  cache import carry the same warning at startup, the
  import summary buckets unknown rows by the persisted `btc_orphan_class`
  (an excluded row reports as `excluded`, never strict/weak, whichever
  exclusion path fired), and `--source-label` rejects blank values so every
  membership row stays auditable. The membership import itself is atomic
  (one transaction holding the per-parent advisory locks in the global sorted
  order) and strict by default (any malformed row is fatal unless
  `--skip-malformed` is passed), so a corrupt file or mid-import failure
  records nothing rather than a partial membership that downstream
  empty-membership guards would treat as complete.
- Derive the historical importer's chain table from `mmm-capture`'s
  `SOURCE_REGISTRY` instead of hand-listing it a second time.
  `historical_ingest::config::HISTORICAL_CHAINS` now filters the registry's
  `Historical`/`Partial` lifecycle rows and looks up only the one field the
  registry does not carry, the CSV `height_column`, from a small local side
  table (missing entries fail loudly at the first lookup). The derived table
  was verified row for row against the deleted hand list before it landed;
  a hygiene test guards the side table against orphaned or duplicate entries.
- Add a cross-repo drift-sync test for the two hand-maintained BIP34
  constants. `mmm-capture`'s `STRICT_BIP34_CHAINS` and `BIP34_HEIGHT` are
  permanent ports of the merge-mining-research repo's
  `BTC_COINBASE_SCRIPTSIG_CHAINS` and `BIP34_HEIGHT`; the new test locates the
  research checkout (via `MERGE_MINING_RESEARCH_DIR`, falling back to the
  sibling `../merge-mining-research` path), parses both Python sources, and
  asserts equality. It skips cleanly when no checkout is available and fails
  loudly if a checkout is present but either constant cannot be located.
  `doichain` joins `STRICT_BIP34_CHAINS` in the same pass, matching the
  published classifier's `BTC_COINBASE_SCRIPTSIG_CHAINS` allowlist.
- Document why `ParentKind` (`mmm-capture::capture`) and `BlockKind`
  (`mmm-bitcoin-core::parent_classifier`) stay separate enums instead of
  merging into one: they model two different DB CHECK domains
  (`merge_mining_event.btc_parent_kind` has four values including `near`;
  `block.kind` has three, since a `block` row never persists a child-only
  near-miss header). Each enum's doc comment now cross-references the other
  and the `mmm-read-model::classify` translation boundary between them.
- Align the historical importer with the research repo's data-consistency
  pass: the default CSV search now prefers the committed
  `results/monitor-evidence/<chain>_monitor_evidence.csv` exports, whose
  per-row `btc_stale_relevance` / `relevance_reason` columns supply
  strict/weak verdicts without the bulky relevance inventory; the
  `classification` column accepts `unknown` (the research repo's new name
  for the broad evidence state, with the legacy `orphan` spelling still
  read); and the relevance-inventory reason matching uses the vocabulary
  the research classifier actually emits (`valid_direct_stale` /
  `valid_stale_descendant`) instead of the never-emitted placeholder
  strings. The explicit VCash/Lyncoin/SixEleven artifact checksums track
  the regenerated research artifacts.
- Track two further lockstep vocabulary changes landing on the research repo
  at the same time: stale and stale-descendant rows in the monitor-evidence
  exports now carry an empty `btc_stale_relevance` (the `relevance_reason`
  column alone signals `valid_direct_stale` / `valid_stale_descendant`,
  which the importer already keyed on, so old and new exports both ingest
  without error), and the excluded verdict token is renamed from
  `btc_stale_excluded` to `excluded` in both repos. The rename covers the
  `block.btc_orphan_class` DB value, the `classification=` query parameter
  and JSON response field, and the frontend's classification vocabulary and
  fill styling. Migration `0005_rename_excluded_orphan_class.sql` rewrites
  existing rows and replaces the CHECK constraint (the baseline schema in
  `0001_canonical_schema.sql` is left alone, since a migration has already
  applied to a persistent database per `migrations/README.md`).

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
