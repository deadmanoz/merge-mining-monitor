//! Bitcoin Core backbone sync.
//!
//! This is the durable Bitcoin-spine producer: it fills canonical block rows
//! from Bitcoin Core with header and coinbase evidence as one complete unit.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use serde_json::json;
use tokio_postgres::{Client, GenericClient, Transaction};

use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_bitcoin_core::{BitcoinCoreBlockCoinbase, BitcoinCoreRpcClient};
use mmm_capture::pool_resolver::PoolResolver;
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use mmm_read_model::{
    CommittedParentMutation, CoreCanonicalWrite, drive_args, reconcile_dependents_after_change,
    record_coinbase_failure, run_exclusive_core_canonical_view_transaction,
    write_core_canonical_validated,
};
use mmm_store::{get_source_id, upsert_pool_snapshot};

mod integrity;
mod live;
mod live_config;
mod live_window;
mod reorg;
mod sync_state;

// Live-mode API, re-exported from the crate root for normal callers while
// this module reopens under db-integration for tests.
pub use live::run_sync_bitcoin_core_follow;
#[cfg(any(test, feature = "db-integration"))]
pub use live::{
    initialize_follow_state, record_retryable_repair_failure_for_test,
    run_bitcoin_core_follow_tick_for_test,
};
use live_window::live_backbone_window_start_height;
pub(crate) use live_window::{repair_near_tip_gaps_to_target, verify_live_backbone_window};
#[cfg(any(test, feature = "db-integration"))]
pub use reorg::repair_near_tip_backbone_for_test;
pub(crate) use reorg::repair_near_tip_backbone_to_target;
use reorg::target_stability_failure;
// Integrity guards live in `integrity.rs`; re-exported at crate visibility so
// `follow` and `live_window` keep importing them from `super`, unchanged.
use integrity::BackboneIntegrityFailure;
pub(crate) use integrity::{BackboneIntegrityError, integrity_error, is_backbone_integrity_error};
use integrity::{guard_existing_link, guard_header_link, guard_same_height_conflicts};
use sync_state::{
    CanonicalHeightRow, SyncState, accept_live_repaired_target, advance_contiguous_complete_prefix,
    guard_no_pending_core_reconcile, load_canonical_rows_at_height, load_or_init_sync_state,
    skipped_complete_has_missing_dependents, update_sync_error, update_sync_progress,
    verify_or_set_target_tip,
};

#[cfg(any(test, feature = "db-integration"))]
pub async fn accept_live_repaired_target_for_test<S>(
    client: &mut Client,
    source: &S,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    accept_live_repaired_target(client, source, source_id, target).await
}
// Live-mode config knobs consumed by `BitcoinCoreSyncConfig::{default, from_args_with_lookup}`.
use live_config::{
    DEFAULT_FOLLOW_INTERVAL_SECS, DEFAULT_NEAR_TIP_REPAIR_WINDOW_HEIGHTS, FOLLOW_DEFAULT_LIMIT,
    parse_follow_interval_from_lookup, parse_near_tip_repair_window_from_lookup,
};

/// One-shot per-batch height cap when `--limit` is unset; the live producer
/// overrides this with the smaller `FOLLOW_DEFAULT_LIMIT`.
const DEFAULT_LIMIT: i64 = 100;
/// The single `sync_mode` key for the contiguous backbone cursor. There is one
/// `bitcoin_core_sync_state` row per source under this key; all cursor reads and
/// writes are scoped by `(source_id, sync_mode)`.
const SYNC_MODE_CONTIGUOUS: &str = "contiguous";
const RETRYABLE_REPAIR_ERROR_CODE: &str = "near_tip_reorg_repair_retry";
const TARGET_TIP_CHANGED_ERROR_CODE: &str = "target_tip_changed";

/// A Bitcoin Core tip observation: the active-chain height and its block hash.
/// Carried as a unit so `verify_or_set_target_tip` can detect a same-height tip
/// reorg (height unchanged, hash differs) and fail-stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitcoinCoreBackboneTip {
    /// Active-chain height of this tip.
    pub height: i32,
    /// Block hash at `height`, held as rust-bitcoin internal/wire bytes.
    pub hash: BlockHash,
}

