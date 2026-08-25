//! Bitcoin Core backbone live producer.
//!
//! The continuous catch-up-then-follow-tip managed-service mode wrapping the
//! one-shot `super::run_sync_bitcoin_core` batch, plus its cursor/decision
//! helpers. Split out of the parent module to keep each file under the
//! arch-lint file-size budget.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mmm_bitcoin_core::ConfiguredParentClassifier;
use serde_json::json;
use tokio_postgres::Client;

use crate::bitcoin_epoch_cache::refresh_bitcoin_core_header_cache;
use crate::live_loop::{LiveProducer, TickOutcome, run_live_loop};

use super::{
    BitcoinCoreBackboneSource, BitcoinCoreBackboneTip, BitcoinCoreSyncConfig, BitcoinCoreSyncStats,
    RETRYABLE_REPAIR_ERROR_CODE, SYNC_MODE_CONTIGUOUS, TARGET_TIP_CHANGED_ERROR_CODE,
    accept_live_repaired_target, is_backbone_integrity_error, load_or_init_sync_state,
    repair_near_tip_backbone_to_target, run_sync_bitcoin_core, update_sync_error,
    verify_live_backbone_window,
};
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use mmm_read_model::drain_core_reconcile_queue;
use mmm_store::get_source_id;

/// Consecutive no-forward-progress batches below tip before the live producer
/// fail-stops, so a stuck cursor surfaces as a stopped service rather than a
/// healthy-looking one. At the default 60s interval this is ~5 minutes.
const FOLLOW_STALL_EXIT_THRESHOLD: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FollowProgress {
    contiguous_complete_height: i32,
    target_tip_height: Option<i32>,
}

impl FollowProgress {
    fn caught_up(self) -> bool {
        self.target_tip_height
            .is_some_and(|tip| self.contiguous_complete_height >= tip)
    }
}

// Normalize the follow invariants at this public entry point so a direct caller
// (not via from_args) still gets correct catch-up-then-follow-tip behavior:
// --tip is required for live mode to record and track the target tip (otherwise
// target_tip stays NULL and the stall logic treats it as caught up, idling below
// tip); missing_only keeps retries and the forward crawl from re-fetching
// already-complete rows; and live mode always works from the persisted cursor
// to the live tip, never a fixed height range.
fn normalize_follow_config(config: &mut BitcoinCoreSyncConfig) {
    config.tip = true;
    config.missing_only = true;
    config.from_height = None;
    config.to_height = None;
}

/// Pure stall accounting. Returns `(next_stall, should_exit)`.
///
/// A batch counts as a STALL when it neither made forward progress NOR is caught
/// up at the tip: an `Ok` no-progress batch below tip, or a transient `Err`
/// (the caller passes `caught_up = false, progressed = false`). Idling at tip
/// (`caught_up`) resets the counter. `FOLLOW_STALL_EXIT_THRESHOLD` consecutive
/// stalls trip a fail-stop so neither a Core-unservable height nor a persistent
/// fetch failure can silently pin a running producer.
fn follow_stall_step(caught_up: bool, progressed: bool, stall: usize) -> (usize, bool) {
    if progressed || caught_up {
        (0, false)
    } else {
        let next = stall + 1;
        (next, next >= FOLLOW_STALL_EXIT_THRESHOLD)
    }
}

/// Whether the live-window repair sweep is due: always on the first pass (no
/// prior repair), then once at least `interval` has elapsed since the last one.
/// The boundary is inclusive (`>=`), so a repair exactly at the interval runs.
fn near_tip_repair_due(last_repair: Option<Instant>, interval: Duration) -> bool {
    last_repair.is_none_or(|last_repair| last_repair.elapsed() >= interval)
}

enum NearTipRepairSchedule {
    IfDue {
        last_repair: Option<Instant>,
        interval: Duration,
    },
    Force,
}

