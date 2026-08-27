//! Synchronous local read-model reconciliation.
//!
//! Producers own `merge_mining_event` (and RSK's sidecar). This module derives
//! the local `block` and `attestation_proof` rows from committed base evidence
//! plus Bitcoin Core classifier results.
//!
//! Parent-level base-evidence mutations (producer capture, revoke/restore,
//! pool reclassification, Core canonical writes) enter through root-level
//! mutation facades backed by the private `mutation` command module, which owns
//! lock ordering, classifier preclassification, source-health snapshot brackets,
//! retry policy, and post-commit dependent cascades.

mod cli_args;
mod known_stale_reclassify;
mod mutation;
mod source_health_sql;
mod unknown_reclassify;

pub use classify::resolve_parent_classification;
pub use cli_args::{ArgCursor, drive_args, require_positive};
pub use known_stale_reclassify::{
    KnownStaleReclassifySummary, ReclassifyKnownStalesConfig,
    reclassify_known_stale_hashes_in_transaction, run_reclassify_known_stales,
};
pub use mutation::{
    CommittedParentMutation, CoreCanonicalReplacement, CoreCanonicalWrite,
    CoreSuffixReplacementInput, CoreSuffixReplacementSummary, ExpectedCoreCanonicalRow,
    capture_in_txn, capture_preclassified_in_txn,
    clear_authoritative_historical_provenance_in_transaction, drain_core_reconcile_queue,
    drain_historical_reconcile_queue, drain_historical_reconcile_queue_with_nbits_table,
    enqueue_historical_parent_reconcile, rebuild_historical_source_health,
    reconcile_authoritative_historical_source_in_transaction, record_coinbase_failure,
    replace_core_canonical_suffix, replace_core_canonical_suffix_validated,
    restore_merge_mining_event, revoke_merge_mining_event,
    run_exclusive_core_canonical_view_transaction, update_parent_events, write_core_canonical,
    write_core_canonical_validated, write_historical_base_in_transaction,
};
#[cfg(feature = "db-integration")]
pub use mutation::{
    drain_core_reconcile_queue_with_budget_for_test,
    drain_historical_reconcile_queue_with_budget_for_test,
};
pub use source_health_sql::invalidate_source_health_in_transaction;
#[cfg(any(test, feature = "db-integration"))]
pub use source_health_sql::{compute_source_health_from_base, rebuild_source_health};
pub use unknown_reclassify::{
    ReclassifyUnknownParentsConfig, run_reclassify_unknown_parents,
    run_reclassify_unknown_parents_strict,
};

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;

use anyhow::{Context, Result, bail};
use bitcoin::block::Header;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash as _;
use serde_json::json;
use tokio_postgres::types::Json;
use tokio_postgres::{Client, GenericClient, Row};
use tracing::debug;

use mmm_bitcoin_core::BitcoinCoreBlockCoinbase;
use mmm_bitcoin_core::{
    BlockKind, ClassifiedHeader, ConfiguredParentClassifier, HeightSource, ParentClassification,
    ParentPreflight,
};
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::btc_orphan::{self, BtcOrphanVerdict};
use mmm_capture::capture::{MergeMiningEventPayload, ParentKind, apply_classification_proof};
use mmm_capture::core_coinbase::resolve_btc_pool_from_coinbase;
use mmm_capture::nbits_table::NbitsTable;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_store::get_source_id;

use mutation::PrimaryDiff;

// Advisory-lock domain for per-parent-header read-model reconciliation.
const BLOCK_LOCK_SEED: i64 = 6_022_140_767_761_816_313;
const DEFAULT_BATCH_SIZE: i64 = 100;
const DEFAULT_MAX_ITERATIONS: usize = 100;
const DEFAULT_CASCADE_BUDGET: usize = 10_000;
pub(crate) const RECONCILE_LOCK_SET_RETRY_LIMIT: usize = 3;

