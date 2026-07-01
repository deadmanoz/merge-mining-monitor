# Data Artifacts

This directory holds committed release-domain data. Test fixtures, migrations,
frontend assets, and generated frontend modules stay in their existing
directories.

## Layout

- `pools/current.json` - generated Bitcoin parent pool snapshot. Regenerate with
  `just gen-pool-snapshot`.
- `pools/slug-map.json` - curated upstream filename to stable pool slug map used
  by the pool snapshot generator.
- `pools/child-identities/` - curated child-chain identity registries used for
  pool attribution. See `pools/README.md`.
- `consensus/btc_nbits_by_epoch.json` - generated Bitcoin nBits epoch table.
  Regenerate with `scripts/gen-nbits-table.py` when extending offline coverage.
- `sources/chain_profiles.json` - hand-authored source profile data used by
  `just gen-source-artifacts`.
- `historical/historical-source-manifest.json` - generated provenance manifest
  for historical stale-block CSV inputs. Regenerate with
  `just gen-historical-source-manifest`.
- `historical/historical-source-manifest.sha256` - checksum of the committed
  historical manifest.

The `csv_path` values inside `historical/historical-source-manifest.json` are
relative to the external [merge-mining-research](https://github.com/deadmanoz/merge-mining-research)
source repository, not this repository's `data/` tree.

## Checks

Run `just check-data-artifacts` for local deterministic checks over this
directory and the generated source-registry artifacts.