impl NearTipRepairSchedule {
    fn skipped_timestamp(&self) -> Option<Instant> {
        match self {
            Self::IfDue {
                last_repair,
                interval,
            } if !near_tip_repair_due(*last_repair, *interval) => *last_repair,
            Self::IfDue { .. } | Self::Force => None,
        }
    }
}

/// Snapshot the current Core tip to use as the live-window repair target. Held
/// as a unit (height + hash) so the repair and its post-verify can detect the
/// tip moving mid-sweep and treat it as an invariant failure.
async fn capture_live_window_target<S>(source: &S) -> Result<BitcoinCoreBackboneTip>
where
    S: BitcoinCoreBackboneSource,
{
    source
        .tip()
        .await
        .context("fetch Bitcoin Core live backbone window target tip")
}

/// Run the live-window repair sweep at most once per `interval`, returning the
/// updated timestamp and whether it is safe to proceed with the ordinary batch.
/// On a transient capture or repair failure it stamps "now" and returns false,
/// preventing the strict ordinary guard from racing ahead of a repair retry. A
/// successful repair is followed by invariant verification and acceptance of
/// the pinned target; a structural breach propagates to fail-stop.
async fn repair_near_tip_backbone_if_due<S>(
    client: &mut Client,
    source: &S,
    source_id: i64,
    schedule: NearTipRepairSchedule,
    delay: Duration,
    window_heights: i32,
) -> Result<(Option<Instant>, bool)>
where
    S: BitcoinCoreBackboneSource,
{
    if let Some(last_repair) = schedule.skipped_timestamp() {
        return Ok((Some(last_repair), true));
    }

    let target = match capture_live_window_target(source).await {
        Ok(target) => target,
        Err(err) => {
            tracing::warn!(
                error = format!("{err:#}"),
                "Bitcoin Core near-tip repair target capture failed; retrying after interval"
            );
            record_retryable_repair_error(client, source_id, None, "target_capture_failed", &err)
                .await;
            return Ok((Some(Instant::now()), false));
        }
    };
    let tip_height = target.height;
    let result = async {
        let stats = repair_near_tip_backbone_to_target(
            client,
            source,
            source_id,
            target,
            delay,
            window_heights,
        )
        .await?;
        if stats.coinbase_failed == 0 {
            verify_live_backbone_window(client, source, target, window_heights).await?;
            accept_live_repaired_target(client, source, source_id, target).await?;
        }
        Ok(stats)
    }
    .await;
    let retry_failure = match &result {
        Ok(stats) if stats.coinbase_failed > 0 => Some((
            "coinbase_incomplete",
            format!(
                "Bitcoin Core near-tip repair left {} coinbase fetch failures",
                stats.coinbase_failed
            ),
        )),
        Err(err) if !is_backbone_integrity_error(err) => {
            Some(("transient_repair_failure", format!("{err:#}")))
        }
        _ => None,
    };
    let repair_succeeded = handle_near_tip_repair_result(tip_height, result)?;
    if !repair_succeeded {
        let (reason, message) =
            retry_failure.expect("a retryable repair result always carries failure details");
        record_retryable_repair_message(client, source_id, Some(target), reason, &message).await;
    }
    Ok((Some(Instant::now()), repair_succeeded))
}

async fn record_retryable_repair_error(
    client: &mut Client,
    source_id: i64,
    target: Option<BitcoinCoreBackboneTip>,
    reason: &str,
    err: &anyhow::Error,
) {
    record_retryable_repair_message(client, source_id, target, reason, &format!("{err:#}")).await;
}

async fn record_retryable_repair_message(
    client: &mut Client,
    source_id: i64,
    target: Option<BitcoinCoreBackboneTip>,
    reason: &str,
    message: &str,
) {
    if let Err(record_err) =
        try_record_retryable_repair_message(client, source_id, target, reason, message).await
    {
        tracing::warn!(
            error = format!("{record_err:#}"),
            "could not persist retryable Bitcoin Core near-tip repair failure"
        );
    }
}