/// In-memory projection of one `merge_mining_event` parent row used as the
/// reconciler's classification input. Hash/byte fields hold internal
/// (`to_byte_array`) order; not reversed. Loaded via `event_from_row` in queries.rs.
#[derive(Debug, PartialEq, Eq)]
struct MergeMiningEvent {
    id: i64,
    btc_parent_header_hash: Vec<u8>,
    btc_parent_prev_header_hash: Vec<u8>,
    btc_parent_header_bytes: Vec<u8>,
    btc_parent_header_time: i64,
    btc_parent_height: Option<i32>,
    btc_parent_kind: ParentKind,
    pow_validates_btc_target: bool,
    difficulty_epoch_ok: Option<bool>,
    btc_parent_coinbase_script: Option<Vec<u8>>,
    btc_parent_coinbase_outputs: Option<Vec<u8>>,
}

impl MergeMiningEvent {
    fn skips_parent_read_model(&self) -> bool {
        self.btc_parent_kind == ParentKind::Near || !self.pow_validates_btc_target
    }
}

/// A classification handed to the reconciler ahead of the parent header it applies
/// to. `for_event` pins `expected_parent_hash` so the reconcile can detect the
/// header shifting under it (lock-set change); `trusted` carries no expected hash
/// for callers that already hold the correct parent (e.g. capture's own preclassify).
/// Strict tokens require the barrier-time revalidation to propagate Core errors.
#[derive(Debug, Clone)]
struct PreclassifiedParent {
    expected_parent_hash: Option<Vec<u8>>,
    classification: ParentClassification,
    strict_core_errors: bool,
}

impl PreclassifiedParent {
    fn trusted(classification: ParentClassification) -> Self {
        Self {
            expected_parent_hash: None,
            classification,
            strict_core_errors: false,
        }
    }

    fn for_event(
        event: &MergeMiningEvent,
        classification: ParentClassification,
        strict_core_errors: bool,
    ) -> Self {
        Self {
            expected_parent_hash: Some(event.btc_parent_header_hash.clone()),
            classification,
            strict_core_errors,
        }
    }
}

/// The full set of derived `block` column values assembled by classify.rs before a
/// single-statement upsert in writers.rs. Built so `kind`, `btc_orphan_class`, and
/// the coinbase columns are written together and never transiently violate the
/// row's CHECK constraints.
#[derive(Debug)]
struct BlockInput {
    hash: Vec<u8>,
    prev_hash: Vec<u8>,
    height: Option<i32>,
    height_source: Option<HeightSource>,
    kind: BlockKind,
    header_bytes: Vec<u8>,
    header_time: i64,
    bitcoin_miner_pool_id: Option<i64>,
    btc_coinbase_txid: Option<Vec<u8>>,
    btc_coinbase_script: Option<Vec<u8>>,
    btc_coinbase_outputs: Option<Vec<u8>>,
    btc_coinbase_status: CoreCoinbaseStatus,
    error_block_reason: Option<String>,
    canonical_competitor_hash: Option<Vec<u8>>,
    total_attestations: i32,
    distinct_sources: i32,
    auxpow_chain_count: i32,
    live_observed: bool,
    core_attested: bool,
    pow_validated: bool,
    difficulty_epoch_ok: Option<bool>,
    first_attested_at: Option<i64>,
    last_attested_at: Option<i64>,
    /// Derived orphan refinement for kind='unknown' blocks; NULL (None) for
    /// canonical/stale/error-block and for pending/unclassified unknowns. Written in the same
    /// statement as `kind` so the `btc_orphan_class IS NULL OR kind='unknown'`
    /// CHECK is never transiently violated.
    btc_orphan_class: Option<String>,
}

/// Persisted lifecycle of a block's Bitcoin Core coinbase fetch, written to
/// `block.btc_coinbase_status`. Monotonic in practice: a `Complete` fetch is
/// never demoted back to `NotAttempted`/`Failed` by a later reconcile that
/// could not reach Core, so a transient classifier outage cannot blank an
/// already-resolved coinbase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum CoreCoinbaseStatus {
    #[default]
    NotAttempted,
    Complete,
    Failed,
}