/// The Bitcoin Core read trait the backbone sync needs, abstracted so tests
/// can drive it with an in-memory fake instead of a live node. The production
/// impl is on `BitcoinCoreRpcClient`; this crate is the only one combining a
/// chain RPC source with DB writes.
#[allow(async_fn_in_trait)]
pub trait BitcoinCoreBackboneSource: Send + Sync {
    /// Fetch the current active-chain tip (height + hash).
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip>;
    /// Resolve the active-chain block hash at `height`.
    async fn block_hash(&self, height: i32) -> Result<BlockHash>;
    /// Fetch the 80-byte header for `hash` (prev-link and coinbase evidence).
    async fn block_header(&self, hash: BlockHash) -> Result<Header>;
    /// Fetch the coinbase evidence (script, pool tag inputs) for `hash`.
    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase>;
}

/// Live-node impl: each method delegates to the corresponding corepc RPC,
/// converting heights to/from `u64` and surfacing an out-of-range height as an
/// error rather than panicking.
impl BitcoinCoreBackboneSource for BitcoinCoreRpcClient {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip> {
        let height_u64 = self.get_block_count().await?;
        let height: i32 = height_u64
            .try_into()
            .context("Bitcoin Core tip height exceeds i32")?;
        let hash = self.get_block_hash(height_u64).await?;
        Ok(BitcoinCoreBackboneTip { height, hash })
    }

    async fn block_hash(&self, height: i32) -> Result<BlockHash> {
        let height_u64: u64 = height
            .try_into()
            .context("Bitcoin Core backbone height must be non-negative")?;
        self.get_block_hash(height_u64).await
    }

    async fn block_header(&self, hash: BlockHash) -> Result<Header> {
        self.get_block_header(hash).await
    }

    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        self.get_block_coinbase(hash).await
    }
}

/// Resolved knobs for one backbone sync invocation, shared by the one-shot
/// batch, the live producer, and the live-window repair pass. Built by
/// `from_args` for the CLI or constructed directly by internal callers (e.g.
/// `repair_near_tip_gaps_to_target` pins an explicit `from`/`to` window).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCoreSyncConfig {
    /// Explicit start height; when `None` the batch resumes from
    /// `contiguous_complete_height + 1`. Cannot be combined with `--follow`.
    pub from_height: Option<i32>,
    /// Explicit inclusive end height; when `None` the batch runs to the tip,
    /// capped by `limit`. Cannot be combined with `--limit` or `--follow`.
    pub to_height: Option<i32>,
    /// Maximum heights processed in one batch when `to_height` is unset. Must be
    /// positive and fit within `i32`.
    pub limit: i64,
    /// Record and verify the live tip as the sync target before processing
    /// (detects a same-height tip reorg). Forced on in follow mode.
    pub tip: bool,
    /// Skip heights already present as a complete canonical row matching Core.
    /// Forced on in follow mode so retries and the forward crawl stay cheap.
    pub missing_only: bool,
    /// Per-height sleep, from `BITCOIN_CORE_SYNC_DELAY_MS`, to rate-limit RPC.
    pub delay: Duration,
    /// Run as a continuous catch-up-then-follow-tip daemon instead of a one-shot
    /// batch. Forces `tip` and `missing_only`; see `run_sync_bitcoin_core_follow`.
    pub follow: bool,
    /// Poll interval between follow-mode batches once caught up (or after a
    /// transient batch failure). Only meaningful when `follow` is set.
    pub follow_interval: Duration,
    /// Bounded recent Core window maintained for the tree's default live-tip
    /// projection. Only meaningful when `follow` is set.
    pub near_tip_repair_window_heights: i32,
}

impl Default for BitcoinCoreSyncConfig {
    fn default() -> Self {
        Self {
            from_height: None,
            to_height: None,
            limit: DEFAULT_LIMIT,
            tip: false,
            missing_only: false,
            delay: Duration::from_millis(0),
            follow: false,
            follow_interval: Duration::from_secs(DEFAULT_FOLLOW_INTERVAL_SECS),
            near_tip_repair_window_heights: DEFAULT_NEAR_TIP_REPAIR_WINDOW_HEIGHTS,
        }
    }
}

