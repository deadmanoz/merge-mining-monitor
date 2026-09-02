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
or Bitcoin Core classification. A synced Bitcoin Core node is required for every
capture, import, and reconciliation command, including database-only
maintenance modes. Each drains committed Core-suffix reconciliation before
refreshing the sparse `bitcoin_core_header` cache through the current synced tip.
`serve` reads that cache from Postgres and makes no Core RPC calls.

The current retarget boundary is marked final only after Core re-reads it at 100
blocks behind that tip. Cache refreshes verify that Core's tip did not change
while the sparse header snapshot was read. A shallow replacement reclassifies
existing orphan rows before the cache lock is released, while settled epoch
history is retained.

After applying migration `0010`, configure and sync a **Bitcoin mainnet** Core
node before running any command that writes or rebuilds monitor data, including
`just rebuild-source-health`. There is intentionally no database-only bypass:
the node is an operational requirement, while the API remains available from the
persisted cache.

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

Migration 0009 removes the RSK reclassification watermark used by older
`reclassify-pools` binaries. Activate the watermark-free application package
and confirm that no older or in-flight `reclassify-pools` process remains
before running the backup-first migration. The new binary works both before
and after the table is dropped. A binary-only rollback to an older release is
not valid after 0009: recreate the empty singleton table (the old command will
populate it) or restore the pre-migration backup before activating the old
binary.

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

In follow mode, each batch also refreshes the sparse Core header cache, even
when no child-chain poller is running. A transient Core or database failure
retries on the next interval without discarding backbone progress from that
batch. A non-mainnet Core node or inconsistent persisted cache is an integrity
failure and stops follow mode for operator repair.

`--to-height` and `--limit` are mutually exclusive, so range and page semantics
stay unambiguous. Follow mode keeps a contiguous local cursor and, during each
interval, repairs a bounded near-tip window so sparse Core-attested rows cannot
leave the Live tip view stale. That window defaults to 64 heights and is
tunable with `BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS` (minimum 16). The producer
pins one Core tip, walks headers backward through the window plus one
common-ancestor anchor, and rechecks that target after taking the exclusive
canonical-view barrier and again immediately before commit. Missing or
incomplete matching rows use the ordinary `missing_only` fill. A conflicting
suffix inside the window is replaced in one transaction after every
replacement coinbase has been fetched. If an in-flight classifier commits a
same-height conflict after the initial scan but before the ordinary fill takes
the canonical-view barrier, the repair reloads the pinned view and plan once so
that conflict follows the suffix-replacement path.

After a long outage, the contiguous cursor can sit below that live-tip window.
If the cursor hash no longer matches Core, the next scheduled repair sweep
captures one additional view ending at the cursor and uses the same configured
window depth to find a complete matching ancestor. It replaces only the short
divergent suffix through the cursor. The ordinary paged follow loop then catches
up the potentially much larger distance to the live tip; that lag is not folded
into one replacement transaction.

The suffix transaction promotes the active Core headers, retains each displaced
header as a Core-attested `stale` block pointing at its same-height canonical
competitor, updates active AuxPoW event classifications and source health, and
advances the contiguous cursor only when the replacement joins the already
proven prefix. It also writes every changed parent to
`bitcoin_core_reconcile_queue`. Follow mode drains that queue after the commit,
and before later sync or cache-refresh work, including process bootstrap and the
first follow tick after a restart. The suffix takes the shared cache lock before
the exclusive canonical-view barrier, so a concurrent cache refresh either
drains its committed queue or finishes before the suffix can commit.
Each row persists both the parent reconcile and the later expansion that
discovers deeper dependents, so a process exit or bounded-drain stop cannot lose
the cascade frontier. Parent replay uses strict live Core classification, so a
new child made inferably stale by the replacement is not completed as unknown
after a transient classification failure. While durable work remains,
`/api/v1/sources` reports `backbone_reorg_reconcile_pending` instead of treating
the Bitcoin source as healthy. If that pending marker temporarily covers an
unrelated or out-of-range producer failure, the exact prior error tuple is
restored after the queue drains. A same-height or link conflict inside the
committed replacement suffix is consumed because that transaction resolved it.

Repair-only statuses are cleared after the producer accepts a verified live
target, but they never replace an unrelated persisted producer failure. This
keeps an incomplete lower cursor visible after a temporary repair failure has
resolved.