impl CoreCoinbaseStatus {
    /// DB token for `block.btc_coinbase_status`. The three strings are the CHECK
    /// constraint domain; do not change them without a migration.
    pub(crate) fn as_db_str(self) -> &'static str {
        match self {
            Self::NotAttempted => "not_attempted",
            Self::Complete => "complete",
            Self::Failed => "failed",
        }
    }
}

/// Aggregate evidence across all live attestations of one parent header
/// (attestation/source/auxpow-chain counts, PoW + difficulty-epoch flags, attested
/// timestamps), computed in queries.rs and folded into the derived `block` row.
#[derive(Debug)]
struct ParentRollup {
    total_attestations: i32,
    distinct_sources: i32,
    auxpow_chain_count: i32,
    pow_validated: bool,
    difficulty_epoch_ok: Option<bool>,
    first_attested_at: Option<i64>,
    last_attested_at: Option<i64>,
}

/// The block-table coinbase columns derived from one Core `getblock` coinbase
/// (txid/script/outputs plus the `CoreCoinbaseStatus`).
/// `Default` is the not-attempted state. Built by `coinbase_columns` in writers.rs.
#[derive(Debug, Default)]
struct CoreCoinbaseColumns {
    txid: Option<Vec<u8>>,
    script: Option<Vec<u8>>,
    outputs: Option<Vec<u8>>,
    status: CoreCoinbaseStatus,
}

/// Snapshot of the derived `block` fields that, if they change, must enqueue
/// dependent reconciles (stale competitor rows, descendant blocks). Equality on this
/// struct is the cascade's change detector: a parent reconcile compares before/after
/// and only fans out when the snapshot differs.
#[derive(Debug, PartialEq, Eq)]
struct BlockCascadeState {
    kind: BlockKind,
    btc_height: Option<i32>,
    btc_height_source: Option<HeightSource>,
    error_block_reason: Option<String>,
    canonical_competitor_hash: Option<Vec<u8>>,
    core_attested: bool,
    difficulty_epoch_ok: Option<bool>,
    btc_coinbase_script: Option<Vec<u8>>,
}

/// Count of rows touched by one reconcile call, split by grain. `parents_reconciled`
/// is the changed parent headers; `descendants_reconciled` is the dependent
/// cascade fanout (descendant events, derived child blocks, stale competitors)
/// drained after the parent change. Returned so callers can log cascade size and
/// spot runaway fanout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileStats {
    pub parents_reconciled: usize,
    pub descendants_reconciled: usize,
}

/// Config for the `run_reconcile_read_model` driver. `missing_only` (the default)
/// scans only rows whose derived state is absent/stale and iterates to a fixpoint;
/// `--all` does a bounded full rescan from a height/source window. A child-height
/// bound selects only rows with an authenticated child height; exact events with
/// `NULL` height participate only in an unbounded or source-only scan.
/// `batch_size` and `max_iterations` bound work per pass and overall; exhausting
/// them raises `ReconcileBudgetExhausted` rather than looping forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileReadModelConfig {
    pub missing_only: bool,
    pub start_height: Option<i32>,
    pub end_height: Option<i32>,
    pub source_code: Option<String>,
    pub batch_size: i64,
    pub max_iterations: usize,
    /// Rebuild `source_health` from base tables before
    /// the read-model scan. This is the post-migration / drift-repair step for
    /// the per-source `/sources` counters; it sets `source_health_ready = TRUE`.
    pub rebuild_source_health: bool,
}

impl Default for ReconcileReadModelConfig {
    fn default() -> Self {
        Self {
            missing_only: true,
            start_height: None,
            end_height: None,
            source_code: None,
            batch_size: DEFAULT_BATCH_SIZE,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            rebuild_source_health: false,
        }
    }
}

