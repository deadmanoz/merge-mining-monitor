# Operations

Use `just` targets for routine work.
They encode the repository's expected environment and safety wrappers.

## Local Setup

```bash
just db-up
just db-migrate-dev
just build
just test
```

Copy `.env.example` to `.env` and adjust endpoints before running live pollers or Bitcoin Core classification.
A synced **Bitcoin mainnet** Core node is required for every capture, import, and reconciliation command, including database-only maintenance modes (`just rebuild-source-health`): each drains committed Core-suffix reconciliation before refreshing the sparse `bitcoin_core_header` cache through the current synced tip.
`serve` reads that persisted cache from Postgres and makes no Core RPC calls.

## Serving

```bash
just serve
```

The API serves JSON under `/api/v1/` and the static frontend from `www/`.
`SERVE_BIND_ADDR` defaults to `127.0.0.1:8080`.

The tree endpoint reads local Bitcoin backbone rows only.
Run `sync-bitcoin-core` before browsing windows you expect to be complete.

## Migrations

Real database migrations go through the backup-first wrapper:

```bash
just db-migrate-dev
just db-migrate-deploy
just db-backup
```

Do not run raw migration commands against a persistent database.

### Scheduled unknown-parent rechecks

Changes that can alter unknown-parent verdicts (a shallow Core reorg, a retarget boundary inside existing coverage, an empty cache, a horizon advance, or a classifier-rule migration) record a pending recheck on the Core-header cache state rather than scanning inside producer startup or a poll tick.
Consume pending rechecks with:

```bash
just reclassify-unknown-parents --scheduled
```

The job processes one page per exclusive cache-lock hold so live capture can interleave, persists its cursor after every page so an interrupted run resumes where it stopped, and exits when nothing is pending.
Run it after every migration-bearing release and whenever a producer logs a pending recheck.
The manual form `just reclassify-unknown-parents` (optionally `--recheck-orphans`) scans without a persisted cursor; prefer `--scheduled` (see `migrations/README.md` for how migrations schedule a recheck).

### Hathor displacement repair (0024)

When applying historical repair migrations (`0019`, `0024`, `0025`), follow a stop/migrate/start rollout: stop `poll-hathor` before `just db-migrate-deploy`, then after `0024` / `0025` reconcile the restored events and refresh source health before restarting the poller:

```bash
just reconcile-read-model --all --source auxpow:hathor
just rebuild-source-health
```

Before applying `0007` to an existing database (duplicate-identity audit query):

```sql
SELECT s.code,
       encode(e.child_block_hash, 'hex') AS child_block_hash,
       count(*) AS rows,
       array_agg(e.child_height ORDER BY e.child_height) AS child_heights
FROM merge_mining_event e
JOIN source s ON s.id = e.source_id
WHERE e.child_block_hash IS NOT NULL
GROUP BY s.code, e.child_block_hash
HAVING count(*) > 1
ORDER BY s.code, child_block_hash;
```

## Cleanup And Local Reset

`just clean` is non-destructive for database state.
It stops this checkout's Compose services, removes Rust build output, and clears ignored runtime scratch directories such as `.tmp/`, `logs/`, `test-results/`, and `playwright-report/`, but it preserves the Postgres Docker volume.

Use the explicit reset target only for disposable local databases:

```bash
just db-reset                    # refuses without confirmation
CONFIRM_DB_RESET=1 just db-reset # backs up, then removes the Postgres volume
just db-up
just db-migrate-dev
just rebuild-source-health
```

`db-reset` runs `just db-backup` first and deletes the volume only after that backup succeeds.
Do not use it for persistent or production databases.

## Live Capture

```bash
just poll-namecoin
just poll-rsk
just poll-syscoin
just poll-fractal
just poll-hathor
just poll-elastos
just poll-qbit
just poll-terracoin
```

Bounded backfills use:

```bash
just backfill-namecoin START END
just backfill-rsk START END
just backfill-syscoin START END
just backfill-fractal START END
just backfill-hathor START END
just backfill-elastos START END
just backfill-qbit START END
just backfill-terracoin START END
```

