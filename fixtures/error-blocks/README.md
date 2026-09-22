# Reviewed body-invalid parents

These fixtures retain the 19 published observations of the ten reviewed
body-invalid Bitcoin parents from merge-mining-research commit
`de4e68ff0234eec9cfe6f6846b965839c296b877`.

`body-invalid-observations.csv` is an exact row subset of that commit's
`results/monitor-evidence/error-block-observations_monitor_evidence.csv`.
`body-invalid-parents.json` selects the distinct parent headers, heights,
hashes and rejection reasons for import and classification assertions.

The database regression imports these witnesses, restores their former stale
state and ordinary publication provenance, then repeats the normal
error-observation import followed by authoritative snapshot cleanup. It checks event
identity and evidence preservation, promotion of all ten parents, API exclusion
from stale competition, and idempotent replay. Core verdicts are scripted;
body-rule verification remains the pinned Research admission's responsibility.
