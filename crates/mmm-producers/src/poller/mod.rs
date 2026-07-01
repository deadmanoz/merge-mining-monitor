//! Live pollers for the AuxPoW capture paths.
//!
//! A single chain-agnostic driver ([`Poller`]) owns the cursor seeding, the
//! trailing rescan window, batch advance, startup read-model repair, and
//! SIGINT/SIGTERM shutdown. It is parameterized over the per-chain
//! [`ChainPoller`] trait, so Namecoin, RSK, and future merge-mined chains share
//! one loop instead of hand-copying it.
//!
//! ## Trailing rescan window
//!
//! `cursor` is the highest height fully processed. `effective_start =
//! max(start_height override, activation_floor)` is the lowest height the poller
//! will ever process. Each tick:
//!
//! 1. `tip = chain_tip()`.
//! 2. `rescan_start = max(effective_start, cursor + 1 - reorg_depth)`.
//! 3. `end = min(tip, cursor + batch_size)`.
//! 4. If `end < rescan_start`, the window is empty (tip has not advanced); do
//!    nothing.
//! 5. Process the window in ascending order, capped by `end` so heights above
//!    the node tip are never requested:
//!    - Replay sub-range `rescan_start..=min(cursor, end)` (already-processed
//!      heights re-scanned for reorgs): best-effort. A per-height failure is
//!      logged and skipped so an old replay error cannot starve the live tip.
//!    - New sub-range `(cursor + 1)..=end` (only when `end > cursor`): fail-fast
//!      with per-height advancement. `Advance` moves the cursor; `Hold` (the
//!      height could not be captured, e.g. an absent RSK canonical block) stops
//!      the new range so the height is retried next tick rather than skipped;
//!      an error propagates after the cursor reflects every completed height.
//!
//! ## Cursor seeding
//!
//! Live progress lives in the dedicated `poll_cursor` table, independent of
//! `merge_mining_event`, so bounded backfills never move the live cursor. On
//! startup the seed is chosen by precedence: an explicit `<PREFIX>_START_HEIGHT`
//! override (deliberate replay, seeds at `start - 1`); else a persisted
//! `poll_cursor` row (resume); else the chain tip (`chain_tip() - reorg_depth`,
//! the new default for a fresh source). A freshly chosen seed with no prior row
//! is persisted immediately so a restart before the first advancing tick resumes
//! from it instead of re-anchoring to a moved tip. All persists are monotonic
//! (`effective_seed_cursor` still clamps any seed up to `effective_start - 1`).

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tokio::time::sleep;
use tokio_postgres::Client;
use tracing::{debug, error, info, warn};

use crate::chains::spec::ChainSpec;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_read_model::{
    ReconcileReadModelConfig, is_reconcile_budget_exhausted, run_reconcile_read_model,
};
use mmm_store::{load_poll_cursor, upsert_poll_cursor_with_target};

use crate::live_loop::{LiveProducer, TickOutcome, run_live_loop};

#[cfg(test)]
pub(crate) use crate::live_loop::{PollLoopDecision, wait_for_next_tick_or_shutdown};

/// Whether the driver may advance the cursor past a freshly-processed new
/// height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightProgress {
    /// The height was captured; the cursor may move past it.
    Advance,
    /// The height could not be captured yet (e.g. an absent RSK canonical
    /// block) and must be retried on a later tick, not skipped.
    Hold,
    /// A cursor-blocking condition (the Hathor nBits-table horizon): the tick
    /// must STOP in both the replay and new sub-ranges rather than advance or be
    /// counted as processed, until an operator resolves it.
    Abort,
}

/// Per-chain default configuration values. Env vars override each field.
/// There is no default start height: an unset `<PREFIX>_START_HEIGHT` means the
/// poller seeds at the chain tip (see `Poller::new`), not an activation floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollerDefaults {
    /// Seconds between ticks when `<PREFIX>_POLL_INTERVAL_SECONDS` is unset.
    pub poll_interval_seconds: u64,
    /// Max new heights advanced per tick (caps `end` at `cursor + batch_size`).
    pub batch_size: i32,
    /// Trailing heights re-scanned for reorgs each tick; sets how deep a child
    /// reorg the live loop self-heals before a manual backfill is needed.
    pub reorg_depth: i32,
}

