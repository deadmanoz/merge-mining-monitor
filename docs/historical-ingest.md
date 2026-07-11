# Historical Ingest

Historical ingest sends recovered full-chain and partial evidence through the
same producer and read-model path as live capture. Source codes retain the
`auxpow:<chain>` form for compatibility, including VCash's tag-based evidence;
the stored events otherwise follow the same derived-state rules as live events.

## Supported Chains

`argentum`, `bitcoin-vault`, `bitmark`, `coiledcoin`, `crown`, `devcoin`,
`elcash`, `emercoin`, `geistgeld`, `groupcoin`, `huntercoin`, `i0coin`,
`ixcoin`, `lyncoin`, `myriadcoin`, `sixeleven`, `terracoin`, `unobtanium`,
`vcash`, and `xaya`.

VCash is a 68-row canonical subset recovered from archived explorer pages, not
the VCash blockchain. Supply its 68-row CSV with `--csv`; it is not part of
the compact validated-stale manifest.

Lyncoin is complete for its Bitcoin-merge-mined era at child heights 0 through
260,499 (the Flex fork begins at 260,500). Its 11-row import artifact is the
canonical subset of 56,653 Bitcoin-difficulty candidates, with exact child
height, hash, and time.
Supply it explicitly with `--csv`; like VCash, it is not added to the generated
validated-stale manifest.

SixEleven is complete through its available tip: 999,407 child blocks from
genesis through height 999,406. Its seven-row import artifact is the canonical
subset of 80,364 Bitcoin-difficulty candidates, with exact child height, hash,
and time.
Supply it explicitly with `--csv`; it also remains outside the generated
validated-stale manifest.

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

1. generated full-evidence CSVs
2. local classified archive CSVs
3. compact stale-block CSVs
4. the manifest path
5. compact validated-stale CSVs

Use `MERGE_MINING_RESEARCH_DIR`, `MERGE_MINING_ARCHIVE_DIR`, `--csv`,
`--manifest`, or `--relevance` to control input paths. Because the raw datasets
are not distributed with this repository, running an import requires supplying
recovered CSVs at one of these paths; the manifest lets you verify a supplied
file matches the recorded provenance checksum. There is no implicit
home-directory fallback; set local roots in `.env`.

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

Import the explicit recovered artifacts:

```bash
just import-dataset lyncoin --csv "$MERGE_MINING_RESEARCH_DIR/results/recovery/lyncoin_evidence.csv"
just import-dataset sixeleven --csv "$MERGE_MINING_RESEARCH_DIR/results/recovery/sixeleven_evidence.csv"
just import-dataset vcash --csv "$MERGE_MINING_RESEARCH_DIR/results/recovery/vcash_canonical_partial.csv"
```

Verify explicit inputs before import:

- VCash's 68-row artifact has SHA-256
  `37f5739a899d6d9856008e8dadcb512e6dcbef5f8eef38a433c14933271c1956`.
- Lyncoin's 11-row artifact has SHA-256
  `896c9ca07288406cc99c80f770acd5135e8b95a842091cdfa514c088b9b856d1`.
- SixEleven's seven-row artifact has SHA-256
  `5ad62cab88e4ae62f1cce84b12acd7b68832b9e428ea9d386f1c05109e9871e1`.

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
