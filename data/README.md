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
- `consensus/error_blocks.csv` - compact mirror of the pinned
  `merge-mining-research` error-block catalogue. It classifies a captured,
  full-proof-of-work invalid header as `error_block`, never as stale or an
  orphan. The catalogue header pin must match
  `historical/historical-source-manifest.json` `source_repo_commit`; the
  generator refuses a `--source-commit` that disagrees. Update the publication
  pin first, then `just gen-error-blocks-catalogue --source-commit
  <research-sha>`.
- `sources/chain_profiles.json` - hand-authored source profile data used by
  `just gen-source-artifacts`.
- `findings/` - hand-authored evidence-backed findings, one JSON file per
  finding (filename matches the slug). Compiled into
  `www/js/findings.generated.js` by `just gen-source-artifacts`.
- `historical/historical-source-manifest.json` - generated provenance manifest
  for historical stale-block CSV inputs. Regenerate with
  `just gen-historical-source-manifest`.
- `historical/historical-source-manifest.sha256` - checksum of the committed
  historical manifest.
- `historical/imported-artifact-seed.json` - last successfully imported
  production pin identities. `import-all --seed-imported-receipts` loads this
  into an empty receipt table only after event provenance counts for that pin
  match. Omit the flag on a fresh or incomplete database.

The `csv_path` values inside `historical/historical-source-manifest.json` are
relative to the external [merge-mining-research](https://github.com/deadmanoz/merge-mining-research)
source repository, not this repository's `data/` tree.

## Checks

Run `just check-data-artifacts` for local deterministic checks over this
directory and the generated source-registry artifacts.
