# Historical Ingest

Historical ingest sends recovered full-chain and partial evidence through the
same producer and read-model path as live capture. Source codes retain the
`auxpow:<chain>` form for compatibility, including VCash's tag-based evidence;
the stored events otherwise follow the same derived-state rules as live events.

## Supported Chains

`argentum`, `bitcoin-vault`, `bitmark`, `coiledcoin`, `crown`, `devcoin`,
`elcash`, `emercoin`, `geistgeld`, `groupcoin`, `huntercoin`, `i0coin`,
`ixcoin`, `lyncoin`, `myriadcoin`, `sixeleven`, `terracoin`, `unobtanium`,
`vcash`, and `xaya`, plus the six live-lifecycle chains that also publish
recovered historical monitor-evidence exports: `namecoin`, `rsk`, `elastos`,
`syscoin`, `hathor`, and `fractal` (the closed `LIVE_IMPORT_CHAINS`
allowlist).

Live-import chains capture into the same `source_id` as their live pollers,
so their rows must carry the exact node-verified `child_block_hash` /
`child_block_time` the research exports publish: that identity is what lets
an imported row collide with a live-captured one under the
`(source_id, child_height, child_block_hash)` upsert (which never rewrites
the first writer's parent evidence) instead of duplicating it. A blank child
cell skips as `empty_field`; the synthetic-hash fallback is reserved for the
legacy historical chains that recovered no child identity. RSK rows
additionally construct the 1:1 `rsk_merge_mining_evidence` sidecar from the
export's seven sidecar columns (miner, merge-mining hash, uncle placement,
merkle proof, coinbase tail) in the same transaction as the event -- the API's
block-detail projection refuses an `auxpow:rsk` event without one. The
sidecar's `pool_identity_id` is left NULL at import; run
`just reclassify-pools` afterwards to late-fill it from the RSK miner
registry.

VCash is a 68-row canonical subset recovered from archived explorer pages, not
the VCash blockchain. The default CSV search resolves it to the research
repo's committed `data/canonical/vcash_canonical_blocks.csv` (see Known-Stale
Membership and Import below); it is not part of the compact validated-stale
manifest.

Lyncoin is complete for its Bitcoin-merge-mined era at child heights 0 through
260,499 (the Flex fork begins at 260,500). Its 11-row import artifact is the
canonical subset of 56,653 Bitcoin-difficulty candidates, with exact child
height, hash, and time.
The default CSV search resolves it to the committed
`data/canonical/lyncoin_canonical_blocks.csv`; like VCash, it is not added to
the generated validated-stale manifest.

SixEleven is complete through its available tip: 999,407 child blocks from
genesis through height 999,406. Its seven-row import artifact is the canonical
subset of 80,364 Bitcoin-difficulty candidates, with exact child height, hash,
and time.
The default CSV search resolves it to the committed
`data/canonical/sixeleven_canonical_blocks.csv`; it also remains outside the
generated validated-stale manifest.

## Provenance