/// Live poller configuration shared by every chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollerConfig {
    /// Explicit `<PREFIX>_START_HEIGHT` override, or `None` to seed at the tip
    /// (fresh) or resume from the persisted `poll_cursor`. `Some(h)` is for
    /// deliberate replays/tests and seeds at `h - 1`.
    pub start_height_override: Option<i32>,
    /// Sleep between ticks; validated `> 0` so the loop always yields.
    pub poll_interval: Duration,
    /// Max new heights advanced per tick; validated `> 0`.
    pub batch_size: i32,
    /// Trailing reorg-rescan depth; validated `>= 0` (0 disables the rescan).
    pub reorg_depth: i32,
}

impl PollerConfig {
    /// Pure config construction driven by an arbitrary lookup. The
    /// `chains::config` drives this with `env::var`; unit tests drive it
    /// with an in-memory map so they never mutate the global (and in Rust 2024
    /// `unsafe`) process environment.
    pub fn from_lookup<F>(prefix: &str, defaults: PollerDefaults, lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let start_height_override = parse_lookup_opt(&lookup, &format!("{prefix}_START_HEIGHT"))?;
        let poll_interval_seconds = parse_lookup_or(
            &lookup,
            &format!("{prefix}_POLL_INTERVAL_SECONDS"),
            defaults.poll_interval_seconds,
        )?;
        let batch_size = parse_lookup_or(
            &lookup,
            &format!("{prefix}_BATCH_SIZE"),
            defaults.batch_size,
        )?;
        let reorg_depth = parse_lookup_or(
            &lookup,
            &format!("{prefix}_REORG_DEPTH"),
            defaults.reorg_depth,
        )?;

        if let Some(start_height) = start_height_override {
            ensure!(
                start_height >= 0,
                "{prefix}_START_HEIGHT must be non-negative"
            );
        }
        ensure!(
            poll_interval_seconds > 0,
            "{prefix}_POLL_INTERVAL_SECONDS must be positive"
        );
        ensure!(batch_size > 0, "{prefix}_BATCH_SIZE must be positive");
        ensure!(
            reorg_depth >= 0,
            "{prefix}_REORG_DEPTH must be non-negative"
        );

        Ok(Self {
            start_height_override,
            poll_interval: Duration::from_secs(poll_interval_seconds),
            batch_size,
            reorg_depth,
        })
    }
}

/// Per-chain behavior the shared driver needs. The driver owns the loop; the
/// trait supplies tip lookup, per-height capture, and the source/client handles
/// used for cursor seeding and startup repair.
/// Shared identity and DB state every live chain poller exposes to the driver.
pub struct ChainPollerState {
    pub(crate) spec: &'static ChainSpec,
    pub(crate) source_id: i64,
    pub(crate) client: Client,
}

impl ChainPollerState {
    pub fn new(spec: &'static ChainSpec, source_id: i64, client: Client) -> Self {
        Self {
            spec,
            source_id,
            client,
        }
    }

    #[cfg(any(test, feature = "db-integration"))]
    pub fn client_mut(&mut self) -> &mut Client {
        &mut self.client
    }
}

// `async fn` in this trait yields non-`Send` futures, which is fine: the poller
// future is awaited directly under `#[tokio::main]` (`block_on`), never
// `tokio::spawn`ed, so no `Send` bound is required.
#[allow(async_fn_in_trait)]
pub trait ChainPoller {
    fn poller_state(&self) -> &ChainPollerState;