impl ReconcileReadModelConfig {
    /// Parse `reconcile-read-model` CLI flags via the shared `cli_args` flag-walk.
    /// `--rebuild-source-health` is a dedicated mode that runs ONLY the source_health
    /// recompute (see `run_reconcile_read_model`), not a scan. Rejects non-positive
    /// `--batch-size`/`--max-iterations` so the driver's budget invariants hold.
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut config = Self::default();
        cli_args::drive_args("reconcile-read-model", args, |flag, cur| {
            Ok(match flag {
                "--missing-only" => {
                    config.missing_only = true;
                    true
                }
                "--all" => {
                    config.missing_only = false;
                    true
                }
                "--start-height" => {
                    config.start_height = Some(cur.parse("--start-height")?);
                    true
                }
                "--end-height" => {
                    config.end_height = Some(cur.parse("--end-height")?);
                    true
                }
                "--source" => {
                    config.source_code = Some(cur.value("--source")?.to_owned());
                    true
                }
                "--batch-size" => {
                    config.batch_size = cur.parse("--batch-size")?;
                    true
                }
                "--max-iterations" => {
                    config.max_iterations = cur.parse("--max-iterations")?;
                    true
                }
                "--rebuild-source-health" => {
                    config.rebuild_source_health = true;
                    true
                }
                _ => false,
            })
        })?;
        if config.batch_size <= 0 {
            bail!("--batch-size must be positive");
        }
        if config.max_iterations == 0 {
            bail!("--max-iterations must be positive");
        }
        Ok(config)
    }
}

/// Reconcile the derived rows for one event's parent header, then drain the
/// post-commit dependent cascade under `DEFAULT_CASCADE_BUDGET`. `preclassified`
/// supplies a trusted offline classification when the classifier is disabled;
/// an enabled classifier re-resolves it after taking the shared Core-view
/// barrier. `None` lets the reconciler classify from event-derived evidence.
/// This is the parent-grain repair entry the reconcile/reclassify drivers loop
/// over.
pub async fn reconcile_from_merge_mining_event(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<ParentClassification>,
) -> Result<ReconcileStats> {
    reconcile_from_merge_mining_event_with_nbits_table(
        client,
        event_id,
        classifier,
        preclassified,
        None,
    )
    .await
}

/// Reconcile the one active parent evidence set identified by its Bitcoin
/// header hash. This is the narrow operator repair path for a newly-supported
/// consensus rule; it delegates to the normal event reconciler so it takes the
/// Core-view barrier, applies source-health diffs, and drains dependents exactly
/// as capture and bulk repair do.
pub async fn run_reclassify_parent(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    hash: &[u8],
) -> Result<ReconcileStats> {
    if !classifier.is_enabled() {
        bail!("reclassify-parent requires a Bitcoin Core classifier");
    }
    let event_id = find_anchor_event_for_block(client, hash)
        .await?
        .with_context(|| {
            format!(
                "no active merge-mining event for parent {}",
                hex::encode(hash)
            )
        })?;
    let mut queue = VecDeque::new();
    queue.push_back(ReconcileWork::StrictEvent(event_id));
    drain_reconcile_queue(
        client,
        classifier,
        queue,
        Some(DEFAULT_CASCADE_BUDGET),
        None,
    )
    .await
}

async fn reconcile_from_merge_mining_event_with_nbits_table(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<ParentClassification>,
    nbits_table: Option<&NbitsTable>,
) -> Result<ReconcileStats> {
    reconcile_from_merge_mining_event_with_preclassification(
        client,
        event_id,
        classifier,
        preclassified.map(PreclassifiedParent::trusted),
        nbits_table,
    )
    .await
}

async fn reconcile_from_merge_mining_event_with_preclassification(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
    nbits_table: Option<&NbitsTable>,
) -> Result<ReconcileStats> {
    let mut queue = VecDeque::new();
    queue.push_back(ReconcileWork::Event(event_id, preclassified.map(Box::new)));
    drain_reconcile_queue(
        client,
        classifier,
        queue,
        Some(DEFAULT_CASCADE_BUDGET),
        nbits_table,
    )
    .await
}

async fn reconcile_by_block_hash_with_nbits_table(
    client: &mut Client,
    hash: &[u8],
    classifier: &ConfiguredParentClassifier,
    nbits_table: Option<&NbitsTable>,
) -> Result<ReconcileStats> {
    let mut queue = VecDeque::new();
    queue.push_back(ReconcileWork::Block(hash.to_vec()));
    drain_reconcile_queue(
        client,
        classifier,
        queue,
        Some(DEFAULT_CASCADE_BUDGET),
        nbits_table,
    )
    .await
}