async fn try_record_retryable_repair_message(
    client: &mut Client,
    source_id: i64,
    target: Option<BitcoinCoreBackboneTip>,
    reason: &str,
    message: &str,
) -> Result<()> {
    let txn = client
        .transaction()
        .await
        .context("begin retryable Bitcoin Core repair status update")?;
    // The upsert locks the sync-state row. Check the queue and status ownership in
    // a separate READ COMMITTED statement so a writer that held the row first
    // is visible after this transaction waits for it. Retry bookkeeping may
    // replace a repair-owned status, but never masks another producer failure.
    let state = load_or_init_sync_state(&txn, source_id).await?;
    let may_record_retry: bool = txn
        .query_one(
            "SELECT NOT EXISTS ( \
                 SELECT 1 FROM bitcoin_core_reconcile_queue WHERE source_id = $1 \
             ) AND (s.last_error_code IS NULL \
                 OR s.last_error_code IN ($3, $4)) \
             FROM bitcoin_core_sync_state s \
             WHERE s.source_id = $1 AND s.sync_mode = $2",
            &[
                &source_id,
                &SYNC_MODE_CONTIGUOUS,
                &RETRYABLE_REPAIR_ERROR_CODE,
                &TARGET_TIP_CHANGED_ERROR_CODE,
            ],
        )
        .await
        .context("check Bitcoin Core status ownership before recording repair retry")?
        .get(0);
    if !may_record_retry {
        txn.commit()
            .await
            .context("commit preserved Bitcoin Core sync status")?;
        return Ok(());
    }

    let height = target
        .map(|target| target.height)
        .or(state.target_tip_height)
        .unwrap_or(state.contiguous_complete_height);
    let details = json!({
        "reason": reason,
        "retryable": true,
        "target_tip_height": target.map(|target| target.height),
        "target_tip_hash": target.map(|target| target.hash.to_string()),
    });
    update_sync_error(
        &txn,
        source_id,
        height,
        RETRYABLE_REPAIR_ERROR_CODE,
        message,
        details,
    )
    .await?;
    txn.commit()
        .await
        .context("commit retryable Bitcoin Core repair status")
}

#[cfg(any(test, feature = "db-integration"))]
#[doc(hidden)]
pub async fn record_retryable_repair_failure_for_test(
    client: &mut Client,
    source_id: i64,
) -> Result<()> {
    try_record_retryable_repair_message(
        client,
        source_id,
        None,
        "test_retryable_failure",
        "test retryable Bitcoin Core near-tip repair failure",
    )
    .await
}

/// Classify a repair sweep's outcome into "verify the window now" (`Ok(true)`)
/// versus "retry later" (`Ok(false)`), or propagate a fail-stop. An integrity
/// error is re-raised so the live producer exits. A transient error is logged and
/// retried. A success that still left `coinbase_failed > 0` returns `false` so
/// window verification is deferred until a later, clean repair.
fn handle_near_tip_repair_result(
    tip_height: i32,
    result: Result<BitcoinCoreSyncStats>,
) -> Result<bool> {
    match result {
        Ok(stats) => {
            log_near_tip_repair_stats(tip_height, &stats);
            if stats.coinbase_failed > 0 {
                tracing::warn!(
                    attempted = stats.attempted,
                    completed = stats.completed,
                    skipped_complete = stats.skipped_complete,
                    coinbase_failed = stats.coinbase_failed,
                    target_tip_height = tip_height,
                    "Bitcoin Core near-tip repair left coinbase fetch failures; retrying after interval"
                );
                Ok(false)
            } else {
                Ok(true)
            }
        }
        Err(err) if is_backbone_integrity_error(&err) => Err(err),
        Err(err) => {
            tracing::warn!(
                error = format!("{err:#}"),
                target_tip_height = tip_height,
                "Bitcoin Core near-tip repair failed; retrying after interval"
            );
            Ok(false)
        }
    }
}