    /// The chain's spec-table row: the single source of identity for the
    /// driver (name, source code, activation floor derive from it).
    fn spec(&self) -> &'static ChainSpec {
        self.poller_state().spec
    }
    /// Human-readable chain name for log lines.
    fn name(&self) -> &'static str {
        self.spec().display_name
    }
    /// `source.code` for this chain, used to scope startup read-model repair.
    fn source_code(&self) -> &'static str {
        self.spec().source_code
    }
    /// `source.id` for this chain, used to seed the poll cursor.
    fn source_id(&self) -> i64 {
        self.poller_state().source_id
    }
    /// Lowest height that can carry capturable evidence (0 when the chain has
    /// merge-mined from genesis).
    fn activation_floor(&self) -> i32 {
        self.spec().activation_floor
    }
    /// Shared DB handle for cursor reads/writes (`load_poll_cursor`,
    /// `upsert_poll_cursor_with_target`).
    fn client(&self) -> &Client {
        &self.poller_state().client
    }
    /// Mutable DB handle for the startup read-model repair, which needs `&mut`.
    fn client_mut(&mut self) -> &mut Client;
    /// Current chain tip height.
    async fn chain_tip(&self) -> Result<i32>;
    /// Capture a single height. Returns whether the cursor may advance past it.
    async fn process_height(&mut self, height: i32) -> Result<HeightProgress>;
    /// Drain durable per-source pending work (best-effort, each tick). Default
    /// no-op; only Hathor implements it (the `poll_pending_reconcile` queue).
    async fn drain_pending(&mut self) -> Result<()> {
        Ok(())
    }
}

/// The chain-agnostic live poller driver.
pub struct Poller<C: ChainPoller> {
    chain: C,
    /// Highest height fully processed; mirrors the persisted `poll_cursor` row.
    /// Advanced only after a height captures (`Advance`), and persisted (with
    /// the current tip as target) after each tick that moved it.
    cursor: i32,
    config: PollerConfig,
}

impl<C: ChainPoller> Poller<C> {
    /// Run startup read-model repair, seed the cursor, and build the poller.
    ///
    /// Seed precedence: (1) an explicit `<PREFIX>_START_HEIGHT` override seeds at
    /// `start - 1` for deliberate replays; (2) a persisted `poll_cursor` row
    /// resumes live progress; (3) a fresh source (no override, no row) seeds at
    /// `chain_tip() - reorg_depth`. A seed chosen on path (1) or (3) with no
    /// prior row is persisted immediately so a restart before the first advancing
    /// tick resumes from it rather than re-anchoring to a moved tip.
    pub async fn new(mut chain: C, config: PollerConfig) -> Result<Self> {
        run_startup_repair(&mut chain).await?;

        let effective_start =
            effective_start(config.start_height_override, chain.activation_floor());
        let source_id = chain.source_id();

        let (cursor, persist_seed, seed_target_height) = match config.start_height_override {
            Some(start) => {
                let had_row = load_poll_cursor(chain.client(), source_id)
                    .await
                    .with_context(|| format!("load {} poll cursor", chain.name()))?
                    .is_some();
                (
                    effective_seed_cursor(start - 1, effective_start),
                    !had_row,
                    None,
                )
            }
            None => match load_poll_cursor(chain.client(), source_id)
                .await
                .with_context(|| format!("load {} poll cursor", chain.name()))?
            {
                Some(persisted) => (
                    effective_seed_cursor(persisted, effective_start),
                    false,
                    None,
                ),
                None => {
                    let tip = fetch_tip_with_retry(&chain).await?;
                    (
                        effective_seed_cursor(tip - config.reorg_depth, effective_start),
                        true,
                        Some(tip),
                    )
                }
            },
        };

        if persist_seed {
            upsert_poll_cursor_with_target(chain.client(), source_id, cursor, seed_target_height)
                .await
                .with_context(|| format!("persist seed cursor for {}", chain.name()))?;
        }

        Ok(Self {
            chain,
            cursor,
            config,
        })
    }

    /// The live loop: tick, then wait `poll_interval` or a shutdown signal.
    /// A failing tick is logged and the loop continues (the next tick retries
    /// from the persisted cursor); only a SIGINT/SIGTERM returns `Ok(())`.
    pub async fn run_forever(self) -> Result<()> {
        run_live_loop(self).await
    }