After either gap fill or suffix replacement, the producer verifies the window
against Core: every expected height must have exactly one complete canonical
row, prev-hash links must be contiguous, and the local tip hash must match the
captured Core target. One-shot and explicit historical sync commands retain
their strict same-height and link-conflict guards; automatic suffix replacement
is a follow-mode near-tip operation only.

The tree endpoint never hydrates Core on demand. `/api/v1/tree` returns HTTP 409
`backbone_unsynced` for heights that have not been synced yet, and
`backbone_conflict` for inconsistent local rows.

On a fresh database the backbone starts empty: the header tree has no canonical
tip (`no_canonical_tip`) and window requests return 409 `backbone_unsynced`
until `sync-bitcoin-core` has filled the windows you want to browse. Sync the
newest default window and any historical ranges you need before treating
`serve` as ready.

If either the live-tip view or the additional cursor-centred view has no unique,
complete matching ancestor inside its configured window, the producer records
`near_tip_reorg_repair_failed` with `common_ancestor_outside_window` details and
changes no chain rows. A temporary same-height canonical pair in the
replacement suffix is repairable,
but a duplicate or incomplete row cannot serve as the common ancestor. A
regressed target or contiguous cursor also fails closed. For these deep or
structurally ambiguous cases, stop `serve`, take a backup, inspect the recorded
bounds and hashes, and rebuild the affected canonical rows under an
operator-reviewed recovery before rerunning follow mode. Do not delete the
displaced branch: it is stale-block evidence.

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

`just reclassify-parent BITCOIN_HEADER_HASH` is the narrow repair path for one
known parent. It requires the same synced Core node as capture and runs the
normal parent reconciler and dependent cascade, so source-health counters and
child relationships remain consistent. Use the ordinary Bitcoin display hash,
for example:

```bash
just reclassify-parent 0000000000000000000198e12592edbe83c84a78f75b3f8d67a3fe2075ef2ffb
```

It is appropriate after deploying a newly supported live consensus rule such
as `time_below_mtp`; it does not run a database migration or contact production
unless the command is explicitly run against that environment.

After applying migration 0008, run the following unbounded full scan without
child-height bounds:

```bash
just reconcile-read-model --all --batch-size 1000 --max-iterations 100000
```

The 1,037,005-row publication plus live-producer rows exceeds the default
budget, so this uses the deployment smoke test's full-scan ceiling. It revisits
captured headers, records catalogue matches as `error_block`, and rebuilds
source health. Error blocks are neither stale nor orphan evidence, so no orphan
reclassification is required.

`just import-known-stales --csv PATH --source-label LABEL` loads the operator
known-stale membership (`known_stale_block`) from an upstream
`stale-blocks.csv`-shaped dataset. Load it once per database, after migrations
and before dataset imports: `import-dataset` refuses to run against an empty
membership, every producer entry path warns loudly when it is empty, and the
orphan classifier excludes any member from strict/weak classification. The
import is atomic and strict by default (any malformed row aborts unless
`--skip-malformed` is passed). On a database that already classified rows
before the membership existed, the import immediately and idempotently demotes
contaminated strict/weak rows to `excluded` in the same transaction, maintaining
`source_health` through the reconciler. `just reclassify-known-stales` remains
available to repeat that repair independently. See
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
mutation. It then compares normalized publication-owned fields with stored
non-operator provenance and base events across research pins. Matching files
skip classification, writes, and authoritative reconciliation before the
Bitcoin Core lock is taken. Changed files reuse compatible Core-attested
canonical or structurally complete stale classifications already proven in the
derived `block` state. Event-only canonical, unknown, or incompatible parents
still require strict live Core classification. The dedicated error-observation
aggregate also retains its Core-plus-catalogue check. Historical and partial
sources require an exact base-event set, live sources permit additional rows,
retained error observations use subset semantics, and Doichain is an explicit
surveyed zero-row source. Database-only enrichment is accepted when the
publication omitted that field.

Pending historical queue, source-health, or published-stale work produces a
finalization-only run instead of replaying source files. Each mismatched chain
commits atomically. Parent read-model work drains from the durable historical
queue in bounded transactions, retaining cascade seeds until dependent work
succeeds. Stop live pollers during a production cutover and retain the backup
until event totals, block details, orphan exclusions, and poller health are
verified.

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