/// Classify a payload's parent header against Core. No-ops when PoW does not
/// validate the BTC target or the classifier is disabled (returns `None`). On
/// success it folds the classification proof back into `payload` and returns
/// the classification for lock-set computation. Live capture calls this after
/// taking its shared canonical-view barrier but before any per-parent locks;
/// other repair paths may still preclassify before opening their transaction.
pub(crate) async fn classify_payload_parent<C: GenericClient>(
    client: &C,
    payload: &mut MergeMiningEventPayload,
    classifier: &ConfiguredParentClassifier,
) -> Result<Option<ParentClassification>> {
    if !payload.pow_validates_btc_target {
        return Ok(None);
    }

    let header: Header = deserialize(&payload.btc_parent_header_bytes)
        .context("deserialize payload parent header for classification")?;
    if !classifier.is_enabled()
        && mmm_capture::error_blocks::lookup(&payload.btc_parent_header_hash).is_none()
    {
        return Ok(None);
    }
    let live = if classifier.is_enabled() {
        let preflight = load_parent_preflight(client, &payload.btc_parent_prev_header_hash).await?;
        Some(classifier.classify_parent(&header, preflight).await?)
    } else {
        None
    };
    let classification = resolve_parent_classification(&header, live)?;
    apply_classification_proof(payload, classification.to_proof())?;
    Ok(Some(classification))
}

/// Acquire, in sorted order, every advisory lock the in-txn reconcile of this
/// payload's parent will need: the parent hash, its prev hash, any classification
/// lock hashes, and the persisted `canonical_competitor_hash`. Sorting+dedup gives
/// a global lock ordering so concurrent mutations cannot deadlock. No-ops for
/// `near` parents and target-failing PoW (nothing derived). Pre-locking the full
/// set is what lets the reconcile detect a lock-set change and retry instead of
/// taking locks mid-reconcile.
pub(crate) async fn lock_payload_parent_read_model_in_txn<C: GenericClient>(
    client: &C,
    payload: &MergeMiningEventPayload,
    preclassified: Option<&ParentClassification>,
) -> Result<()> {
    if payload.btc_parent_kind == ParentKind::Near || !payload.pow_validates_btc_target {
        return Ok(());
    }

    let mut hashes = vec![
        payload.btc_parent_header_hash.clone(),
        payload.btc_parent_prev_header_hash.clone(),
    ];
    if let Some(classification) = preclassified {
        push_classification_lock_hashes(&mut hashes, classification);
    }
    if let Some(persisted) =
        load_block_cascade_state(client, &payload.btc_parent_header_hash).await?
        && let Some(hash) = persisted.canonical_competitor_hash
    {
        hashes.push(hash);
    }
    hashes.sort();
    hashes.dedup();
    lock_block_hashes(client, &hashes).await
}

/// Top-level read-model driver. Three disjoint modes: (1) `rebuild_source_health`
/// runs ONLY the source_health recompute and returns 0 (kept
/// separate so `just rebuild-source-health` cannot trip an unrelated
/// reconcile-budget or classifier failure after `source_health_ready` is set);
/// (2) `!missing_only` does a bounded full rescan over a height/source window;
/// (3) the default missing-only mode iterates the candidate scan to a fixpoint.
/// Returns the repaired count, or `ReconcileBudgetExhausted` when the
/// batch/iteration budget runs out before convergence.
pub async fn run_reconcile_read_model(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: ReconcileReadModelConfig,
) -> Result<usize> {
    if config.rebuild_source_health {
        crate::source_health_sql::rebuild_source_health(client).await?;
        tracing::info!("rebuilt source_health from base tables");
        // Dedicated operation: a rebuild does NOT also run the read-model scan, so
        // `just rebuild-source-health` cannot unexpectedly repair unrelated rows,
        // run long, or fail on an unrelated reconcile-budget/classifier issue
        // after source_health_ready has already been set. Run a separate
        // reconcile-read-model invocation when a read-model scan is also wanted.
        return Ok(0);
    }

    // Degraded-state guard: a Core-enabled scan assigns btc_orphan_class, and an
    // empty known-stale membership means the exclusion gate cannot fire, so a
    // catalogued stale could be labelled strict/weak. Warn once per run (only
    // when Core is enabled: a Core-off reconcile does not fresh-classify).
    if classifier.is_enabled() && mmm_store::count_known_stale_blocks(client).await? == 0 {
        tracing::warn!(
            "reconcile-read-model: known_stale_block is EMPTY; known stales cannot be excluded \
             and may be labelled strict/weak. Import the upstream stale-blocks dataset with \
             import-known-stales."
        );
    }

    if !config.missing_only {
        return run_reconcile_all_read_model(client, classifier, &config, None).await;
    }

    run_reconcile_missing_read_model(client, classifier, &config, None).await
}