    /// One tick: drain pending work, fetch the tip, compute the trailing-rescan
    /// window, and run the per-height policy. The cursor is persisted with the
    /// fetched tip as `target_height` so progress lag is observable even on an
    /// empty tick; if the policy advances the cursor mid-window before erroring,
    /// the partial progress is persisted before the error propagates so completed
    /// heights are not reprocessed. Returns the count of heights processed.
    pub async fn poll_tick(&mut self) -> Result<usize> {
        self.drain_pending_best_effort().await;
        let tip = self.fetch_tip_and_persist_target().await?;
        let Some(window) = self.tick_window(tip) else {
            return Ok(0);
        };
        self.run_window_and_persist_progress(window, tip).await
    }

    fn log_starting(&self) {
        info!(
            chain = self.chain.name(),
            cursor = self.cursor,
            batch_size = self.config.batch_size,
            reorg_depth = self.config.reorg_depth,
            poll_interval_seconds = self.config.poll_interval.as_secs(),
            "starting AuxPoW poller"
        );
    }

    fn log_poll_tick_result(&self, result: &Result<TickOutcome>) {
        match result {
            Ok(outcome) => {
                debug!(
                    chain = self.chain.name(),
                    progressed = outcome.progressed,
                    cursor = self.cursor,
                    "poll tick complete"
                );
            }
            Err(err) => {
                error!(
                    chain = self.chain.name(),
                    error = %err,
                    cursor = self.cursor,
                    "poll tick failed"
                );
            }
        }
    }

    fn log_shutdown(&self) {
        info!(
            chain = self.chain.name(),
            cursor = self.cursor,
            "shutdown signal received; stopping AuxPoW poller"
        );
    }

    async fn drain_pending_best_effort(&mut self) {
        if let Err(err) = self.chain.drain_pending().await {
            warn!(
                chain = self.chain.name(),
                error = %err,
                "draining pending work failed; continuing tick"
            );
        }
    }

    async fn fetch_tip_and_persist_target(&self) -> Result<i32> {
        let tip = fetch_tip_with_retry(&self.chain)
            .await
            .with_context(|| format!("get {} tip", self.chain.name()))?;

        self.persist_poll_cursor_target_best_effort(tip).await;
        Ok(tip)
    }

    async fn persist_poll_cursor_target_best_effort(&self, tip: i32) {
        if let Err(persist_err) = upsert_poll_cursor_with_target(
            self.chain.client(),
            self.chain.source_id(),
            self.cursor,
            Some(tip),
        )
        .await
        {
            warn!(
                chain = self.chain.name(),
                cursor = self.cursor,
                target_height = tip,
                error = %persist_err,
                "failed to persist poll target; continuing tick"
            );
        }
    }

    fn tick_window(&self, tip: i32) -> Option<TickWindow> {
        let effective_start = effective_start(
            self.config.start_height_override,
            self.chain.activation_floor(),
        );
        compute_tick_window(
            effective_start,
            self.cursor,
            tip,
            self.config.reorg_depth,
            self.config.batch_size,
        )
    }

    async fn run_window_and_persist_progress(
        &mut self,
        window: TickWindow,
        tip: i32,
    ) -> Result<usize> {
        // `cursor` is a local so its `&mut` borrow does not alias the
        // `&mut self.chain` captured by the processor closure.
        let mut cursor = self.cursor;
        let result = run_tick_policy(&mut cursor, window, async |height| {
            self.chain.process_height(height).await
        })
        .await;

        // `run_tick_policy` advances `cursor` height-by-height and can still
        // return an error on a later new height, so persist any progress before
        // propagating; otherwise the completed heights replay next tick/restart.
        self.persist_progress_if_advanced(cursor, tip).await;

        result
    }

    async fn persist_progress_if_advanced(&mut self, new_cursor: i32, tip: i32) {
        if new_cursor <= self.cursor {
            return;
        }
        self.cursor = new_cursor;
        if let Err(persist_err) = upsert_poll_cursor_with_target(
            self.chain.client(),
            self.chain.source_id(),
            self.cursor,
            Some(tip),
        )
        .await
        {
            warn!(
                chain = self.chain.name(),
                cursor = self.cursor,
                error = %persist_err,
                "failed to persist poll cursor; keeping in-memory cursor"
            );
        }
    }
}