`data/historical/historical-source-manifest.json` records the compact
validated-stale input set for its manifest-backed chains: source commit,
per-chain CSV path, child-height column, row count, and SHA-256. The explicit
VCash, Lyncoin, and SixEleven recovery artifacts remain outside that generated
stale-only manifest.
The raw CSVs, full-evidence inventories, and dataset production artefacts are not
committed to this repo. They will be made available in the public
[`merge-mining-research`](https://github.com/deadmanoz/merge-mining-research)
repository; this repo keeps the manifest and checksums needed to verify the
supplied inputs.

The importer prefers richer local inputs when present:

1. for the explicit-recovery chains (VCash, Lyncoin, SixEleven), the research
   repo's committed `data/canonical/<chain>_canonical_blocks.csv` artifacts,
   whose monitor-evidence exports predate the `child_block_time` column those
   chains require; for every other chain -- including the live-import chains,
   whose exports carry hydrated `child_block_hash` / `child_block_time` and
   RSK's sidecar columns -- its committed monitor-evidence export
   (`results/monitor-evidence/<chain>_monitor_evidence.csv`, which carries
   per-row `btc_stale_relevance` / `relevance_reason` verdicts, so no
   separate relevance inventory is needed).
2. generated full-evidence CSVs
3. local classified archive CSVs
4. compact stale-block CSVs
5. the manifest path
6. compact validated-stale CSVs

Use `MERGE_MINING_RESEARCH_DIR`, `MERGE_MINING_ARCHIVE_DIR`, `--csv`,
`--manifest`, or `--relevance` to control input paths. Because the raw datasets
are not distributed with this repository, running an import requires supplying
recovered CSVs at one of these paths; the manifest lets you verify a supplied
file matches the recorded provenance checksum. There is no implicit
home-directory fallback; set local roots in `.env`.

## Known-Stale Membership

`import-dataset` refuses to run while the `known_stale_block` table is empty
(pass `--allow-empty-known-stales` to opt out): without the membership, a
catalogued stale that Bitcoin Core attests absent would be mislabelled a
strict/weak BTC orphan. Load the membership once per database, after
migrations, from an upstream `stale-blocks.csv`-shaped dataset (the
`bitcoin-data/stale-blocks` repository's file, with a `hash` column of display
block hashes and an optional `height`):

```bash
just import-known-stales --csv path/to/stale-blocks.csv --source-label "bitcoin-data/stale-blocks@<commit>"
```

The `--source-label` records the dataset's provenance on every imported row.
The import is idempotent and atomic (all rows commit in one transaction, or
nothing does), and it fails rather than record a partial or empty membership:
a missing `hash` column, zero usable rows, or ANY malformed row aborts the
run, since downstream guards only test membership emptiness and a corrupt
dataset must not count as initialized. Pass `--skip-malformed` to import the
valid subset of a file with known-bad rows. The summary prints
inserted/already-present/skipped counts. On a database that already holds
classified rows, follow up with:

```bash
just reclassify-known-stales
```

which retroactively demotes any strict/weak `unknown` block already in the
membership to `excluded`, idempotently, maintaining `source_health` through
the reconciler. The full fresh-database ordering is: migrations, then
`import-known-stales`, then `reclassify-known-stales` (a no-op when nothing
was classified yet), then `import-dataset`.

## Import

Prepare the DB and Bitcoin Core classifier:

```bash
just db-up
just db-migrate-dev
set -a; source .env; set +a
```

Import one chain:

```bash
just import-dataset devcoin
```

Import the explicit recovered artifacts. These exact-child-field chains need
the authoritative child hash and time that the compact monitor-evidence
exports omit, so the default CSV search resolves them to the research repo's
committed `data/canonical/<chain>_canonical_blocks.csv` artifacts instead and
no `--csv` override is needed:

```bash
just import-dataset lyncoin
just import-dataset sixeleven
just import-dataset vcash
```

Import a live-import chain. The default CSV search resolves it to the research
repo's committed monitor-evidence export, which carries the node-verified
child identity these chains require; RSK also writes its
`rsk_merge_mining_evidence` sidecar per event, then late-fills pool identity:

```bash
just import-dataset namecoin
just import-dataset rsk
just reclassify-pools --only rsk
```

Verify explicit inputs before import:

- VCash's 68-row artifact has SHA-256
  `4ad387246b5730c05af9216df5c82e80d64fec6cbe2e8db40487dfac3514f801`.
- Lyncoin's 11-row artifact has SHA-256
  `12027329ef7c19c7a8654e348138e4e462b027aa39f6744cdedd4c4e58181ae9`.
- SixEleven's seven-row artifact has SHA-256
  `086552bd812cc6c52e334970ea4b0f466041f8b4040c0e9b7360a7ec758c32dd`.
  (Hashes track the published research history, whose data-consistency pass
  normalized these artifacts to the shared vocabulary and schema.)

The command requires Bitcoin Core classification by default. Use
`--allow-unclassified` only for local dry-run checks; production imports should
prove parent state through Core.

After bulk imports:

```bash
just rebuild-source-health
just reclassify-pools
```

Run historical chains twice on a fresh database when importing branch
attestations: the second pass can classify stale-descendant rows whose
predecessor branch block was imported during the first pass.
