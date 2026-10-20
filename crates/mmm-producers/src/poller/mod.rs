//! Chain-agnostic live poller driver ([`Poller`]) parameterized over [`ChainPoller`].
//!
//! Startup seeds `poll_cursor` by precedence: explicit `<PREFIX>_START_HEIGHT`
//! (`start - 1`), persisted `poll_cursor`, or `chain_tip() - reorg_depth`, all
//! clamped to `max(start_override, activation_floor) - 1` and persisted before
//! the first tick when no row exists. Each tick processes
//! `max(effective_start, cursor + 1 - reorg_depth)..=min(tip, cursor + batch_size)`:
//! replay heights (`<= cursor`) are best-effort, while new heights (`> cursor`)
//! advance monotonically on `Advance`, stop on `Hold`, and persist partial cursor
//! progress before propagating errors.

use std::future::Future;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use tokio::time::sleep;
use tokio_postgres::Client;
use tracing::{debug, error, info, warn};

use crate::chains::spec::ChainSpec;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_read_model::{
    ReconcileReadModelConfig, is_reconcile_budget_exhausted, run_reconcile_read_model,
};
use mmm_rpc::{RpcMetrics, RpcMetricsSnapshot};
use mmm_store::{load_poll_cursor, prune_child_chain_heads_below, upsert_poll_cursor_with_target};

use crate::live_loop::{LiveProducer, TickOutcome, run_live_loop};

/// Most `child_chain_head` rows one tick prunes. Each pruned row holds an advisory lock until
/// the statement ends, so concurrent pollers stay well inside Postgres's shared lock table.
const CHAIN_HEAD_PRUNE_BATCH: i64 = 250;