/// `--missing-only` mode: repeatedly scan for missing/stale derived rows until
/// no candidates remain. Each scan is capped by `batch_size`; convergence before
/// `max_iterations` means the read model reached a fixpoint.
async fn run_reconcile_missing_read_model(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &ReconcileReadModelConfig,
    nbits_table: Option<&NbitsTable>,
) -> Result<usize> {
    let mut repaired = 0;
    for iteration in 0..config.max_iterations {
        let candidates = load_reconcile_candidates(client, config, classifier.is_enabled()).await?;
        if candidates.is_empty() {
            debug!(iteration, repaired, "read-model reconciliation converged");
            return Ok(repaired);
        }

        for candidate in candidates {
            match candidate {
                ReconcileCandidate::Event(id) => {
                    reconcile_from_merge_mining_event_with_nbits_table(
                        client,
                        id,
                        classifier,
                        None,
                        nbits_table,
                    )
                    .await?;
                }
                ReconcileCandidate::Block(hash) => {
                    reconcile_by_block_hash_with_nbits_table(
                        client,
                        &hash,
                        classifier,
                        nbits_table,
                    )
                    .await?;
                }
            }
            repaired += 1;
        }
    }
    Err(reconcile_budget_exhausted(config))
}

/// `--all` mode: bounded full rescan of every non-`near` event in the optional
/// height/source window, keyset-paginated by `(child_height, id)` and reconciled
/// one event at a time. Any child-height bound deliberately excludes events
/// whose authenticated child height is unavailable; an unbounded/source-only
/// scan includes them after height-bearing rows. Raises `ReconcileBudgetExhausted`
/// if `max_iterations` pages are consumed without draining the window.
async fn run_reconcile_all_read_model(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &ReconcileReadModelConfig,
    nbits_table: Option<&NbitsTable>,
) -> Result<usize> {
    let source_id = match &config.source_code {
        Some(code) => Some(get_source_id(client, code).await?),
        None => None,
    };
    let mut repaired = 0;
    let mut cursor: Option<(i64, i64)> = None;

    for iteration in 0..config.max_iterations {
        let cursor_height = cursor.map(|(child_height, _)| child_height);
        let cursor_id = cursor.map(|(_, id)| id);
        let rows = client
            .query(
                "SELECT id, COALESCE(child_height::bigint, 2147483648::bigint) AS sort_child_height \
                 FROM merge_mining_event \
                 WHERE btc_parent_kind <> 'near' \
                   AND ($1::int IS NULL OR child_height >= $1) \
                   AND ($2::int IS NULL OR child_height <= $2) \
                   AND ($3::bigint IS NULL OR source_id = $3) \
                   AND ($5::bigint IS NULL OR (COALESCE(child_height::bigint, 2147483648::bigint), id) > ($5::bigint, $6::bigint)) \
                 ORDER BY child_height NULLS LAST, id \
                 LIMIT $4",
                &[
                    &config.start_height,
                    &config.end_height,
                    &source_id,
                    &config.batch_size,
                    &cursor_height,
                    &cursor_id,
                ],
            )
            .await
            .context("load full reconcile candidates")?;
        if rows.is_empty() {
            debug!(
                iteration,
                repaired, "full read-model reconciliation completed"
            );
            return Ok(repaired);
        }

        for row in rows {
            let event_id: i64 = row.get(0);
            let child_height: i64 = row.get(1);
            cursor = Some((child_height, event_id));
            reconcile_from_merge_mining_event_with_nbits_table(
                client,
                event_id,
                classifier,
                None,
                nbits_table,
            )
            .await?;
            repaired += 1;
        }
    }

    Err(reconcile_budget_exhausted(config))
}