impl BitcoinCoreSyncConfig {
    /// Parse CLI args for `sync-bitcoin-core`, reading follow-mode env knobs from
    /// the real process environment. Thin wrapper over `from_args_with_lookup`.
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::from_args_with_lookup(args, |key| std::env::var(key).ok())
    }

    /// Argument parser with the follow-interval environment lookup injected, so
    /// follow-mode env behavior is unit-testable without mutating the global
    /// process environment. `env_lookup` is consulted ONLY when `--follow` is
    /// present, so an invalid follow-interval value cannot affect a one-shot run.
    pub(crate) fn from_args_with_lookup<I, S, L>(args: I, env_lookup: L) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        L: Fn(&str) -> Option<String>,
    {
        let mut config = Self {
            delay: parse_delay_from_env()?,
            ..Self::default()
        };
        let mut limit_seen = false;
        drive_args("sync-bitcoin-core", args, |flag, cur| {
            Ok(match flag {
                "--from-height" => {
                    config.from_height = Some(cur.parse("--from-height")?);
                    true
                }
                "--to-height" => {
                    config.to_height = Some(cur.parse("--to-height")?);
                    true
                }
                "--limit" => {
                    limit_seen = true;
                    config.limit = cur.parse("--limit")?;
                    true
                }
                "--tip" => {
                    config.tip = true;
                    true
                }
                "--missing-only" => {
                    config.missing_only = true;
                    true
                }
                "--follow" => {
                    config.follow = true;
                    true
                }
                _ => false,
            })
        })?;
        if config.follow {
            // Follow mode always runs the default contiguous pass from the
            // persisted cursor to the live tip, skipping already-complete rows.
            if config.from_height.is_some() || config.to_height.is_some() {
                bail!("--follow cannot be combined with --from-height or --to-height");
            }
            config.tip = true;
            config.missing_only = true;
            if !limit_seen {
                config.limit = FOLLOW_DEFAULT_LIMIT;
            }
            config.follow_interval = parse_follow_interval_from_lookup(&env_lookup)?;
            config.near_tip_repair_window_heights =
                parse_near_tip_repair_window_from_lookup(&env_lookup)?;
        }
        if config.limit <= 0 {
            bail!("--limit must be positive");
        }
        if config.limit > i32::MAX as i64 {
            bail!("--limit must fit within i32");
        }
        if limit_seen && config.to_height.is_some() {
            bail!("--limit cannot be combined with --to-height");
        }
        if config.from_height.is_some_and(|height| height < 0)
            || config.to_height.is_some_and(|height| height < 0)
        {
            bail!("sync-bitcoin-core heights must be non-negative");
        }
        if let (Some(from), Some(to)) = (config.from_height, config.to_height)
            && from > to
        {
            bail!("--from-height must be <= --to-height");
        }
        Ok(config)
    }
}

/// Per-batch outcome tally, summed across heights. The live producer and
/// live-window repair inspect `coinbase_failed` to decide whether to retry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BitcoinCoreSyncStats {
    /// Heights the batch tried to process.
    pub attempted: usize,
    /// Heights written complete (header + coinbase) this batch.
    pub completed: usize,
    /// Heights already complete and matching Core, skipped under `missing_only`.
    pub skipped_complete: usize,
    /// Heights written header-only after a coinbase fetch failure.
    pub coinbase_failed: usize,
}

/// Run one backbone sync batch: seed the pool snapshot, resolve the source,
/// advance the contiguous cursor, then fill `from_height..=to_height` (resolved
/// from config + cursor + tip, capped by `limit`). Each height writes only
/// canonical `block` evidence through `read_model::mutation`; the cursor is
/// updated monotonically. Returns the per-batch stats.
pub async fn run_sync_bitcoin_core<S>(
    client: &mut Client,
    source: &S,
    config: BitcoinCoreSyncConfig,
) -> Result<BitcoinCoreSyncStats>
where
    S: BitcoinCoreBackboneSource,
{
    let resolver = PoolResolver::from_default_snapshot().context("load embedded pool snapshot")?;
    upsert_pool_snapshot(client, resolver.snapshot())
        .await
        .context("seed pool snapshot for Bitcoin Core backbone sync")?;

    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let (mut state, tip) = prepare_sync_batch(client, source, source_id, config.tip).await?;
    if let Some(to_height) = config.to_height
        && to_height > tip.height
    {
        let err = anyhow!(
            "--to-height {to_height} exceeds Bitcoin Core tip {}",
            tip.height
        );
        update_sync_error(
            client,
            source_id,
            to_height,
            "to_height_above_tip",
            &err.to_string(),
            json!({ "core_tip_height": tip.height }),
        )
        .await?;
        return Err(err);
    }
    let from_height = config
        .from_height
        .unwrap_or_else(|| (state.contiguous_complete_height + 1).max(0));
    let limit_end_height = (from_height as i64 + config.limit - 1).min(i32::MAX as i64) as i32;
    let uncapped_to_height = config.to_height.unwrap_or(tip.height);
    let to_height = if config.to_height.is_some() {
        uncapped_to_height
    } else {
        uncapped_to_height.min(limit_end_height)
    };

    let mut stats = BitcoinCoreSyncStats::default();
    if from_height > to_height {
        return Ok(stats);
    }

    let default_contiguous_pass = config.from_height.is_none();
    for height in from_height..=to_height {
        stats.attempted += 1;
        match sync_one_height(
            client,
            source,
            source_id,
            height,
            config.missing_only,
            default_contiguous_pass,
            &mut state,
        )
        .await
        {
            Ok(HeightSyncOutcome::Completed) => {
                stats.completed += 1;
            }
            Ok(HeightSyncOutcome::SkippedComplete) => {
                stats.skipped_complete += 1;
            }
            Ok(HeightSyncOutcome::CoinbaseFailed) => {
                stats.coinbase_failed += 1;
            }
            Err(err) => {
                return Err(err);
            }
        }

        if !config.delay.is_zero() {
            tokio::time::sleep(config.delay).await;
        }
    }

    Ok(stats)
}

