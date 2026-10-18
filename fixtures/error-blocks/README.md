# Reviewed body-invalid parents

These fixtures retain the 19 published observations of the ten reviewed body-invalid Bitcoin parents from merge-mining-research commit `de4e68ff0234eec9cfe6f6846b965839c296b877`.

`body-invalid-observations.csv` is an exact row subset of that commit's `results/monitor-evidence/error-block-observations_monitor_evidence.csv`.
`body-invalid-parents.json` selects the distinct parent headers, heights, hashes and rejection reasons for import and classification assertions.

Used by DB integration tests to verify error-observation import, promotion of all ten parents to `error_block`, exclusion from stale competition, and idempotent snapshot cleanup with scripted Core verdicts.