impl<C: ChainPoller> LiveProducer for Poller<C> {
    fn name(&self) -> &'static str {
        self.chain.name()
    }

    async fn tick(&mut self) -> Result<TickOutcome> {
        let processed = self.poll_tick().await?;
        Ok(TickOutcome {
            progressed: processed > 0,
            idle_at_target: false,
        })
    }

    fn wait_after_tick(&mut self, _result: Result<TickOutcome>) -> Result<Duration> {
        Ok(self.config.poll_interval)
    }

    fn log_starting(&self) {
        Poller::log_starting(self);
    }

    fn log_tick_result(&self, result: &Result<TickOutcome>) {
        self.log_poll_tick_result(result);
    }

    fn log_shutdown(&self) {
        Poller::log_shutdown(self);
    }
}

/// Run startup read-model repair: missing-only, classifier disabled, scoped to
/// the chain's own source for the event-candidate work (orphan `block` cleanup
/// remains global). Budget exhaustion is logged and tolerated so historical
/// rows never block poller startup.
async fn run_startup_repair<C: ChainPoller>(chain: &mut C) -> Result<()> {
    let name = chain.name();
    let config = ReconcileReadModelConfig {
        source_code: Some(chain.source_code().to_owned()),
        ..ReconcileReadModelConfig::default()
    };
    match run_reconcile_read_model(
        chain.client_mut(),
        &ConfiguredParentClassifier::Disabled,
        config,
    )
    .await
    {
        Ok(repaired) => {
            info!(
                chain = name,
                repaired, "repaired read model before seeding poll cursor"
            );
            Ok(())
        }
        Err(err) if is_reconcile_budget_exhausted(&err) => {
            warn!(
                chain = name,
                error = %err,
                "read-model startup repair budget exhausted; continuing poller startup"
            );
            Ok(())
        }
        Err(err) => Err(err).context("repair read model before seeding poll cursor"),
    }
}

/// Clamp the database-seeded cursor up to `effective_start - 1` so the poller
/// never parks below the configured start. This closes both stall modes: an
/// empty source seeded below the activation floor, and a persisted
/// `MAX(child_height)` below a newly-raised start height. In the common case
/// (persisted max above the start) it is a no-op.
fn effective_seed_cursor(seeded: i32, effective_start: i32) -> i32 {
    seeded.max(effective_start - 1)
}

/// The hard floor for cursor seeding and windowing: the explicit start override
/// (if any) lifted to the activation floor, else the activation floor itself.
/// With no override, the tip-anchored seed sits far above this, so the floor is
/// only material on the explicit-override path.
fn effective_start(start_height_override: Option<i32>, activation_floor: i32) -> i32 {
    start_height_override
        .unwrap_or(activation_floor)
        .max(activation_floor)
}

/// Bounded retry around a chain tip fetch, used by both the fresh-seed startup
/// path ([`Poller::new`]) and the steady-state [`Poller::poll_tick`]. The RPC
/// tunnels show intermittent tip-fetch failures (a stale pooled socket, a
/// transient connect/timeout error), so retry a few times with linear backoff
/// before giving up rather than failing the call on a single transient error.
async fn fetch_tip_with_retry<C: ChainPoller>(chain: &C) -> Result<i32> {
    fetch_tip_with_retry_inner(chain, |attempt| async move {
        sleep(Duration::from_secs(u64::from(attempt))).await;
    })
    .await
}

/// Retry core for [`fetch_tip_with_retry`], parameterized over the backoff
/// `sleeper` so tests inject a no-op sleeper and exercise the retry behavior
/// without real-time delays (production passes the `tokio::time::sleep`-based
/// sleeper). `sleeper(attempt)` is awaited between failed attempts; it is not
/// called after the final attempt.
async fn fetch_tip_with_retry_inner<C, S, Fut>(chain: &C, mut sleeper: S) -> Result<i32>
where
    C: ChainPoller,
    S: FnMut(u32) -> Fut,
    Fut: Future<Output = ()>,
{
    const ATTEMPTS: u32 = 5;
    let mut last_err = None;
    for attempt in 1..=ATTEMPTS {
        match chain.chain_tip().await {
            Ok(tip) => return Ok(tip),
            Err(err) => {
                warn!(
                    chain = chain.name(),
                    attempt,
                    max_attempts = ATTEMPTS,
                    error = %err,
                    "tip fetch failed; retrying"
                );
                last_err = Some(err);
                if attempt < ATTEMPTS {
                    sleeper(attempt).await;
                }
            }
        }
    }
    Err(last_err.expect("loop runs at least once")).with_context(|| {
        format!(
            "{} chain tip unavailable after {ATTEMPTS} attempts",
            chain.name()
        )
    })
}

