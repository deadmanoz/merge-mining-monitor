# Historical Ingest

Historical ingest imports recovered dead-chain AuxPoW evidence into the same
producer/read-model path as live capture. Sources use `auxpow:<chain>` codes and
the same derived-state rules as live events.

## Supported Chains

`argentum`, `bitcoin-vault`, `bitmark`, `coiledcoin`, `crown`, `devcoin`,
`elcash`, `emercoin`, `geistgeld`, `groupcoin`, `huntercoin`, `i0coin`,
`ixcoin`, `myriadcoin`, `terracoin`, `unobtanium`, and `xaya`.

## Provenance

`data/historical/historical-source-manifest.json` records the compact validated-stale input set:
source commit, per-chain CSV path, child-height column, row count, and SHA-256.
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