/// Emit an info log for a repair sweep only when it did real work (filled a hole
/// or hit a coinbase failure), so a steady-state no-op sweep stays quiet.
fn log_near_tip_repair_stats(tip_height: i32, stats: &BitcoinCoreSyncStats) {
    if stats.completed > 0 || stats.coinbase_failed > 0 {
        tracing::info!(
            attempted = stats.attempted,
            completed = stats.completed,
            skipped_complete = stats.skipped_complete,
            coinbase_failed = stats.coinbase_failed,
            target_tip_height = tip_height,
            "repaired Bitcoin Core near-tip window"
        );
    }
}

fn wait_after_live_tick(
    outcome: TickOutcome,
    stall: &mut usize,
    follow_interval: Duration,
) -> Result<Duration> {
    let (next_stall, should_exit) =
        follow_stall_step(outcome.idle_at_target, outcome.progressed, *stall);
    *stall = next_stall;
    if should_exit {
        bail!(
            "Bitcoin Core backbone live producer stalled below tip after \
             {FOLLOW_STALL_EXIT_THRESHOLD} consecutive no-progress batches"
        );
    }
    if outcome.progressed {
        // Still catching up: loop immediately, but stay shutdown-aware.
        Ok(Duration::ZERO)
    } else {
        Ok(follow_interval)
    }
}

struct BitcoinCoreLiveProducer<'a, S>
where
    S: BitcoinCoreBackboneSource,
{
    client: &'a mut Client,
    source: &'a S,
    source_id: i64,
    initial_cch: i32,
    header_cache_classifier: &'a ConfiguredParentClassifier,
    config: BitcoinCoreSyncConfig,
    stall: usize,
    last_near_tip_repair_at: Option<Instant>,
}

impl<S> BitcoinCoreLiveProducer<'_, S>
where
    S: BitcoinCoreBackboneSource,
{
    async fn bookkeeping_failure_outcome(
        &self,
        progress_before: Option<FollowProgress>,
    ) -> TickOutcome {
        let progress_after = load_follow_progress(self.client, self.source_id).await.ok();
        bookkeeping_failure_outcome_from(progress_before, progress_after)
    }

    async fn repair_near_tip(&mut self, schedule: NearTipRepairSchedule) -> Result<bool> {
        let (last_repair, repair_succeeded) = repair_near_tip_backbone_if_due(
            self.client,
            self.source,
            self.source_id,
            schedule,
            self.config.delay,
            self.config.near_tip_repair_window_heights,
        )
        .await?;
        self.last_near_tip_repair_at = last_repair;
        Ok(repair_succeeded)
    }
}

fn bookkeeping_failure_outcome_from(
    progress_before: Option<FollowProgress>,
    progress_after: Option<FollowProgress>,
) -> TickOutcome {
    let progressed = progress_before
        .zip(progress_after)
        .is_some_and(|(before, after)| {
            after.contiguous_complete_height > before.contiguous_complete_height
        });
    let idle_at_target = progress_after
        .or(progress_before)
        .map(FollowProgress::caught_up)
        .unwrap_or(false);
    TickOutcome {
        progressed,
        idle_at_target,
    }
}

/// Return whether follow mode can continue this tick after refreshing the
/// derived Core-header cache. Cache transport and database failures are retried
/// like the backbone batch; typed cache or backbone integrity failures fail-stop.
fn handle_header_cache_refresh_result(result: Result<()>) -> Result<bool> {
    match result {
        Ok(()) => Ok(true),
        Err(err)
            if is_backbone_integrity_error(&err)
                || mmm_store::is_bitcoin_core_header_cache_integrity_error(&err) =>
        {
            Err(err)
        }
        Err(err) => {
            tracing::warn!(
                error = format!("{err:#}"),
                "Bitcoin Core header-cache refresh failed; retrying after interval"
            );
            Ok(false)
        }
    }
}

