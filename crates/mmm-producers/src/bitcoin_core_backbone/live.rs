//! Bitcoin Core backbone live producer.
//!
//! The continuous catch-up-then-follow-tip managed-service mode wrapping the
//! one-shot `super::run_sync_bitcoin_core` batch, plus its cursor/decision
//! helpers. Split out of the parent module to keep each file under the
//! arch-lint file-size budget.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio_postgres::Client;

use crate::live_loop::{LiveProducer, TickOutcome, run_live_loop};

use super::{
    BitcoinCoreBackboneSource, BitcoinCoreBackboneTip, BitcoinCoreSyncConfig, BitcoinCoreSyncStats,
    SYNC_MODE_CONTIGUOUS, is_backbone_integrity_error, load_or_init_sync_state,
    repair_near_tip_gaps_to_target, run_sync_bitcoin_core, verify_live_backbone_window,
};
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
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
/// (possibly updated) last-repair instant for the loop to thread through.
/// Returns the prior instant unchanged when not yet due. On a target-capture
/// failure it logs and stamps "now" so the next attempt still respects the
/// interval. A successful repair (no coinbase failures) is followed by a window
/// invariant verification; an integrity error there propagates to fail-stop.
async fn repair_near_tip_gaps_if_due<S>(
    client: &mut Client,
    source: &S,
    last_repair: Option<Instant>,
    interval: Duration,
    delay: Duration,
    window_heights: i32,
) -> Result<Option<Instant>>
where
    S: BitcoinCoreBackboneSource,
{
    if !near_tip_repair_due(last_repair, interval) {
        return Ok(last_repair);
    }

    let target = match capture_live_window_target(source).await {
        Ok(target) => target,
        Err(err) => {
            tracing::warn!(
                error = format!("{err:#}"),
                "Bitcoin Core near-tip repair target capture failed; retrying after interval"
            );
            return Ok(Some(Instant::now()));
        }
    };
    let tip_height = target.height;
    let repair_succeeded = handle_near_tip_repair_result(
        tip_height,
        repair_near_tip_gaps_to_target(client, source, target, delay, window_heights).await,
    )?;
    if repair_succeeded {
        verify_live_backbone_window(client, source, target, window_heights).await?;
    }
    Ok(Some(Instant::now()))
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
    config: BitcoinCoreSyncConfig,
    stall: usize,
    last_near_tip_repair_at: Option<Instant>,
}

impl<S> BitcoinCoreLiveProducer<'_, S>
where
    S: BitcoinCoreBackboneSource,
{
    async fn bookkeeping_failure_outcome(&self) -> TickOutcome {
        let best_effort_progress = load_follow_progress(self.client, self.source_id).await.ok();
        bookkeeping_failure_outcome_from(best_effort_progress)
    }
}

fn bookkeeping_failure_outcome_from(best_effort_progress: Option<FollowProgress>) -> TickOutcome {
    let idle_at_target = best_effort_progress
        .map(FollowProgress::caught_up)
        .unwrap_or(false);
    TickOutcome {
        progressed: false,
        idle_at_target,
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
        let progress_before = match load_follow_progress(self.client, self.source_id).await {
            Ok(progress) => progress,
            Err(err) => {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "Bitcoin Core backbone live bookkeeping read failed before batch; retrying after interval"
                );
                return Ok(self.bookkeeping_failure_outcome().await);
            }
        };

        match run_sync_bitcoin_core(self.client, self.source, self.config.clone()).await {
            Ok(_stats) => {}
            Err(err) if is_backbone_integrity_error(&err) => return Err(err),
            Err(err) => tracing::warn!(
                error = format!("{err:#}"),
                "Bitcoin Core backbone live batch failed; retrying after interval"
            ),
        }

        self.last_near_tip_repair_at = repair_near_tip_gaps_if_due(
            self.client,
            self.source,
            self.last_near_tip_repair_at,
            self.config.follow_interval,
            self.config.delay,
            self.config.near_tip_repair_window_heights,
        )
        .await?;

        let progress_after = match load_follow_progress(self.client, self.source_id).await {
            Ok(progress) => progress,
            Err(err) => {
                tracing::warn!(
                    error = format!("{err:#}"),
                    "Bitcoin Core backbone live bookkeeping read failed after batch; retrying after interval"
                );
                return Ok(self.bookkeeping_failure_outcome().await);
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
/// (`BackboneIntegrityError`: height conflict, link mismatch, or a same-height
/// tip reorg) is propagated immediately so the producer exits and the operator is
/// alerted rather than the service appearing healthy while permanently stuck.
pub async fn run_sync_bitcoin_core_follow<S>(
    client: &mut Client,
    source: &S,
    mut config: BitcoinCoreSyncConfig,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    normalize_follow_config(&mut config);
    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let initial_cch = initialize_follow_state(client, source_id).await?;
    run_live_loop(BitcoinCoreLiveProducer {
        client,
        source,
        source_id,
        initial_cch,
        config,
        stall: 0,
        last_near_tip_repair_at: None,
    })
    .await
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
            bookkeeping_failure_outcome_from(None),
            TickOutcome {
                progressed: false,
                idle_at_target: false
            },
            "unknown progress counts below target"
        );
        assert_eq!(
            bookkeeping_failure_outcome_from(Some(FollowProgress {
                contiguous_complete_height: 9,
                target_tip_height: Some(10),
            })),
            TickOutcome {
                progressed: false,
                idle_at_target: false
            },
            "known below-target progress contributes to stall"
        );
        assert_eq!(
            bookkeeping_failure_outcome_from(Some(FollowProgress {
                contiguous_complete_height: 10,
                target_tip_height: Some(10),
            })),
            TickOutcome {
                progressed: false,
                idle_at_target: true
            },
            "known caught-up progress idles instead of stalling"
        );
        assert_eq!(
            bookkeeping_failure_outcome_from(Some(FollowProgress {
                contiguous_complete_height: 10,
                target_tip_height: None,
            })),
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