async fn prepare_sync_batch<S>(
    client: &mut Client,
    source: &S,
    source_id: i64,
    pin_target: bool,
) -> Result<(SyncState, BitcoinCoreBackboneTip)>
where
    S: BitcoinCoreBackboneSource,
{
    let result = run_exclusive_core_canonical_view_transaction(
        client,
        "prepare Bitcoin Core sync batch",
        async |txn| {
            guard_no_pending_core_reconcile(txn, source_id).await?;
            let mut state = load_or_init_sync_state(txn, source_id).await?;
            let tip = source.tip().await.context("fetch Bitcoin Core tip")?;
            if pin_target {
                verify_or_set_target_tip(txn, source_id, &mut state, tip).await?;
            }
            advance_contiguous_complete_prefix(txn, source_id, &mut state).await?;
            Ok((state, tip))
        },
    )
    .await;
    if let Err(err) = &result
        && let Some(failure) = err.downcast_ref::<BackboneIntegrityFailure>()
    {
        failure.persist(client, source_id).await?;
    }
    result
}

/// Result of syncing a single height, mapped 1:1 onto a `BitcoinCoreSyncStats`
/// counter by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeightSyncOutcome {
    /// Header + coinbase written this call.
    Completed,
    /// Already complete and matching Core; skipped under `missing_only`.
    SkippedComplete,
    /// Header written but coinbase fetch failed; row left coinbase-incomplete.
    CoinbaseFailed,
}

/// Sync one height as a single fallible unit: fetch the Core hash, run the
/// same-height and link integrity guards, short-circuit if `missing_only` and
/// the row is already complete, then write header + coinbase (or header-only on
/// a coinbase failure) through `write_core_canonical`. Advances the contiguous
/// cursor and defers the dependent-reconcile cascade until after sync-state
/// bookkeeping so failure-path observability is preserved.
async fn sync_one_height<S>(
    client: &mut Client,
    source: &S,
    source_id: i64,
    height: i32,
    missing_only: bool,
    default_contiguous_pass: bool,
    state: &mut SyncState,
) -> Result<HeightSyncOutcome>
where
    S: BitcoinCoreBackboneSource,
{
    let core_hash = source
        .block_hash(height)
        .await
        .with_context(|| format!("fetch Bitcoin Core hash at height {height}"))?;
    let rows = load_canonical_rows_at_height(client, height).await?;
    guard_same_height_conflicts(client, source_id, height, core_hash, &rows).await?;

    if missing_only
        && let Some(row) = rows.first()
        && row.hash.as_slice() == core_hash.to_byte_array().as_slice()
        && row.coinbase_status == "complete"
        && let Some(skipped_hash) = try_skip_complete(
            client,
            source,
            source_id,
            height,
            core_hash,
            default_contiguous_pass,
            state,
        )
        .await?
    {
        if skipped_complete_has_missing_dependents(client, &skipped_hash).await? {
            reconcile_dependents_after_change(
                client,
                &skipped_hash,
                &ConfiguredParentClassifier::Disabled,
            )
            .await?;
        }
        return Ok(HeightSyncOutcome::SkippedComplete);
    }

    let header = source
        .block_header(core_hash)
        .await
        .with_context(|| format!("fetch Bitcoin Core header for {core_hash}"))?;
    guard_header_link(
        client,
        source_id,
        height,
        &header.prev_blockhash.to_byte_array(),
        default_contiguous_pass,
    )
    .await?;
    let observation = CoreHeightObservation {
        source,
        source_id,
        height,
        hash: core_hash,
        header: &header,
        default_contiguous_pass,
    };

    let coinbase = match source.block_coinbase(core_hash).await {
        Ok(coinbase) => coinbase,
        Err(err) => {
            let err_msg = err.to_string();
            // The mutation module owns the parent lock, the source-health
            // bracket, and the commit; the coinbase-failure status update rides
            // in the same transaction as the injected extra. The returned token
            // defers the dependent cascade until after the sync-state
            // bookkeeping below, preserving failure-path observability.
            let committed = write_observed_core_canonical(
                client,
                &observation,
                None,
                async |txn| {
                    record_coinbase_failure(txn, height, core_hash).await?;
                    update_sync_error(
                        txn,
                        source_id,
                        height,
                        "coinbase_fetch_failed",
                        &err_msg,
                        json!({ "hash": core_hash.to_string() }),
                    )
                    .await
                },
                "sync-bitcoin-core coinbase failure",
            )
            .await?;
            state.preserve_error = true;
            committed
                .cascade(client, &ConfiguredParentClassifier::Disabled)
                .await?;
            return Ok(HeightSyncOutcome::CoinbaseFailed);
        }
    };

    let committed = write_observed_core_canonical(
        client,
        &observation,
        Some(coinbase),
        async |txn| {
            advance_contiguous_complete_prefix(txn, source_id, state).await?;
            update_sync_progress(txn, source_id, height, state).await
        },
        "sync-bitcoin-core",
    )
    .await?;
    committed
        .cascade(client, &ConfiguredParentClassifier::Disabled)
        .await?;
    Ok(HeightSyncOutcome::Completed)
}