impl<S> LiveProducer for BitcoinCoreLiveProducer<'_, S>
where
    S: BitcoinCoreBackboneSource,
{
    fn name(&self) -> &'static str {
        "Bitcoin Core backbone"
    }

    async fn tick(&mut self) -> Result<TickOutcome> {
        if let Err(err) = drain_core_reconcile_queue(self.client, self.source_id).await {
            tracing::warn!(
                error = format!("{err:#}"),
                "Bitcoin Core pending reorg cascade failed; retrying before further sync work"
            );
            return Ok(self.bookkeeping_failure_outcome(None).await);
        }

        let progress_before = match load_follow_progress(self.client, self.source_id).await {
            Ok(progress) => progress,
            Err(err) => {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "Bitcoin Core backbone live bookkeeping read failed before batch; retrying after interval"
                );
                return Ok(self.bookkeeping_failure_outcome(None).await);
            }
        };

        let repair_succeeded = self
            .repair_near_tip(NearTipRepairSchedule::IfDue {
                last_repair: self.last_near_tip_repair_at,
                interval: self.config.follow_interval,
            })
            .await?;
        if !repair_succeeded {
            return Ok(TickOutcome {
                // A partial gap/suffix commit is not safe progress until the
                // complete repair, queue drain, verification, and target
                // acceptance sequence succeeds. Waiting one interval also
                // makes the scheduled repair due again on the next tick.
                progressed: false,
                // Height equality is insufficient on a same-height fork. A
                // target is idle only after its hash has been accepted by a
                // successful repair/verification pass.
                idle_at_target: false,
            });
        }

        match run_sync_bitcoin_core(self.client, self.source, self.config.clone()).await {
            Ok(_stats) => {}
            Err(err) if is_backbone_integrity_error(&err) => {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "Bitcoin Core ordinary live batch found a backbone conflict; forcing bounded repair"
                );
                let repair_succeeded = self.repair_near_tip(NearTipRepairSchedule::Force).await?;
                if !repair_succeeded {
                    return Ok(TickOutcome {
                        progressed: false,
                        idle_at_target: false,
                    });
                }
                match run_sync_bitcoin_core(self.client, self.source, self.config.clone()).await {
                    Ok(_stats) => {}
                    Err(err) if is_backbone_integrity_error(&err) => return Err(err),
                    Err(err) => tracing::warn!(
                        error = format!("{err:#}"),
                        "Bitcoin Core post-repair batch failed; retrying after interval"
                    ),
                }
            }
            Err(err) => tracing::warn!(
                error = format!("{err:#}"),
                "Bitcoin Core backbone live batch failed; retrying after interval"
            ),
        }

        let refreshed = handle_header_cache_refresh_result(
            refresh_bitcoin_core_header_cache(self.client, self.header_cache_classifier)
                .await
                .context("refresh Core header cache after Bitcoin backbone follow batch")
                .map(|_| ()),
        )?;
        if !refreshed {
            return Ok(self
                .bookkeeping_failure_outcome(Some(progress_before))
                .await);
        }
        let progress_after = match load_follow_progress(self.client, self.source_id).await {
            Ok(progress) => progress,
            Err(err) => {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "Bitcoin Core backbone live bookkeeping read failed after batch; retrying after interval"
                );
                return Ok(self
                    .bookkeeping_failure_outcome(Some(progress_before))
                    .await);
            }
        };

        Ok(TickOutcome {
            progressed: progress_after.contiguous_complete_height
                > progress_before.contiguous_complete_height,
            idle_at_target: progress_after.caught_up(),
        })
    }

    fn wait_after_tick(&mut self, result: Result<TickOutcome>) -> Result<Duration> {
        match result {
            Ok(outcome) => {
                wait_after_live_tick(outcome, &mut self.stall, self.config.follow_interval)
            }
            Err(err) => Err(err),
        }
    }

    fn log_starting(&self) {
        tracing::info!(
            source = BITCOIN_SOURCE_CODE,
            cch = self.initial_cch,
            limit = self.config.limit,
            follow_interval_secs = self.config.follow_interval.as_secs(),
            live_window_heights = self.config.near_tip_repair_window_heights,
            "starting Bitcoin Core backbone live producer"
        );
    }

    fn log_shutdown(&self) {
        tracing::info!("shutdown signal received; stopping Bitcoin Core backbone live producer");
    }
}

