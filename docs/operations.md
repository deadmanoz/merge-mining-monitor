# Operations

Use `just` targets for routine work.
They encode the repository's expected environment and safety wrappers.

## Dependencies

| Activity | Required inputs |
|---|---|
| Browse an existing database (`serve`) | Postgres with populated, ready read-model tables. No Bitcoin Core or child-node RPC calls. |
| Sync the Bitcoin backbone | A synced Bitcoin mainnet Core RPC endpoint, including block bodies for coinbase enrichment. |
| Poll or backfill a child chain | Bitcoin Core plus that chain's RPC/REST endpoint. Enable only the sources you can reach. |
| Import historical evidence | Bitcoin Core, known-stale membership, and the pinned Research publication with its Git LFS files. Child-chain nodes are not needed for the published records. |
| Reconcile or rebuild derived state | Normally Bitcoin Core, even for work using stored data: commands check Core readiness, drain pending Core-suffix work and refresh the header cache. An already-current `rebuild-protocol-commitments --if-needed` exits after a Postgres check. Pool reclassification uses stored identity evidence after the shared Core bootstrap. |

Hathor defaults to a public REST API with a second public endpoint as fallback; this is API polling, not HTML scraping.
Outages, throttling or unavailable block records can leave capture incomplete despite retries.
See [endpoint configuration](configuration.md#endpoint-availability) for defaults and backfill safeguards.
These are software defaults, not a claim about which endpoints any deployment uses.

Core coinbase retrieval fetches full block bodies. Historical enrichment therefore needs those bodies to be available, not just the headers.
A failed coinbase fetch is recorded by backbone sync; it does not prove that the block had no declarations or payouts.
Pruned-node completeness depends on the requested history and retained bodies; this is not a blanket `txindex` requirement.

## Database Size

On 23 October 2026, the production monitor database (0.11.0, live and recovered historical evidence) occupied approximately **17 GiB**, measured with `pg_database_size(current_database())`.
This is an example of observed occupancy, not a minimum requirement; node storage and backups are additional.

## Local Setup

Install Rust (the version required by `Cargo.toml`), `just`, Docker with Compose v2, and PostgreSQL client tools (`psql` and `pg_dump`, compatible with PostgreSQL 16).
Node.js/npm is needed for frontend checks; Git LFS is needed for historical publication files.
Compose starts Postgres only, not the monitor or any blockchain nodes.
Its example credentials and published database port are for a controlled development environment, not a public deployment.

1. Copy `.env.example` to `.env` and configure Postgres, a synced Bitcoin mainnet Core RPC endpoint, and only the child endpoints you intend to use. Keep credentials out of Git.
2. Create and migrate the database with the backup-first wrapper, then build:

   ```bash
   just db-up
   just db-migrate-dev
   just build
   ```

3. Load [known-stale membership](historical-ingest.md#known-stale-membership) before capture or publication import:

   ```bash
   just import-known-stales --csv path/to/stale-blocks.csv \
     --source-label "bitcoin-data/stale-blocks@<commit>"
   ```

   On a fresh database, this command first builds the shared Core header cache, which can take a while against a remote node before membership import starts.

4. Choose either or both evidence paths. For recovered data, follow [Running Imports](historical-ingest.md#running-imports), which checks the pinned Research files before `just import-all`. For live capture, configure a source's start height and run its `just poll-CHAIN` command. Starting at the tip does not backfill the chain; use bounded `just backfill-CHAIN START END` ranges for earlier records.
5. Fill the canonical window you want to browse, including the newest window for the default view. Replace `START` and `END` with a bounded range ending at the current Core tip:

   ```bash
   just sync-bitcoin-core --from-height START --to-height END
   just rebuild-source-health
   just rebuild-protocol-commitments --if-needed
   just rebuild-relationship-signals --if-needed
   ```

   Stop pollers while rebuilding source health; `import-all` already rebuilds it after its queue drains. Run `just sync-bitcoin-core --follow` as a long-lived process to keep canonical context current. Additional historical tree windows need their own backbone coverage.
6. Start `just serve` in another terminal. Check `/api/v1/sources` and `/api/v1/tree` at the configured bind address before considering the UI ready. Sources must return counters, not `internal_error`; the tree must return the intended window, not `no_canonical_tip`, `backbone_unsynced` or `backbone_conflict`. `/health` alone does not establish data readiness.

   For active capture, also check each enabled producer's runtime status: Waiting indicates a Core dependency problem, while Catching up can include startup or local recovery. Existing data can remain browsable while capture is degraded.

See [source-health and classification repair](#source-health-and-classification-repair) for interrupted or incomplete loads.

## Automatic Core recovery

Live pollers and `sync-bitcoin-core --follow` install shutdown handling and record their runtime identity before probing Core. Temporary transport failures, timeouts, warm-up, IBD, header lag and stale tips leave the process running in Waiting. One live Core transport attempt is made per probe. The retry ceiling starts at five seconds, doubles to 300 seconds, and each delay is drawn between half and all of that ceiling. Sources shows the chosen next retry only while capture needs another attempt. Within a run, backoff resets only after a completed healthy capture cycle, including an idle cycle at target. Partial progress or a previously caught-up cursor does not clear a failed cycle; a new process starts fresh escalation. Finite imports, backfills and one-shot sync retain their bounded transport retries.

A successful probe resumes durable cursor and queue work. Local cache contention, cache bootstrap and bounded reconciliation report Catching up and wait the ordinary producer interval. They do not consume the follower's five-stall budget. Wrong-network, authentication, invalid-response and proven integrity errors stop the process; systemd's restart policy remains the fallback. General PostgreSQL reconnection is outside this recovery path.

Heartbeat-only write failures retry at the next heartbeat interval, with the same run-ownership check. Live Hathor recovery reads at most 26 held heights per tick and attempts 25; the extra row records that more work remains.

During live startup, a block body still downloading or a tip-height race keeps capture in recovery at the ordinary producer interval. Queued reconciliation remains pending until it succeeds. These capture retries do not escalate Core-outage backoff or change finite-command behaviour.

Finite commands re-drain reconciliation work that arrives during cache preparation before retrying publication. They fail after three unsuccessful publication attempts and can be rerun; they do not wait indefinitely for concurrent writers. A live child poller reports healthy completion after successful capture or replay, or when idle exactly at its observed tip. A tip below the available rescan window remains Catching up while the monotonic cursor is preserved. Core miscellaneous RPC errors, including a block body not downloaded yet, remain capture errors retried at the ordinary interval rather than fatal invalid responses.

The shared live supervisor owns readiness, chain initialization, capture, waiting and shutdown under one cancellation boundary. Adapters construct and tick their producer; they do not manage separate startup or shutdown state. Cache publication allows a brief reader transaction to finish within a 500 ms lock budget, then yields for a fresh snapshot if contention persists.

Check `/api/v1/sources` as well as systemd status: a running unit in Waiting is degraded. Runtime heartbeat expiry is 90 seconds, independent of cursor age; a long tick renews its heartbeat every 30 seconds. After a clean stop, Sources and Capture show `Runtime: Producer stopped` with no scheduled retry. Error continues to take priority when unresolved evidence or backbone errors exist. `source_health_ready` remains an aggregate-integrity gate and is not a Core-availability flag.

For migration 0036, stop all Core-backed writers before running the backup-protected migration, then start the new binaries. Older writers do not increment `publication_revision` and must not overlap new cache publishers. Rolling back the binary is compatible with the additive schema, but stop new writers first and keep producer ownership exclusive. Keep the automatic backup until rollback verification is complete.

## Serving

```bash
just serve
```

The API serves JSON under `/api/v1/` and the static frontend from `www/`.
`SERVE_BIND_ADDR` defaults to `127.0.0.1:8080`.

The tree endpoint reads local Bitcoin backbone rows only.
Run `sync-bitcoin-core` before browsing windows you expect to be complete.

## Migrations

Before 1.0.0 the schema can change, and migrations may be consolidated for that release.
A continuous in-place upgrade path is not guaranteed; follow the [migration policy](../migrations/README.md#pre-10-schema-policy) and release-specific upgrade instructions.
The current migrations remain append-only and backup-first.

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

### Legacy upgrade: databases before 0007

The following audit is only for an existing database that has **not** applied `0007`.
It references columns removed by later migrations and must not be run against the current schema.
Fresh installations use the setup above and need no manual legacy audit.

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
Live producers (`poll-*` and `sync-bitcoin-core --follow`) stop on SIGINT or SIGTERM without finishing the current tick: a write that has already committed first finishes its dependent cascade (for up to 60 s), any open transaction rolls back, and the next start resumes from the persisted cursors and reconcile queues.
Exit can still wait up to 15 s for an in-flight Bitcoin Core call to return.

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
Counters are maintained incrementally afterward, so re-running it is only needed to repair drift or once after an upgrade that changes a counter's definition (0.10.0, which takes near counts from `child_block_observation`, is one, and its migration 0034 clears `source_health_ready`, so `/api/v1/sources` fails closed until the rebuild runs).
`import-all` rebuilds source health once after all durable historical parent and dependent work has drained, so no separate rebuild is required after that command.

`just rebuild-protocol-commitments` (optionally with `--batch-size N`, default 100 headers per page) fills the derived CORE and exSat `protocol_commitment` tables from stored coinbase outputs.
Run it once after upgrading to 0.11.0 (migration `0035`) and again after any release that raises the protocol-commitment decoder version.
It re-derives every stored script with the compiled decoder, then refreshes every header with a `block` row, a near observation with stored outputs or derived rows, one page per exclusive canonical-view hold so live capture and Core writes wait at most one page.
It records `protocol_commitments_decoder_version` only after the walk finishes; an interrupted run leaves the version unchanged, and a rerun is idempotent.
Producers keep the tables current afterward.

`just rebuild-relationship-signals` (optionally with `--batch-size N`) fills `relationship_summary`, the stored mixed-signals flags, after upserting the pool snapshot.
Run it with `--if-needed` after every upgrade, after `just rebuild-protocol-commitments`: it skips when the recorded fingerprint matches the running derivation, protocol decoder and pool snapshot, and it records the fingerprint only once the protocol commitments are current, so a run before then repeats next time.
The Core follower then keeps the table current by draining `relationship_refresh_queue` each tick.
`just sweep-relationship-signals` (with `--batch-size` blocks per page and `--max-iterations` pages per run) first turns queued pool-identity changes into header refreshes, then re-derives summaries from a saved cursor and logs a warning with the count of rows it corrected that no write had queued; schedule it like `reclassify-unknown-parents --scheduled` so every row is checked once per pass, and treat a non-zero drift as a write path that failed to queue its header.

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

After setup and known-stale membership import, materialize the pinned Research Git LFS objects, then import the publication and refresh pool attributions:

```bash
set -a; source .env; set +a # exports the Research path for git, unlike just's dotenv loading
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