/// The inclusive height range to process this tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TickWindow {
    /// Lowest height to touch this tick: the trailing reorg-rescan floor,
    /// clamped up to `effective_start`.
    rescan_start: i32,
    /// Highest height to touch this tick: `min(tip, cursor + batch_size)`, so
    /// it never exceeds the node tip nor the per-tick batch cap.
    end: i32,
}

/// Compute the trailing-rescan window for a tick. Returns `None` when there is
/// nothing to process (`end < rescan_start`). Saturating arithmetic guards the
/// (practically unreachable) i32 extremes.
fn compute_tick_window(
    effective_start: i32,
    cursor: i32,
    tip: i32,
    reorg_depth: i32,
    batch_size: i32,
) -> Option<TickWindow> {
    let rescan_start = effective_start.max(cursor.saturating_add(1).saturating_sub(reorg_depth));
    let end = tip.min(cursor.saturating_add(batch_size));
    if end < rescan_start {
        None
    } else {
        Some(TickWindow { rescan_start, end })
    }
}

/// Apply the per-tick failure policy over `window`, advancing `cursor`. The
/// processor is a closure so this is unit-testable without a live `Client`.
///
/// Replay sub-range (`<= cursor`) is best-effort (failures logged and skipped);
/// the new sub-range (`> cursor`) is fail-fast and only advances on `Advance`.
async fn run_tick_policy<F>(cursor: &mut i32, window: TickWindow, mut process: F) -> Result<usize>
where
    F: AsyncFnMut(i32) -> Result<HeightProgress>,
{
    let mut processed = 0usize;

    // Replay sub-range: already-processed heights, re-scanned for reorgs.
    // Capped at `end` so a cursor ahead of the node tip never requests heights
    // above the tip.
    let replay_end = (*cursor).min(window.end);
    for height in window.rescan_start..=replay_end {
        match process(height).await {
            Ok(HeightProgress::Abort) => {
                bail!("cursor-blocking hold at replay height {height}; aborting tick")
            }
            Ok(_) => processed += 1,
            Err(err) => warn!(
                height,
                error = %err,
                "rescan of already-processed height failed; continuing (best-effort)"
            ),
        }
    }

    // New sub-range: the ordered chain tail.
    if window.end > *cursor {
        for height in (*cursor + 1)..=window.end {
            match process(height).await {
                Ok(HeightProgress::Advance) => {
                    *cursor = (*cursor).max(height);
                    processed += 1;
                }
                Ok(HeightProgress::Hold) => {
                    debug!(
                        height,
                        "new height not yet captured (hold); retry next tick"
                    );
                    break;
                }
                Ok(HeightProgress::Abort) => {
                    bail!("cursor-blocking hold at new height {height}; aborting tick")
                }
                Err(err) => return Err(err),
            }
        }
    }

    Ok(processed)
}

/// Parse a `FromStr` env value, returning `default` when the key is unset. A
/// present-but-unparseable value is a hard error carrying the offending key and
/// value, so a typo in deployment config fails loudly instead of silently
/// falling back to the default.
fn parse_lookup_or<T, F>(lookup: &F, key: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        Some(value) => value
            .parse()
            .with_context(|| format!("{key} has invalid value {value:?}")),
        None => Ok(default),
    }
}

/// Parse an optional `i32` env value. `None` when the key is unset (which, for
/// `<PREFIX>_START_HEIGHT`, selects the tip-anchored / resume seed paths).
fn parse_lookup_opt<F>(lookup: &F, key: &str) -> Result<Option<i32>>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup(key) {
        Some(value) => {
            Ok(Some(value.parse().with_context(|| {
                format!("{key} has invalid value {value:?}")
            })?))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