/// Continuous catch-up-then-follow-tip producer wrapping the one-shot
/// `run_sync_bitcoin_core` batch. Catches the contiguous-complete cursor up to
/// the Bitcoin Core tip, then follows the tip as new blocks arrive. Installs its
/// own SIGINT/SIGTERM handler so the live-test manager can stop it cleanly and
/// the public signature stays free of any shutdown type.
///
/// Error policy: a transient batch error (Core/DB fetch failure) is logged and
/// retried after `follow_interval`, but it counts toward the stall streak so a
/// PERSISTENT transient failure below tip fail-stops after
/// `FOLLOW_STALL_EXIT_THRESHOLD` consecutive batches rather than retrying
/// forever behind a healthy-looking status. A backbone integrity error
/// outside the bounded repair window is propagated immediately so the producer
/// exits and alerts the operator. A recent fork with a matching anchor inside
/// the window is switched atomically before the strict ordinary batch runs.
pub async fn run_sync_bitcoin_core_follow<S>(
    client: &mut Client,
    source: &S,
    header_cache_classifier: &ConfiguredParentClassifier,
    config: BitcoinCoreSyncConfig,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    let mut config = config;
    normalize_follow_config(&mut config);
    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let initial_cch = initialize_follow_state(client, source_id).await?;
    run_live_loop(BitcoinCoreLiveProducer {
        client,
        source,
        source_id,
        initial_cch,
        header_cache_classifier,
        config,
        stall: 0,
        last_near_tip_repair_at: None,
    })
    .await
}

/// Execute one finite follow tick with a deliberately fresh repair timestamp.
///
/// This db-integration seam exercises the control-flow race in which the
/// scheduled sweep is throttled but the ordinary batch discovers a fork and
/// must force an immediate bounded repair. The returned pair is
/// `(progressed, idle_at_target)`.
#[cfg(any(test, feature = "db-integration"))]
#[doc(hidden)]
pub async fn run_bitcoin_core_follow_tick_for_test<S>(
    client: &mut Client,
    source: &S,
    header_cache_classifier: &ConfiguredParentClassifier,
    mut config: BitcoinCoreSyncConfig,
) -> Result<(bool, bool)>
where
    S: BitcoinCoreBackboneSource,
{
    normalize_follow_config(&mut config);
    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let initial_cch = initialize_follow_state(client, source_id).await?;
    let outcome = BitcoinCoreLiveProducer {
        client,
        source,
        source_id,
        initial_cch,
        header_cache_classifier,
        config,
        stall: 0,
        last_near_tip_repair_at: Some(Instant::now()),
    }
    .tick()
    .await?;
    Ok((outcome.progressed, outcome.idle_at_target))
}

/// Ensure the `bitcoin_core_sync_state` row exists and return the initial
/// contiguous-complete height (cch). Public so the live producer can initialize
/// before its first cursor read AND an external integration test can exercise
/// the fresh-DB startup invariant directly.
pub async fn initialize_follow_state(client: &Client, source_id: i64) -> Result<i32> {
    let state = load_or_init_sync_state(client, source_id).await?;
    Ok(state.contiguous_complete_height)
}

