# Operations

Use `just` targets for routine work. They encode the repository's expected
environment and safety wrappers.

## Local Setup

```bash
just db-up
just db-migrate-dev
just build
just test
```

Copy `.env.example` to `.env` and adjust endpoints before running live pollers
or Bitcoin Core classification.

## Serving

```bash
just serve
```

The API serves JSON under `/api/v1/` and the static frontend from `www/`.
`SERVE_BIND_ADDR` defaults to `127.0.0.1:8080`.

The tree endpoint reads local Bitcoin backbone rows only. Run
`sync-bitcoin-core` before browsing windows you expect to be complete.

## Migrations

Real database migrations go through the backup-first wrapper:

```bash
just db-migrate-dev
just db-migrate-deploy
just db-backup
```

Do not run raw migration commands against a persistent database.

Migration 0007 preserves existing event values while making child evidence
nullable. Its publication cutover is a separate, explicit operation. Stop live
pollers, back up, apply the migration, and run `just import-all`; authoritative
historical sources then remove obsolete rows, while live sources remain
additive. See `docs/historical-ingest.md`.

Before applying 0007 to an existing database, audit the exact child identities:

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

The result must be empty. Migration 0007 repeats this check and aborts before
altering the schema if it finds a conflict. Resolve each conflict against the
authenticated child-chain evidence; do not automatically keep or delete a row
based on height, activity, or insertion order.

## Cleanup And Local Reset

`just clean` is non-destructive for database state. It stops this checkout's
Compose services, removes Rust build output, and clears ignored runtime scratch
directories such as `.tmp/`, `logs/`, `test-results/`, and
`playwright-report/`, but it preserves the Postgres Docker volume.

Use the explicit reset target only for disposable local databases:

```bash
just db-reset                    # refuses without confirmation
CONFIRM_DB_RESET=1 just db-reset # backs up, then removes the Postgres volume
just db-up
just db-migrate-dev
just rebuild-source-health
```

`db-reset` runs `just db-backup` first and deletes the volume only after that
backup succeeds. Do not use it for persistent or production databases.

## Live Capture

```bash
just poll-namecoin
just poll-rsk
just poll-syscoin
just poll-fractal
just poll-hathor
just poll-elastos
```

Bounded backfills use:

```bash
just backfill-namecoin START END
just backfill-rsk START END
just backfill-syscoin START END
just backfill-fractal START END
just backfill-hathor START END
just backfill-elastos START END
```

## Bitcoin Core Backbone

`sync-bitcoin-core` fills the canonical Bitcoin header backbone that the tree
browses. It walks canonical heights from Core, writes complete canonical `block`
rows with coinbase evidence, and refuses same-height conflicts or broken
prev-hash links.

```bash
just sync-bitcoin-core --tip --limit 2016                       # advance the next contiguous page toward the tip
just sync-bitcoin-core --from-height <start> --to-height <end>  # bounded historical range
just sync-bitcoin-core --from-height <start> --to-height <end> --missing-only  # repair gaps in a range
just sync-bitcoin-core --follow                                 # long-lived catch-up-then-follow daemon
```

`--to-height` and `--limit` are mutually exclusive, so range and page semantics
stay unambiguous. Follow mode keeps a contiguous local cursor and, during each
interval, repairs a bounded near-tip window (`missing_only`) so sparse
Core-attested rows cannot leave the Live tip view stale. That window defaults to
64 heights and is tunable with `BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS` (minimum
16). After a repair, the producer verifies the window against Core: every
expected height must have exactly one complete canonical row, prev-hash links
must be contiguous, and the local tip hash must match the captured Core tip.

The tree endpoint never hydrates Core on demand. `/api/v1/tree` returns HTTP 409
`backbone_unsynced` for heights that have not been synced yet, and
`backbone_conflict` for inconsistent local rows.

On a fresh database the backbone starts empty: the header tree has no canonical
tip (`no_canonical_tip`) and window requests return 409 `backbone_unsynced`
until `sync-bitcoin-core` has filled the windows you want to browse. Sync the
newest default window and any historical ranges you need before treating
`serve` as ready.

Sync does not automatically rewrite already-synced rows after a near-tip
Bitcoin reorg; automatic trailing reorg repair is not implemented yet. If
`sync-bitcoin-core --tip` fails with `backbone_link_mismatch` after a reorg
inside synced heights, stop `serve`, take a backup, and rebuild the affected
canonical rows before rerunning sync.

## Source Health And Classification Repair

`just rebuild-source-health` recomputes the per-source `/api/v1/sources` rollup
counters. It is required on a fresh database and after bulk backfills:
`/api/v1/sources` fails closed until the first rebuild sets
`source_health_ready`. Run it during a quiescent window (pollers stopped) so it
sees a stable base. Counters are maintained incrementally afterward, so
re-running it is only needed to repair drift. `import-all` rebuilds source
health once after all durable historical parent and dependent work has drained,
so no separate rebuild is required after that command.

`reconcile-read-model --start-height` and `--end-height` are child-height
bounds. Either bound excludes exact events whose authenticated child height is
unavailable. Run an unbounded scan, optionally restricted with `--source`, to
include those events.

`just reclassify-unknown-parents` upgrades `unknown` Bitcoin parents once Core can
classify their headers (for example after a historical load that deferred
classification). Each invocation pages through all currently unknown parents;
`--batch-size` controls the DB page size, not a per-run cap, so rerun it while it
reports changed rows. A transient `unknown` never demotes an already-proven
`canonical` or `stale` row, so a Bitcoin Core gap costs nothing but a backlog of
unknowns to sweep later.

After applying migration 0008, run `just reconcile-read-model --all` without
child-height bounds. The full scan revisits already-captured proof-of-work-valid
headers, including existing `unknown` rows that predate the pinned catalogue,
records every match as `error_block`, and rebuilds the separate source-health
counter. An error block is not stale or orphan evidence, so no orphan
reclassification command is required.

`just import-known-stales --csv PATH --source-label LABEL` loads the operator
known-stale membership (`known_stale_block`) from an upstream
`stale-blocks.csv`-shaped dataset. Load it once per database, after migrations
and before dataset imports: `import-dataset` refuses to run against an empty
membership, every producer entry path warns loudly when it is empty, and the
orphan classifier excludes any member from strict/weak classification. The
import is atomic and strict by default (any malformed row aborts unless
`--skip-malformed` is passed). On a database that already classified rows
before the membership existed, `just reclassify-known-stales` retroactively
demotes contaminated strict/weak rows to `excluded`, idempotently, and
maintains `source_health` through the reconciler. See
`docs/historical-ingest.md` for the full fresh-database ordering.

## Historical Publication

Materialize the pinned research Git LFS objects, then import the whole
publication:

```bash
git -C "$MERGE_MINING_RESEARCH_DIR" lfs pull \
  --include="results/monitor-evidence/*_monitor_evidence.csv"
just import-all
just reclassify-pools
```

`import-all` verifies all 27 per-chain artifacts before the first database
mutation. Each chain then commits atomically. Historical and partial sources
are authoritative snapshots; live sources are additive; Doichain is a surveyed
zero-row no-op. Parent read-model work then drains from the durable historical
queue in bounded transactions, retaining cascade seeds until their dependent
work succeeds. Stop live pollers during a production cutover and retain the
backup until event totals, block details, orphan exclusions, and poller health
are verified.

## Live Test Deployment

The `live-test-*` targets provide a local validation workflow with processed
range ledgers under `.tmp/live-test-deployment/` and logs under
`logs/live-test-deployment/`.

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