async fn try_skip_complete<S>(
    client: &mut Client,
    source: &S,
    source_id: i64,
    height: i32,
    expected_hash: BlockHash,
    default_contiguous_pass: bool,
    state: &mut SyncState,
) -> Result<Option<Vec<u8>>>
where
    S: BitcoinCoreBackboneSource,
{
    let result = run_exclusive_core_canonical_view_transaction(
        client,
        "skip complete Bitcoin Core height",
        async |txn| {
            guard_no_pending_core_reconcile(txn, source_id).await?;
            let current_hash = source.block_hash(height).await.with_context(|| {
                format!("revalidate skipped Bitcoin Core hash at height {height}")
            })?;
            if current_hash != expected_hash {
                let failure = active_hash_changed_failure(height, expected_hash, current_hash);
                return Err(failure.into_error());
            }
            let rows = load_canonical_rows_at_height(txn, height).await?;
            guard_same_height_conflicts(txn, source_id, height, expected_hash, &rows).await?;
            let [row] = rows.as_slice() else {
                return Ok(None);
            };
            if row.hash.as_slice() != expected_hash.to_byte_array().as_slice()
                || row.coinbase_status != "complete"
            {
                return Ok(None);
            }
            guard_existing_link(txn, source_id, height, row, default_contiguous_pass).await?;
            advance_contiguous_complete_prefix(txn, source_id, state).await?;
            update_sync_progress(txn, source_id, height, state).await?;
            Ok(Some(row.hash.clone()))
        },
    )
    .await;
    if let Err(err) = &result
        && let Some(failure) = err.downcast_ref::<BackboneIntegrityFailure>()
    {
        failure.persist(client, source_id).await?;
    }
    result
}

struct CoreHeightObservation<'a, S> {
    source: &'a S,
    source_id: i64,
    height: i32,
    hash: BlockHash,
    header: &'a Header,
    default_contiguous_pass: bool,
}

impl<S: BitcoinCoreBackboneSource> CoreHeightObservation<'_, S> {
    /// Recheck both Core and the local topology under the mutation layer's
    /// exclusive canonical-view barrier.
    async fn validate<C: GenericClient>(&self, client: &C) -> Result<()> {
        guard_no_pending_core_reconcile(client, self.source_id).await?;
        let current =
            self.source.block_hash(self.height).await.with_context(|| {
                format!("revalidate Bitcoin Core hash at height {}", self.height)
            })?;
        if current != self.hash {
            return Err(active_hash_changed_failure(self.height, self.hash, current).into_error());
        }
        let rows = load_canonical_rows_at_height(client, self.height).await?;
        guard_same_height_conflicts(client, self.source_id, self.height, self.hash, &rows).await?;
        guard_header_link(
            client,
            self.source_id,
            self.height,
            &self.header.prev_blockhash.to_byte_array(),
            self.default_contiguous_pass,
        )
        .await
    }
}