/// Read the current follow-loop progress for the source. The loop calls this
/// before/after each batch to detect forward progress and distinguish healthy
/// idle-at-tip from a stuck-below-tip stall.
async fn load_follow_progress(client: &Client, source_id: i64) -> Result<FollowProgress> {
    let row = client
        .query_one(
            "SELECT contiguous_complete_height, target_tip_height FROM bitcoin_core_sync_state \
             WHERE source_id = $1 AND sync_mode = $2",
            &[&source_id, &SYNC_MODE_CONTIGUOUS],
        )
        .await
        .context("load Bitcoin Core follow progress")?;
    Ok(FollowProgress {
        contiguous_complete_height: row.get(0),
        target_tip_height: row.get(1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    // Integrity-error helpers live in the parent module (used by its guards);
    // the classification test exercises them here.
    use super::super::{BackboneIntegrityError, integrity_error};
    use anyhow::anyhow;

    #[test]
    fn follow_stall_step_exact_boundary_and_resets() {
        // Caught up at tip: healthy idle, reset regardless of progress.
        assert_eq!(follow_stall_step(true, false, 4), (0, false));
        // Forward progress below tip: reset, no exit.
        assert_eq!(follow_stall_step(false, true, 4), (0, false));
        // Stuck (no progress, not caught up): an Ok no-progress batch below tip
        // OR a transient Err (caller passes caught_up=false, progressed=false).
        // Increments, and exits EXACTLY at the threshold.
        assert_eq!(follow_stall_step(false, false, 0), (1, false));
        assert_eq!(
            follow_stall_step(false, false, FOLLOW_STALL_EXIT_THRESHOLD - 2),
            (FOLLOW_STALL_EXIT_THRESHOLD - 1, false),
            "one below threshold does not exit"
        );
        assert_eq!(
            follow_stall_step(false, false, FOLLOW_STALL_EXIT_THRESHOLD - 1),
            (FOLLOW_STALL_EXIT_THRESHOLD, true),
            "threshold exits"
        );
    }

    #[test]
    fn live_wait_policy_maps_progress_idle_and_stall() {
        let interval = Duration::from_secs(60);
        let mut stall = 3;
        let wait = wait_after_live_tick(
            TickOutcome {
                progressed: true,
                idle_at_target: false,
            },
            &mut stall,
            interval,
        )
        .expect("progress waits zero");
        assert_eq!(wait, Duration::ZERO);
        assert_eq!(stall, 0, "progress resets stall");

        let wait = wait_after_live_tick(
            TickOutcome {
                progressed: false,
                idle_at_target: true,
            },
            &mut stall,
            interval,
        )
        .expect("idle waits interval");
        assert_eq!(wait, interval);
        assert_eq!(stall, 0, "idle at target resets stall");

        let wait = wait_after_live_tick(
            TickOutcome {
                progressed: false,
                idle_at_target: false,
            },
            &mut stall,
            interval,
        )
        .expect("first below-target stall waits interval");
        assert_eq!(wait, interval);
        assert_eq!(stall, 1);

        stall = FOLLOW_STALL_EXIT_THRESHOLD - 1;
        let err = wait_after_live_tick(
            TickOutcome {
                progressed: false,
                idle_at_target: false,
            },
            &mut stall,
            interval,
        )
        .expect_err("threshold fail-stops");
        assert!(
            err.to_string().contains("stalled below tip"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn bookkeeping_failure_outcome_uses_best_effort_caught_up() {
        assert_eq!(
            bookkeeping_failure_outcome_from(None, None),
            TickOutcome {
                progressed: false,
                idle_at_target: false
            },
            "unknown progress counts below target"
        );
        assert_eq!(
            bookkeeping_failure_outcome_from(
                Some(FollowProgress {
                    contiguous_complete_height: 9,
                    target_tip_height: Some(10),
                }),
                Some(FollowProgress {
                    contiguous_complete_height: 9,
                    target_tip_height: Some(10),
                }),
            ),
            TickOutcome {
                progressed: false,
                idle_at_target: false
            },
            "known below-target progress contributes to stall"
        );
        assert_eq!(
            bookkeeping_failure_outcome_from(
                Some(FollowProgress {
                    contiguous_complete_height: 9,
                    target_tip_height: Some(10),
                }),
                Some(FollowProgress {
                    contiguous_complete_height: 10,
                    target_tip_height: Some(10),
                }),
            ),
            TickOutcome {
                progressed: true,
                idle_at_target: true
            },
            "a failed cache refresh retains progress made by its backbone batch"
        );
        assert_eq!(
            bookkeeping_failure_outcome_from(
                Some(FollowProgress {
                    contiguous_complete_height: 10,
                    target_tip_height: None,
                }),
                None,
            ),
            TickOutcome {
                progressed: false,
                idle_at_target: false
            },
            "missing target is not treated as idle"
        );
    }

    #[test]
    fn near_tip_repair_due_runs_initially_and_after_interval() {
        let interval = Duration::from_secs(60);
        assert!(
            near_tip_repair_due(None, interval),
            "no prior repair runs immediately"
        );
        assert!(
            !near_tip_repair_due(Some(Instant::now()), interval),
            "fresh repair is not due"
        );
        assert!(
            near_tip_repair_due(Some(Instant::now() - interval), interval),
            "repair at the interval boundary is due"
        );
        assert!(
            near_tip_repair_due(
                Some(Instant::now() - interval - Duration::from_millis(1)),
                interval
            ),
            "repair after the interval is due"
        );
    }

    #[test]
    fn near_tip_repair_result_policy_propagates_integrity_only() {
        let structural = integrity_error(
            BackboneIntegrityError::HeightConflict,
            "same-height conflict detail".to_owned(),
        );
        assert!(
            handle_near_tip_repair_result(953_621, Err(structural)).is_err(),
            "integrity errors fail-stop the live producer"
        );
        assert!(
            !handle_near_tip_repair_result(953_621, Err(anyhow!("temporary RPC outage")))
                .expect("transient repair errors are retryable"),
            "transient repair errors are logged and retried later"
        );
        assert!(
            handle_near_tip_repair_result(953_621, Ok(BitcoinCoreSyncStats::default()))
                .expect("successful repair stats are accepted"),
            "successful repair stats are accepted"
        );
    }

    #[test]
    fn header_cache_refresh_policy_retries_transient_errors() {
        assert!(
            !handle_header_cache_refresh_result(Err(anyhow!("temporary cache RPC outage")))
                .expect("transient cache errors are retryable"),
            "a cache refresh outage must not terminate follow mode"
        );
        let structural = integrity_error(
            BackboneIntegrityError::HeightConflict,
            "same-height conflict detail".to_owned(),
        );
        assert!(
            handle_header_cache_refresh_result(Err(structural)).is_err(),
            "integrity failures still fail-stop follow mode"
        );
        let cache_integrity = mmm_store::bitcoin_core_header_cache_integrity_error(
            "persisted Core header disagrees with the refreshed header",
        );
        assert!(
            handle_header_cache_refresh_result(Err(cache_integrity)).is_err(),
            "Core-header cache integrity failures fail-stop follow mode"
        );
        assert!(
            handle_header_cache_refresh_result(Ok(())).expect("success is accepted"),
            "successful cache refresh continues follow mode"
        );
    }

    #[test]
    fn near_tip_repair_result_policy_retries_coinbase_failures() {
        let stats = BitcoinCoreSyncStats {
            attempted: 1,
            completed: 1,
            skipped_complete: 0,
            coinbase_failed: 1,
        };
        assert!(
            !handle_near_tip_repair_result(953_621, Ok(stats))
                .expect("coinbase fetch failures remain retryable"),
            "coinbase fetch failures skip invariant verification until a later repair"
        );
    }

    #[test]
    fn integrity_error_classification() {
        let structural = integrity_error(
            BackboneIntegrityError::HeightConflict,
            "same-height conflict detail".to_owned(),
        );
        assert!(is_backbone_integrity_error(&structural), "marker downcasts");
        assert!(
            structural
                .to_string()
                .contains("same-height conflict detail"),
            "descriptive message stays on top"
        );
        let transient = anyhow!("Bitcoin Core tip fetch failed: connection refused");
        assert!(
            !is_backbone_integrity_error(&transient),
            "plain transient error is not structural"
        );
    }
}