fn reconcile_budget_exhausted(config: &ReconcileReadModelConfig) -> anyhow::Error {
    ReconcileBudgetExhausted {
        max_iterations: config.max_iterations,
        batch_size: config.batch_size,
    }
    .into()
}

/// Raised when `run_reconcile_read_model`/`run_reconcile_all_read_model` consume
/// `max_iterations` batches without converging. Distinct from the per-change
/// cascade overflow `ReconcileCascadeBudgetExhausted`. Callers downcast via
/// `is_reconcile_budget_exhausted`; producers treat it as a soft 'retry with a
/// bigger budget' signal rather than a hard error.
#[derive(Debug)]
pub(crate) struct ReconcileBudgetExhausted {
    max_iterations: usize,
    batch_size: i64,
}

impl fmt::Display for ReconcileBudgetExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "reconcile-read-model did not converge within {} iterations at batch size {}; retry with larger --max-iterations or --batch-size",
            self.max_iterations, self.batch_size
        )
    }
}

impl std::error::Error for ReconcileBudgetExhausted {}

/// True if `err` is a reconcile-budget overflow: EITHER the iteration-budget
/// `ReconcileBudgetExhausted` OR the per-change cascade overflow
/// `ReconcileCascadeBudgetExhausted`. Producers match on this to downgrade a
/// non-convergent reconcile to a retriable warning instead of failing the poll.
pub fn is_reconcile_budget_exhausted(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ReconcileBudgetExhausted>().is_some()
        || err
            .downcast_ref::<ReconcileCascadeBudgetExhausted>()
            .is_some()
}

/// Raised when the post-commit dependent cascade for a SINGLE change exceeds its
/// budget (default `DEFAULT_CASCADE_BUDGET`), guarding against an unbounded fanout
/// of descendant events / derived blocks / stale competitors. Recovery is a
/// `reconcile-read-model --missing-only` rerun (per the Display text). Folded into
/// `is_reconcile_budget_exhausted`.
#[derive(Debug)]
pub(crate) struct ReconcileCascadeBudgetExhausted {
    budget: usize,
}

impl fmt::Display for ReconcileCascadeBudgetExhausted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read-model cascade exceeded budget {} during a single change; rerun reconcile-read-model --missing-only",
            self.budget
        )
    }
}

impl std::error::Error for ReconcileCascadeBudgetExhausted {}

/// Raised inside the in-txn reconcile when the set of advisory locks actually
/// needed turns out to differ from the set pre-acquired by
/// `lock_payload_parent_read_model_in_txn` (e.g. classification or competitor hash
/// shifted under concurrency). The mutation/reconcile retry loops catch it via
/// `is_reconcile_lock_set_changed` and re-run the txn with the corrected lock set,
/// up to `RECONCILE_LOCK_SET_RETRY_LIMIT`. Crate-visible because it crosses the
/// reconcile/mutation module boundary as an Error.
#[derive(Debug)]
pub(crate) struct ReconcileLockSetChanged {
    event_id: i64,
}

impl fmt::Display for ReconcileLockSetChanged {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "read-model lock set changed while reconciling event {}; retry reconciliation",
            self.event_id
        )
    }
}

impl std::error::Error for ReconcileLockSetChanged {}

/// True if `err` is `ReconcileLockSetChanged`. The bounded retry loops in
/// `mutation` and `reconcile` match on this to re-run the txn with the corrected
/// advisory-lock set rather than surfacing the error.
pub(crate) fn is_reconcile_lock_set_changed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<ReconcileLockSetChanged>().is_some()
}

mod classify;
mod competition;
mod queries;
mod reconcile;
mod writers;