async fn write_observed_core_canonical<S, F>(
    client: &mut Client,
    observation: &CoreHeightObservation<'_, S>,
    coinbase: Option<BitcoinCoreBlockCoinbase>,
    in_txn_extra: F,
    label: &str,
) -> Result<CommittedParentMutation>
where
    S: BitcoinCoreBackboneSource,
    F: AsyncFnOnce(&Transaction<'_>) -> Result<()>,
{
    let result = write_core_canonical_validated(
        client,
        CoreCanonicalWrite {
            header: observation.header,
            height: observation.height,
            coinbase,
        },
        async |txn| observation.validate(txn).await,
        in_txn_extra,
        label,
    )
    .await;
    if let Err(err) = &result
        && let Some(failure) = err.downcast_ref::<BackboneIntegrityFailure>()
    {
        failure.persist(client, observation.source_id).await?;
    }
    result
}

fn active_hash_changed_failure(
    height: i32,
    expected: BlockHash,
    current: BlockHash,
) -> BackboneIntegrityFailure {
    BackboneIntegrityFailure::new(
        BackboneIntegrityError::HeightConflict,
        "backbone_height_conflict",
        height,
        format!(
            "Bitcoin Core active hash changed before canonical write at height {height}: \
             fetched={expected}, current={current}"
        ),
        json!({
            "core_hash": current.to_string(),
            "fetched_hash": expected.to_string(),
        }),
    )
}

/// Per-height RPC throttle from `BITCOIN_CORE_SYNC_DELAY_MS` (milliseconds).
/// Unset means zero delay. Unlike the follow interval, a zero value is allowed
/// here: a one-shot batch is finite, so it cannot hot-loop.
fn parse_delay_from_env() -> Result<Duration> {
    match std::env::var("BITCOIN_CORE_SYNC_DELAY_MS") {
        Ok(value) => {
            let millis: u64 = value.parse().with_context(|| {
                format!("BITCOIN_CORE_SYNC_DELAY_MS has invalid value {value:?}")
            })?;
            Ok(Duration::from_millis(millis))
        }
        Err(std::env::VarError::NotPresent) => Ok(Duration::from_millis(0)),
        Err(err) => Err(err).context("read BITCOIN_CORE_SYNC_DELAY_MS"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_args_pins_error_text() {
        let unknown = BitcoinCoreSyncConfig::from_args(["--nope"]).expect_err("unknown flag");
        assert_eq!(
            unknown.to_string(),
            "unknown sync-bitcoin-core argument \"--nope\""
        );
        let missing = BitcoinCoreSyncConfig::from_args(["--limit"]).expect_err("missing value");
        assert_eq!(missing.to_string(), "--limit requires a value");
        let invalid =
            BitcoinCoreSyncConfig::from_args(["--limit", "abc"]).expect_err("invalid value");
        assert_eq!(invalid.to_string(), "--limit has invalid value \"abc\"");
    }

    #[test]
    fn from_args_rejects_oversize_limit() {
        let err =
            BitcoinCoreSyncConfig::from_args(["--limit", "3000000000"]).expect_err("limit fails");
        assert!(err.to_string().contains("--limit must fit within i32"));
    }

    #[test]
    fn from_args_rejects_invalid_ranges_and_flags() {
        let cases = [
            (
                vec!["--limit", "0"],
                "--limit must be positive",
                "zero limit",
            ),
            (
                vec!["--from-height", "-1"],
                "heights must be non-negative",
                "negative from height",
            ),
            (
                vec!["--to-height", "-1"],
                "heights must be non-negative",
                "negative to height",
            ),
            (
                vec!["--from-height", "5", "--to-height", "4"],
                "--from-height must be <= --to-height",
                "inverted range",
            ),
            (
                vec!["--to-height", "10", "--limit", "5"],
                "--limit cannot be combined with --to-height",
                "explicit height plus limit",
            ),
            (
                vec!["--unknown"],
                "unknown sync-bitcoin-core argument",
                "unknown flag",
            ),
            (
                vec!["--from-height"],
                "--from-height requires a value",
                "missing flag value",
            ),
        ];
        for (args, expected, label) in cases {
            let err = BitcoinCoreSyncConfig::from_args(args).expect_err(label);
            assert!(
                err.to_string().contains(expected),
                "{label}: expected {expected:?}, got {err:?}"
            );
        }
    }
}