#[cfg(test)]
pub(crate) use crate::live_loop::{PollLoopDecision, wait_for_next_tick_or_shutdown};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightProgress {
    Advance,
    /// Height not yet capturable; stop the new sub-range and retry next tick.
    Hold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollerDefaults {
    pub poll_interval_seconds: u64,
    pub batch_size: i32,
    pub reorg_depth: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PollerConfig {
    pub start_height_override: Option<i32>,
    pub poll_interval: Duration,
    pub batch_size: i32,
    pub reorg_depth: i32,
}

impl PollerConfig {
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

// Poller futures are awaited under `#[tokio::main]` (`block_on`), never `tokio::spawn`ed.
#[allow(async_fn_in_trait)]
pub trait ChainPoller {
    fn poller_state(&self) -> &ChainPollerState;

    fn spec(&self) -> &'static ChainSpec {
        self.poller_state().spec
    }
    fn name(&self) -> &'static str {
        self.spec().display_name
    }
    fn source_code(&self) -> &'static str {
        self.spec().source_code
    }
    fn source_id(&self) -> i64 {
        self.poller_state().source_id
    }
    fn activation_floor(&self) -> i32 {
        self.spec().activation_floor
    }
    fn client(&self) -> &Client {
        &self.poller_state().client
    }
    fn client_mut(&mut self) -> &mut Client;
    async fn chain_tip(&self) -> Result<i32>;
    async fn process_height(&mut self, height: i32) -> Result<HeightProgress>;
    async fn rescan_height(&mut self, height: i32) -> Result<HeightProgress> {
        self.process_height(height).await
    }
    async fn refresh_core_cache(&mut self) -> Result<()> {
        Ok(())
    }
    /// Drain durable per-source pending work (best-effort, each tick).
    async fn drain_pending(&mut self) -> Result<()> {
        Ok(())
    }
    fn chain_rpc_metrics(&self) -> Option<RpcMetrics> {
        None
    }
    fn parent_classifier(&self) -> Option<&ConfiguredParentClassifier> {
        None
    }
    fn non_bitcoin_parents(&self) -> usize {
        0
    }
    fn rpc_metrics(&self) -> Vec<RpcMetrics> {
        let mut metrics = Vec::from_iter(self.chain_rpc_metrics());
        metrics.extend(self.parent_classifier().and_then(|c| c.metrics()));
        metrics
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TickSummary {
    tip: Option<i32>,
    processed: usize,
    rescanned: usize,
    new: i32,
    held: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct TickProgress {
    processed: usize,
    held: bool,
}

pub struct Poller<C: ChainPoller> {
    chain: C,
    cursor: i32,
    config: PollerConfig,
}

impl<C: ChainPoller> Poller<C> {
    /// Run startup read-model repair, seed `poll_cursor`, and build the poller.
    pub async fn new(mut chain: C, config: PollerConfig) -> Result<Self> {
        run_startup_repair(&mut chain).await?;

        crate::producer_runtime::warn_if_empty_known_stale_membership(chain.client(), chain.name())
            .await?;

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

    pub async fn run_forever(self) -> Result<()> {
        run_live_loop(self).await
    }

    /// Execute one poll tick and log a `poll tick` summary with per-client RPC deltas.
    pub async fn poll_tick(&mut self) -> Result<usize> {
        let tick_start = Instant::now();
        let metrics = self.chain.rpc_metrics();
        let before = metrics.iter().map(RpcMetrics::snapshot).collect::<Vec<_>>();
        let non_bitcoin_before = self.chain.non_bitcoin_parents();
        let mut summary = TickSummary::default();
        let result = self.poll_tick_steps(&mut summary).await;
        info!(
            chain = self.chain.name(),
            cursor = self.cursor,
            tip = summary.tip,
            rescanned = summary.rescanned,
            new = summary.new,
            held = summary.held,
            non_bitcoin = self.chain.non_bitcoin_parents() - non_bitcoin_before,
            failed = result.is_err(),
            tick_ms = tick_start.elapsed().as_millis(),
            rpc = rpc_deltas(&metrics, &before),
            "poll tick"
        );
        result.map(|()| summary.processed)
    }

    async fn poll_tick_steps(&mut self, summary: &mut TickSummary) -> Result<()> {
        self.chain.refresh_core_cache().await?;
        self.drain_pending_best_effort().await;
        let tip = self.fetch_tip_and_persist_target().await?;
        summary.tip = Some(tip);
        let result = match self.tick_window(tip) {
            Some(window) => {
                self.run_window_and_persist_progress(window, tip, summary)
                    .await
            }
            None => Ok(()),
        };
        self.prune_chain_heads_best_effort().await;
        result
    }

    /// Drop `child_chain_head` rows the rescan window no longer reaches, a bounded batch per
    /// tick so a long backlog drains over several ticks. While a start override is set, heads
    /// from the override up stay: they prove the live replay reached the activation tip.
    async fn prune_chain_heads_best_effort(&self) {
        let rescan_floor = rescan_floor(self.cursor, self.config.reorg_depth);
        let floor = self
            .config
            .start_height_override
            .map_or(rescan_floor, |start| rescan_floor.min(start));
        if let Err(err) = prune_child_chain_heads_below(
            self.chain.client(),
            self.chain.source_id(),
            floor,
            CHAIN_HEAD_PRUNE_BATCH,
        )
        .await
        {
            warn!(
                chain = self.chain.name(),
                floor,
                error = %err,
                "failed to prune child_chain_head; continuing"
            );
        }
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
        summary: &mut TickSummary,
    ) -> Result<()> {
        let old_cursor = self.cursor;
        let mut cursor = self.cursor;
        let mut progress = TickProgress::default();
        let result =
            run_tick_policy_counting(&mut cursor, &mut progress, window, async |height, kind| {
                match kind {
                    TickHeight::Rescan => self.chain.rescan_height(height).await,
                    TickHeight::New => self.chain.process_height(height).await,
                }
            })
            .await;

        // Persist partial cursor advancement before propagating a mid-window error.
        self.persist_progress_if_advanced(cursor, tip).await;

        let new = cursor - old_cursor;
        summary.processed = progress.processed;
        summary.rescanned = progress
            .processed
            .saturating_sub(usize::try_from(new).unwrap_or(0));
        summary.new = new;
        summary.held = progress.held;
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

fn rpc_deltas(metrics: &[RpcMetrics], before: &[RpcMetricsSnapshot]) -> String {
    metrics
        .iter()
        .zip(before)
        .map(|(metrics, before)| metrics.snapshot().since(before).to_string())
        .collect::<Vec<_>>()
        .join("; ")
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

/// Missing-only startup read-model repair (classifier disabled, scoped to this
/// source); budget exhaustion is tolerated so historical rows do not block startup.
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

fn effective_seed_cursor(seeded: i32, effective_start: i32) -> i32 {
    seeded.max(effective_start - 1)
}

fn effective_start(start_height_override: Option<i32>, activation_floor: i32) -> i32 {
    start_height_override
        .unwrap_or(activation_floor)
        .max(activation_floor)
}

async fn fetch_tip_with_retry<C: ChainPoller>(chain: &C) -> Result<i32> {
    fetch_tip_with_retry_inner(chain, |attempt| async move {
        sleep(Duration::from_secs(u64::from(attempt))).await;
    })
    .await
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TickWindow {
    rescan_start: i32,
    end: i32,
}

/// The lowest height a tick's trailing rescan reaches from `cursor`.
fn rescan_floor(cursor: i32, reorg_depth: i32) -> i32 {
    cursor.saturating_add(1).saturating_sub(reorg_depth)
}

fn compute_tick_window(
    effective_start: i32,
    cursor: i32,
    tip: i32,
    reorg_depth: i32,
    batch_size: i32,
) -> Option<TickWindow> {
    let rescan_start = effective_start.max(rescan_floor(cursor, reorg_depth));
    let end = tip.min(cursor.saturating_add(batch_size));
    if end < rescan_start {
        None
    } else {
        Some(TickWindow { rescan_start, end })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RescanOutcome<T> {
    Unchanged,
    Captured(T),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickHeight {
    Rescan,
    New,
}

#[cfg(test)]
async fn run_tick_policy<F>(cursor: &mut i32, window: TickWindow, process: F) -> Result<usize>
where
    F: AsyncFnMut(i32, TickHeight) -> Result<HeightProgress>,
{
    let mut progress = TickProgress::default();
    run_tick_policy_counting(cursor, &mut progress, window, process)
        .await
        .map(|()| progress.processed)
}

async fn run_tick_policy_counting<F>(
    cursor: &mut i32,
    progress: &mut TickProgress,
    window: TickWindow,
    mut process: F,
) -> Result<()>
where
    F: AsyncFnMut(i32, TickHeight) -> Result<HeightProgress>,
{
    let replay_end = (*cursor).min(window.end);
    for height in window.rescan_start..=replay_end {
        match process(height, TickHeight::Rescan).await {
            Ok(HeightProgress::Hold) => {
                progress.held = true;
                progress.processed += 1;
            }
            Ok(HeightProgress::Advance) => progress.processed += 1,
            Err(err) => warn!(
                height,
                error = %err,
                "rescan of already-processed height failed; continuing (best-effort)"
            ),
        }
    }

    if window.end > *cursor {
        for height in (*cursor + 1)..=window.end {
            match process(height, TickHeight::New).await {
                Ok(HeightProgress::Advance) => {
                    *cursor = (*cursor).max(height);
                    progress.processed += 1;
                }
                Ok(HeightProgress::Hold) => {
                    progress.held = true;
                    debug!(
                        height,
                        "new height not yet captured (hold); retry next tick"
                    );
                    break;
                }
                Err(err) => return Err(err),
            }
        }
    }

    Ok(())
}

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
