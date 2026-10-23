//! Bitcoin Core backbone sync: fills canonical `block` rows with header and
//! coinbase evidence.

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

use integrity::BackboneIntegrityFailure;
pub(crate) use integrity::{BackboneIntegrityError, integrity_error, is_backbone_integrity_error};
use integrity::{guard_existing_link, guard_header_link, guard_same_height_conflicts};
pub use live::run_sync_bitcoin_core_follow;
#[cfg(any(test, feature = "db-integration"))]
pub use live::{
    record_retryable_repair_failure_for_test, run_bitcoin_core_follow_tick_for_test,
    run_bitcoin_core_initial_follow_tick_for_test,
};
use live_window::live_backbone_window_start_height;
pub(crate) use live_window::{repair_near_tip_gaps_to_target, verify_live_backbone_window};
#[cfg(any(test, feature = "db-integration"))]
pub use reorg::repair_near_tip_backbone_for_test;
pub(crate) use reorg::repair_near_tip_backbone_to_target;
use reorg::target_stability_failure;
use sync_state::{
    CanonicalHeightRow, SyncState, accept_live_repaired_target, advance_contiguous_complete_prefix,
    clear_resolved_backbone_link_error, guard_no_pending_core_reconcile,
    load_canonical_rows_at_height, load_or_init_sync_state,
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

use live_config::{
    DEFAULT_FOLLOW_INTERVAL_SECS, DEFAULT_NEAR_TIP_REPAIR_WINDOW_HEIGHTS, FOLLOW_DEFAULT_LIMIT,
    parse_follow_interval_from_lookup, parse_near_tip_repair_window_from_lookup,
};

const DEFAULT_LIMIT: i64 = 100;
const SYNC_MODE_CONTIGUOUS: &str = "contiguous";
// Status codes cleared when a verified live target is accepted.
const RETRYABLE_REPAIR_ERROR_CODE: &str = "near_tip_reorg_repair_retry";
const TARGET_TIP_CHANGED_ERROR_CODE: &str = "target_tip_changed";
const REORG_REPAIR_ERROR_CODE: &str = "near_tip_reorg_repair_failed";
const LIVE_WINDOW_INVARIANT_ERROR_CODE: &str = "live_window_invariant_failed";
const REPAIR_OWNED_ERROR_CODES: [&str; 4] = [
    RETRYABLE_REPAIR_ERROR_CODE,
    TARGET_TIP_CHANGED_ERROR_CODE,
    REORG_REPAIR_ERROR_CODE,
    LIVE_WINDOW_INVARIANT_ERROR_CODE,
];

/// Active-chain height and block hash, carried together so `verify_or_set_target_tip`
/// detects same-height tip reorgs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitcoinCoreBackboneTip {
    pub height: i32,
    pub hash: BlockHash,
}

#[allow(async_fn_in_trait)]
pub trait BitcoinCoreBackboneSource: Send + Sync {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip>;
    async fn block_hash(&self, height: i32) -> Result<BlockHash>;
    async fn block_header(&self, hash: BlockHash) -> Result<Header>;
    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase>;
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCoreSyncConfig {
    /// Resumes from `contiguous_complete_height + 1` when `None`.
    pub from_height: Option<i32>,
    /// Runs to tip capped by `limit` when `None`.
    pub to_height: Option<i32>,
    pub limit: i64,
    /// Record and verify the live tip as the sync target before processing (forced on in follow mode).
    pub tip: bool,
    /// Skip heights already present as a complete canonical row matching Core (forced on in follow mode).
    pub missing_only: bool,
    pub delay: Duration,
    pub follow: bool,
    pub follow_interval: Duration,
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
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::from_args_with_lookup(args, |key| std::env::var(key).ok())
    }

    /// `env_lookup` is consulted only when `--follow` is present.
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BitcoinCoreSyncStats {
    pub attempted: usize,
    pub completed: usize,
    pub skipped_complete: usize,
    pub coinbase_failed: usize,
}

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
            clear_resolved_backbone_link_error(txn, source, source_id, &state).await?;
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeightSyncOutcome {
    Completed,
    SkippedComplete,
    CoinbaseFailed,
}

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
        Err(err) if crate::live_loop::must_propagate(&err) => return Err(err),
        Err(err) => {
            let err_msg = err.to_string();
            // Defer dependent cascade until after sync-state bookkeeping commits.
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
    /// Recheck Core and local topology under the exclusive canonical-view barrier.
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