Every poll tick logs one `poll tick` line with the cursor, tip, rescanned and new height counts, whether any height held, how many non-Bitcoin parents the lineage gate refused (`non_bitcoin`, never stored; see `docs/capture.md`), tick duration, and per-tick RPC deltas (`attempts`, `elements`, `retries`, `failures`, `latency_avg_ms`) for the child chain and Bitcoin Core.
Batch jobs (backfills, reconciles, scheduled rechecks, queue drains) log progress (`done`, `total`, `rate`, `ETA`) at most every 30 seconds through `ProgressReporter` and always finish with a `job ended` summary plus cumulative per-client metrics including `latency_max_ms`.
Because production round-trip latency is ~340 ms per remote call, `attempts` per tick or per height is the primary diagnostic when a job is slower than expected.

Accept a newly activated live source by its parent mix, not only by its cursor.
The first ticks should show `non_bitcoin` counts in line with the chain's known parents (most Terracoin parents are Bitcoin Cash headers, so most of its ticks refuse some) and no open `capture_error` rows.
Then compare the source's `counts` in `/api/v1/sources` (near, unknown, strict and weak orphans) with the Research publication's classification counts over the overlapping heights.
Unknown or near counts that grow where Research records none for the same heights mean parents are being stored that should not be: stop the poller before they spread and investigate.

A hold-policy backfill (`qbit`, `terracoin`) exits non-zero when any height in the range holds an unresolved capture error.
The written evidence and the read-model repair still complete; the non-zero exit says the range is not fully covered.
Inspect the open heights with:

```sql
SELECT height, error_kind, detail
FROM capture_error
WHERE source_id = (SELECT id FROM source WHERE code = 'auxpow:qbit')
ORDER BY height;
```

Re-run the range once the node can serve them.
`/api/v1/sources` reports the earliest unresolved height as `sync.error_code = auxpow_capture_error` until each held height is reprocessed successfully.

### Activating Or Resetting A Live Source After Historical Import

When first starting a live poller (`poll-qbit`, `poll-terracoin`, etc.) against a database that already holds the source's imported Research publication (or after a controlled cursor reset) seed `<PREFIX>_START_HEIGHT` at or below the publication's proven contiguous coverage tip `C` (for example `C + 1 - reorg_depth` so the initial overlap tick live-captures `C` and populates `child_chain_head`).
Without `<PREFIX>_START_HEIGHT`, the poller seeds its cursor at `tip - reorg_depth` on the live node, leaving an uncaptured gap above `C` while `/api/v1/sources` still reports `live`.
Remove `<PREFIX>_START_HEIGHT` from `.env` only after `hash(C)` appears in `child_chain_head` with no open `capture_error` rows (pollers keep chain heads from the override height up while it is set), then restart for normal cursor-based resume.

## Bitcoin Core Backbone

`sync-bitcoin-core` fills the canonical Bitcoin header backbone that the tree browses.
It walks canonical heights from Core, writes complete canonical `block` rows with coinbase evidence, and refuses same-height conflicts or broken prev-hash links.

```bash
just sync-bitcoin-core --tip --limit 2016                       # advance the next contiguous page toward the tip
just sync-bitcoin-core --from-height <start> --to-height <end>  # bounded historical range
just sync-bitcoin-core --from-height <start> --to-height <end> --missing-only  # repair gaps in a range
just sync-bitcoin-core --follow                                 # long-lived catch-up-then-follow daemon
```

`--to-height` and `--limit` are mutually exclusive.
In `--follow` mode, each interval refreshes the sparse Core header cache and repairs a bounded near-tip window (`BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS`, default 64, minimum 16).
If a long outage leaves a divergent contiguous cursor below the live-tip window, follow mode first repairs the short divergent suffix ending at the cursor and then catches up to the tip in bounded pages.
Replaced suffixes atomically promote the active Core chain, retain displaced blocks as Core-attested `stale` evidence, and enqueue affected hashes in `bitcoin_core_reconcile_queue` (reporting `backbone_reorg_reconcile_pending` on `/api/v1/sources` until drained).
One-shot and historical sync commands do not perform automatic suffix replacement.

The tree endpoint never hydrates Core on demand.
`/api/v1/tree` returns HTTP 409 `backbone_unsynced` for heights that have not been synced yet, and `backbone_conflict` for inconsistent local rows.

On a fresh database the backbone starts empty: the header tree has no canonical tip (`no_canonical_tip`) and window requests return 409 `backbone_unsynced` until `sync-bitcoin-core` has filled the windows you want to browse.
Sync the newest default window and any historical ranges you need before treating `serve` as ready.