pub use classify::load_parent_preflight;
pub(crate) use classify::*;
pub use competition::load_persisted_kind_and_orphan_class;
pub(crate) use competition::*;
pub(crate) use queries::*;
pub use queries::{lock_block_hash, lock_block_hashes};
pub use reconcile::reconcile_dependents_after_change;
pub(crate) use reconcile::*;
pub(crate) use writers::*;

#[cfg(test)]
mod config_args_tests {
    use super::{ReclassifyUnknownParentsConfig, ReconcileReadModelConfig};

    #[test]
    fn reconcile_from_args_defaults() {
        let config = ReconcileReadModelConfig::from_args(Vec::<String>::new()).expect("defaults");
        assert!(config.missing_only);
        assert_eq!(config.start_height, None);
        assert_eq!(config.end_height, None);
        assert_eq!(config.source_code, None);
        assert!(!config.rebuild_source_health);
    }

    #[test]
    fn reconcile_from_args_parses_every_flag() {
        let config = ReconcileReadModelConfig::from_args([
            "--all",
            "--start-height",
            "5",
            "--end-height",
            "9",
            "--source",
            "auxpow:namecoin",
            "--batch-size",
            "10",
            "--max-iterations",
            "3",
            "--rebuild-source-health",
        ])
        .expect("full flag set");
        assert!(!config.missing_only);
        assert_eq!(config.start_height, Some(5));
        assert_eq!(config.end_height, Some(9));
        assert_eq!(config.source_code.as_deref(), Some("auxpow:namecoin"));
        assert_eq!(config.batch_size, 10);
        assert_eq!(config.max_iterations, 3);
        assert!(config.rebuild_source_health);
    }

    #[test]
    fn reconcile_from_args_pins_error_text() {
        let unknown = ReconcileReadModelConfig::from_args(["--nope"]).expect_err("unknown flag");
        assert_eq!(
            unknown.to_string(),
            "unknown reconcile-read-model argument \"--nope\""
        );
        let missing =
            ReconcileReadModelConfig::from_args(["--start-height"]).expect_err("missing value");
        assert_eq!(missing.to_string(), "--start-height requires a value");
        let invalid = ReconcileReadModelConfig::from_args(["--batch-size", "abc"])
            .expect_err("invalid value");
        assert_eq!(
            invalid.to_string(),
            "--batch-size has invalid value \"abc\""
        );
        let zero =
            ReconcileReadModelConfig::from_args(["--batch-size", "0"]).expect_err("zero batch");
        assert_eq!(zero.to_string(), "--batch-size must be positive");
        let iters = ReconcileReadModelConfig::from_args(["--max-iterations", "0"])
            .expect_err("zero iterations");
        assert_eq!(iters.to_string(), "--max-iterations must be positive");
    }

    #[test]
    fn reclassify_unknown_from_args_defaults_and_flags() {
        let config =
            ReclassifyUnknownParentsConfig::from_args(Vec::<String>::new()).expect("defaults");
        assert!(!config.recheck_orphans);
        let config =
            ReclassifyUnknownParentsConfig::from_args(["--batch-size", "7", "--recheck-orphans"])
                .expect("flags");
        assert_eq!(config.batch_size, 7);
        assert!(config.recheck_orphans);
    }

    #[test]
    fn reclassify_unknown_from_args_pins_error_text() {
        let unknown =
            ReclassifyUnknownParentsConfig::from_args(["--nope"]).expect_err("unknown flag");
        assert_eq!(
            unknown.to_string(),
            "unknown reclassify-unknown-parents argument \"--nope\""
        );
        let missing =
            ReclassifyUnknownParentsConfig::from_args(["--batch-size"]).expect_err("missing");
        assert_eq!(missing.to_string(), "--batch-size requires a value");
        let invalid = ReclassifyUnknownParentsConfig::from_args(["--batch-size", "abc"])
            .expect_err("invalid");
        assert_eq!(
            invalid.to_string(),
            "--batch-size has invalid value \"abc\""
        );
        let zero =
            ReclassifyUnknownParentsConfig::from_args(["--batch-size", "0"]).expect_err("zero");
        assert_eq!(zero.to_string(), "--batch-size must be positive");
    }
}