If either the live-tip view or the additional cursor-centred view has no unique, complete matching ancestor inside its configured window, the producer records `near_tip_reorg_repair_failed` with `common_ancestor_outside_window` details and changes no chain rows.
A temporary same-height canonical pair in the replacement suffix is repairable, but a duplicate or incomplete row cannot serve as the common ancestor.
A regressed target or contiguous cursor also fails closed.
For these deep or structurally ambiguous cases, stop `serve`, take a backup, inspect the recorded bounds and hashes, and rebuild the affected canonical rows under an operator-reviewed recovery before rerunning follow mode.
Do not delete the displaced branch: it is stale-block evidence.

## Source Health And Classification Repair

`just rebuild-source-health` recomputes the per-source `/api/v1/sources` rollup counters.
It is required on a fresh database and after bulk backfills: `/api/v1/sources` fails closed until the first rebuild sets `source_health_ready`.
Run it during a quiescent window (pollers stopped) so it sees a stable base.
Counters are maintained incrementally afterward, so re-running it is only needed to repair drift or once after an upgrade that changes a counter's definition (the release that takes near counts from `child_block_observation` is one).
`import-all` rebuilds source health once after all durable historical parent and dependent work has drained, so no separate rebuild is required after that command.

`reconcile-read-model --start-height` and `--end-height` are child-height bounds.
Either bound excludes exact events whose authenticated child height is unavailable.
Run an unbounded scan, optionally restricted with `--source`, to include those events.

`just reclassify-unknown-parents` upgrades `unknown` Bitcoin parents once Core can classify their headers (for example after a historical load that deferred classification).
Each invocation pages through all currently unknown parents; `--batch-size` controls the DB page size, not a per-run cap, so rerun it while it reports changed rows.
A transient `unknown` never demotes an already-proven `canonical` or `stale` row, so a Bitcoin Core gap costs nothing but a backlog of unknowns to sweep later.

`just reclassify-parent BITCOIN_HEADER_HASH` is the narrow repair path for one known parent.
It requires the same synced Core node as capture and runs the normal parent reconciler and dependent cascade, so source-health counters and child relationships remain consistent.
Use the ordinary Bitcoin display hash, for example:

```bash
just reclassify-parent 0000000000000000000198e12592edbe83c84a78f75b3f8d67a3fe2075ef2ffb
```

It is appropriate after deploying a newly supported live consensus rule such as `time_below_mtp`; it does not run a database migration or contact production unless the command is explicitly run against that environment.

`just import-known-stales --csv PATH --source-label LABEL` loads the operator known-stale membership (`known_stale_block`) from an upstream `stale-blocks.csv`-shaped dataset.
Load it once per database, after migrations and before dataset imports: `import-dataset` refuses to run against an empty membership, every producer entry path warns loudly when it is empty, and the orphan classifier excludes any member from strict/weak classification.
The import is atomic and strict by default (any malformed row aborts unless `--skip-malformed` is passed).
On a database that already classified rows before the membership existed, the import immediately and idempotently demotes contaminated strict/weak rows to `excluded` in the same transaction, maintaining `source_health` through the reconciler.
`just reclassify-known-stales` remains available to repeat that repair independently.
See `docs/historical-ingest.md` for the full fresh-database ordering.

## Historical Publication

Materialize the pinned research Git LFS objects, then import the complete publication and refresh pool attributions:

```bash
git -C "$MERGE_MINING_RESEARCH_DIR" lfs pull \
  --include="results/monitor-evidence/*_monitor_evidence.csv"
just import-all
just reclassify-pools
```

See `docs/historical-ingest.md` for manifest preflight, state comparison across research pins, lifecycle replacement semantics, and single-chain diagnostic imports (`just import-dataset CHAIN`).

## Live Test Deployment

The `live-test-*` targets provide a local validation workflow with processed range ledgers under `.tmp/live-test-deployment/` and logs under `logs/live-test-deployment/`.

Common sequence:

```bash
just live-test-init
just live-test-preflight
just live-test-capture-tips
just live-test-backfill-next namecoin 10000
just live-test-classify
just live-test-reconcile-all
just live-test-smoke
```
