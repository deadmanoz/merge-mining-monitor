//! Read-model mutation entry points.
//!
//! Owns advisory lock ordering, preclassification, source-health brackets,
//! lock-set retry loops, and post-commit dependent cascades for base-evidence writes.

use anyhow::{Context, Result, bail};
use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use tokio_postgres::{Client, GenericClient, Transaction};
use tracing::debug;

use crate::pending_cascade::PendingCascade;
use crate::protocol_commitments::refresh_protocol_commitments;
use crate::source_health_sql::ParentContribution;
use mmm_bitcoin_core::BitcoinCoreBlockCoinbase;
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::capture::{MergeMiningEventPayload, apply_classification_proof};
use mmm_store::{CaptureWrite, EventWriteOutcome};

mod historical_queue;
use historical_queue::enqueue_historical_parent;
pub use historical_queue::{
    clear_authoritative_historical_provenance_in_transaction, drain_historical_reconcile_queue,
    drain_historical_reconcile_queue_with_nbits_table,
    enqueue_changed_historical_attestations_in_transaction,
};
#[cfg(feature = "db-integration")]
pub use historical_queue::{
    drain_historical_reconcile_queue_with_budget_for_test,
    reconcile_proven_canonical_batch_for_test,
};

mod lineage_gate;
pub use lineage_gate::CaptureOutcome;

mod core_suffix;
mod core_suffix_status;
#[cfg(feature = "db-integration")]
pub use core_suffix::drain_core_reconcile_queue_with_budget_for_test;
pub use core_suffix::{
    CORE_RECOVERY_BATCH, CoreCanonicalReplacement, CoreSuffixReplacementInput,
    CoreSuffixReplacementSummary, ExpectedCoreCanonicalRow, drain_core_reconcile_queue,
    drain_core_reconcile_queue_batch, replace_core_canonical_suffix,
    replace_core_canonical_suffix_validated,
};
pub(crate) use core_suffix::{
    enqueue_core_reconcile_expansions, enqueue_core_reconcile_primaries,
    settle_core_reconcile_primary,
};

use super::{
    CoreCoinbaseStatus, DEFAULT_CASCADE_BUDGET, PreclassifiedParent,
    RECONCILE_LOCK_SET_RETRY_LIMIT, classify_payload_parent, is_reconcile_lock_set_changed,
    load_event, lock_block_hash, lock_core_header_cache_for_reconcile,
    lock_event_for_source_health, lock_payload_parent_read_model_in_txn, preclassify_event_parent,
    reconcile_dependents_after_changes_with_budget, reconcile_one_event_in_txn,
    upsert_core_canonical_header_with_coinbase,
};

fn retry_attempts() -> usize {
    RECONCILE_LOCK_SET_RETRY_LIMIT.max(1)
}

// Shared/exclusive barrier around the local Bitcoin Core canonical view.
// Core-backed classifiers and reconcilers take the shared form before their
// authoritative read and retain it through commit. Canonical-row writers,
// cursor/target bookkeeping, and suffix replacement take the exclusive form
// before final source validation or any block lock. A classifier therefore
// either commits first and is reclassified by a later suffix, or observes the
// already-switched view; competing Core writers serialize their final proof
// and mutation.
const CORE_CANONICAL_VIEW_LOCK_CLASS: i32 = 0x4243; // 'BC'
const CORE_CANONICAL_VIEW_LOCK_OBJECT: i32 = 1;

pub(crate) async fn lock_core_canonical_view_shared<C: GenericClient>(client: &C) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock_shared($1, $2)",
            &[
                &CORE_CANONICAL_VIEW_LOCK_CLASS,
                &CORE_CANONICAL_VIEW_LOCK_OBJECT,
            ],
        )
        .await
        .context("acquire shared Bitcoin Core canonical-view barrier")?;
    Ok(())
}

/// Acquire the stable Core-backed classification view in global lock order.
///
/// Cache refresh holds the exclusive cache lock before reclassifying against
/// the canonical view. Readers must therefore take cache-shared first and the
/// canonical-view barrier second, before any parent advisory or event-row lock.
pub(crate) async fn lock_core_classification_view_shared<C: GenericClient>(
    client: &C,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
) -> Result<()> {
    lock_core_header_cache_for_reconcile(client, nbits_table).await?;
    lock_core_canonical_view_shared(client).await
}

pub(crate) async fn lock_core_canonical_view_exclusive<C: GenericClient>(client: &C) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[
                &CORE_CANONICAL_VIEW_LOCK_CLASS,
                &CORE_CANONICAL_VIEW_LOCK_OBJECT,
            ],
        )
        .await
        .context("acquire exclusive Bitcoin Core canonical-view barrier")?;
    Ok(())
}

/// Snapshot bracket for a primary parent's `source_health` contribution around a base-evidence mutation.
///
/// Opened under the parent advisory lock before the mutation and consumed by [`Self::close`]
/// after in-transaction reconciliation so the primary diff is applied once.
pub(crate) struct PrimarySourceHealthBracket {
    parent_hash: Vec<u8>,
    before: ParentContribution,
}

impl PrimarySourceHealthBracket {
    /// Snapshot the parent's current `source_health` contribution (caller must hold the parent advisory lock).
    pub(crate) async fn open<C: GenericClient>(client: &C, parent_hash: &[u8]) -> Result<Self> {
        let before =
            crate::source_health_sql::snapshot_parent_contribution(client, parent_hash).await?;
        Ok(Self {
            parent_hash: parent_hash.to_vec(),
            before,
        })
    }

    pub(crate) fn parent_hash(&self) -> &[u8] {
        &self.parent_hash
    }

    /// Snapshot the post-mutation contribution and apply the diff to `source_health`.
    pub(crate) async fn close<C: GenericClient>(self, client: &C) -> Result<()> {
        let after =
            crate::source_health_sql::snapshot_parent_contribution(client, &self.parent_hash)
                .await?;
        crate::source_health_sql::apply_source_health_diff(client, &self.before, &after).await
    }
}

/// Ownership of the primary parent's `source_health` diff during an in-transaction reconcile.
pub(crate) enum PrimaryDiff<'a> {
    /// Reconcile snapshots before/after itself (cascade, bulk repair, and reclassify paths).
    Reconcile,
    /// Caller owns `source_health` accounting for the wider mutation (full rebuild or multi-parent bracket).
    BulkImport,
    /// Mutation wrapper opened a bracket before its base write and owns the primary diff.
    Wrapper(&'a PrimarySourceHealthBracket),
}

/// Committed parent-level mutation whose dependent reconcile cascade must be run via [`.cascade()`](Self::cascade).
#[must_use = "a committed read-model mutation must cascade its dependents; call .cascade()"]
pub struct CommittedParentMutation {
    changed_hashes: Vec<Vec<u8>>,
    /// Holds live-producer shutdown off until [`cascade`](Self::cascade) returns.
    _pending: PendingCascade,
}

impl CommittedParentMutation {
    /// Build immediately before `commit()`, so a drop mid-commit is still fenced.
    fn before_commit(changed_hashes: Vec<Vec<u8>>) -> Self {
        Self {
            changed_hashes,
            _pending: PendingCascade::open(),
        }
    }

    /// Reconcile dependents of the committed change under `DEFAULT_CASCADE_BUDGET`.
    pub async fn cascade(
        self,
        client: &mut Client,
        classifier: &ConfiguredParentClassifier,
    ) -> Result<()> {
        cascade_changed_with_nbits_table(
            client,
            classifier,
            self.changed_hashes,
            DEFAULT_CASCADE_BUDGET,
            None,
        )
        .await
    }
}

async fn cascade_changed_with_nbits_table(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    changed_hashes: Vec<Vec<u8>>,
    cascade_budget: usize,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
) -> Result<()> {
    if changed_hashes.is_empty() {
        return Ok(());
    }
    match nbits_table {
        Some(nbits_table) => {
            super::reconcile::reconcile_dependents_after_changes_with_budget_and_nbits_table(
                client,
                &changed_hashes,
                classifier,
                cascade_budget,
                Some(nbits_table),
            )
            .await
        }
        None => {
            reconcile_dependents_after_changes_with_budget(
                client,
                &changed_hashes,
                classifier,
                cascade_budget,
            )
            .await
        }
    }
}

/// Shared per-block capture transaction sequence for AuxPoW producers.
///
/// Passes the classified `payload` into `upsert` (an [`AsyncFn`] invoked across lock-set retries)
/// only when the lineage gate admits the parent.
pub async fn capture_in_txn<F>(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
    chain_label: &str,
    upsert: F,
) -> Result<CaptureOutcome>
where
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<CaptureWrite>,
{
    capture_event(client, source_id, classifier, payload, chain_label, upsert).await
}

/// Write one historical observation inside `txn` and enqueue its parent in
/// `historical_reconcile_queue` when read-model inputs changed.
pub async fn write_historical_base_in_transaction<F>(
    txn: &Transaction<'_>,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
    parent_classification: Option<ParentClassification>,
    upsert: F,
) -> Result<EventWriteOutcome>
where
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<EventWriteOutcome>,
{
    match parent_classification {
        Some(classification) => {
            apply_classification_proof(payload, classification.to_proof())?;
        }
        None => {
            classify_payload_parent(txn, payload, classifier).await?;
        }
    }
    let outcome = upsert(txn, source_id, payload).await?;
    if outcome.parent_read_model_changed {
        enqueue_historical_parent(txn, &payload.btc_parent_header_hash).await?;
    }
    Ok(outcome)
}

/// Durably enqueue a historical parent for a fresh primary reconcile.
pub async fn enqueue_historical_parent_reconcile<C: GenericClient>(
    client: &C,
    parent_hash: &[u8],
) -> Result<()> {
    enqueue_historical_parent(client, parent_hash).await
}

/// Remove authoritative-source events absent from the current publication and
/// durably enqueue every affected parent in the same transaction.
pub async fn reconcile_authoritative_historical_source_in_transaction(
    txn: &Transaction<'_>,
    source_id: i64,
    publication_ref: &str,
    chain: &str,
) -> Result<u64> {
    // Lock removal targets in `e.id` order before enqueueing into `historical_reconcile_queue`.
    let rows = txn
        .query(
            "SELECT e.btc_parent_header_hash \
             FROM merge_mining_event e \
             WHERE e.source_id = $1 \
               AND NOT EXISTS ( \
                    SELECT 1 FROM historical_event_provenance invalid_provenance \
                    WHERE invalid_provenance.event_id = e.id \
                      AND (invalid_provenance.artifact_scope = 'error-block-observations' \
                           OR invalid_provenance.publication_ref = 'operator-csv') \
               ) \
               AND NOT EXISTS ( \
                    SELECT 1 FROM historical_event_provenance p \
                    WHERE p.event_id = e.id \
                      AND p.publication_ref = $2 \
                      AND p.chain = $3 \
               ) \
             ORDER BY e.id \
             FOR UPDATE OF e",
            &[&source_id, &publication_ref, &chain],
        )
        .await
        .context("load authoritative historical removals")?;
    let mut parent_hashes = rows
        .into_iter()
        .map(|row| row.get::<_, Vec<u8>>(0))
        .collect::<Vec<_>>();
    parent_hashes.sort();
    parent_hashes.dedup();
    if let Some(hash) = parent_hashes
        .iter()
        .find(|hash| mmm_capture::invalid_blocks::lookup(hash).is_some())
    {
        bail!(
            "authoritative snapshot would delete catalogued invalid witness for {}; \
             run import-all to establish invalid-observation provenance first",
            BlockHash::from_slice(hash)?
        );
    }

    // The publication was the only witness of these child blocks, so their observations go too.
    let sql = "WITH removed AS ( \
                 DELETE FROM merge_mining_event e \
                 WHERE e.source_id = $1 \
                   AND NOT EXISTS ( \
                        SELECT 1 FROM historical_event_provenance invalid_provenance \
                        WHERE invalid_provenance.event_id = e.id \
                          AND (invalid_provenance.artifact_scope = 'error-block-observations' \
                               OR invalid_provenance.publication_ref = 'operator-csv') \
                   ) \
                   AND NOT EXISTS ( \
                        SELECT 1 FROM historical_event_provenance p \
                        WHERE p.event_id = e.id \
                          AND p.publication_ref = $2 \
                          AND p.chain = $3 \
                   ) \
                 RETURNING e.observation_id \
             ), removed_observations AS ( \
                 DELETE FROM child_block_observation o USING removed e \
                 WHERE o.id = e.observation_id \
             ) \
             SELECT count(*) FROM removed";
    let removed: i64 = txn
        .query_one(sql, &[&source_id, &publication_ref, &chain])
        .await
        .context("remove events absent from authoritative historical snapshot")?
        .get(0);
    for hash in parent_hashes {
        enqueue_historical_parent(txn, &hash).await?;
    }
    u64::try_from(removed).context("count removed historical events")
}

/// Recompute source-health state after all durable historical work has drained.
pub async fn rebuild_historical_source_health(client: &mut Client) -> Result<()> {
    let txn = client
        .transaction()
        .await
        .context("begin historical source-health rebuild")?;
    crate::source_health_sql::rebuild_source_health_in_transaction(&txn).await?;
    txn.commit()
        .await
        .context("commit historical source-health rebuild")
}

/// Capture one merge-mining event: acquire child-height lock (when present), shared
/// Core-view barrier, run lineage gate (a non-Bitcoin parent retracts the child's events
/// and records only its child side), classify, lock parent read-model set, bracket
/// `source_health`, run `upsert`, reconcile in-txn, and cascade dependents after commit.
async fn capture_event<F>(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
    chain_label: &str,
    upsert: F,
) -> Result<CaptureOutcome>
where
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<CaptureWrite>,
{
    let mut attempts = 0;
    let (outcome, committed) = loop {
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {chain_label} capture transaction"))?;
        if let Some(child_height) = payload.child_height {
            mmm_store::lock_child_chain_height(&txn, source_id, child_height).await?;
        }
        lock_core_classification_view_shared(&txn, None).await?;
        match lineage_gate::refuse_parent(&txn, source_id, payload).await? {
            None => {}
            Some(refused @ CaptureOutcome::NotBitcoin { .. }) => {
                let changed = lineage_gate::retract_child_block(&txn, source_id, payload).await?;
                let committed = CommittedParentMutation::before_commit(changed);
                txn.commit()
                    .await
                    .with_context(|| format!("commit {chain_label} retraction"))?;
                break (refused, committed);
            }
            Some(refused) => {
                txn.rollback()
                    .await
                    .with_context(|| format!("roll back {chain_label} pending capture"))?;
                return Ok(refused);
            }
        }
        let preclassified = classify_payload_parent(&txn, payload, classifier).await?;
        lock_payload_parent_read_model_in_txn(&txn, payload, preclassified.as_ref()).await?;
        // Lock the parent even for near/target-failing payloads (which skip `lock_payload_parent_read_model_in_txn`)
        // before opening the source-health bracket.
        lock_block_hash(&txn, &payload.btc_parent_header_hash).await?;
        let bracket =
            PrimarySourceHealthBracket::open(&txn, &payload.btc_parent_header_hash).await?;
        let write = upsert(&txn, source_id, payload).await?;
        let reconcile_result = match write {
            CaptureWrite::Event(written) => {
                reconcile_one_event_in_txn(
                    &txn,
                    written.event_id,
                    classifier,
                    preclassified.clone().map(PreclassifiedParent::trusted),
                    PrimaryDiff::Wrapper(&bracket),
                    None,
                )
                .await
            }
            // A near has no event or `block` row; the bracket counts it. Only its parent outputs
            // can change the parent's protocol commitments.
            CaptureWrite::Near { .. } if payload.btc_parent_coinbase_outputs.is_some() => {
                refresh_protocol_commitments(
                    &txn,
                    std::slice::from_ref(&payload.btc_parent_header_hash),
                )
                .await
                .map(|()| Vec::new())
            }
            CaptureWrite::Near { .. } => Ok(Vec::new()),
        };
        match reconcile_result {
            Ok(changed_hashes) => {
                bracket.close(&txn).await?;
                let committed = CommittedParentMutation::before_commit(changed_hashes);
                txn.commit()
                    .await
                    .with_context(|| format!("commit {chain_label} capture transaction"))?;
                break (CaptureOutcome::Written(write), committed);
            }
            Err(err) if is_reconcile_lock_set_changed(&err) && attempts + 1 < retry_attempts() => {
                txn.rollback().await.with_context(|| {
                    format!("rollback {chain_label} capture transaction after lock-set change")
                })?;
                attempts += 1;
            }
            Err(err) => {
                let _ = txn.rollback().await;
                return Err(err);
            }
        }
    };
    committed.cascade(client, classifier).await?;
    Ok(outcome)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RevocationChange {
    Revoke { reason: String },
    Restore,
}

impl RevocationChange {
    fn op(&self) -> &'static str {
        match self {
            Self::Revoke { .. } => "revoke",
            Self::Restore => "restore",
        }
    }

    fn desired_revoked(&self) -> bool {
        matches!(self, Self::Revoke { .. })
    }
}

/// Revoke or restore one merge-mining event and reconcile its parent read model in one transaction.
pub(crate) async fn set_event_revocation(
    client: &mut Client,
    event_id: i64,
    change: RevocationChange,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    if let RevocationChange::Revoke { reason } = &change
        && reason.trim().is_empty()
    {
        bail!("revocation reason must be non-empty");
    }
    let op = change.op();
    let now = mmm_capture::capture::now_epoch_seconds()?;
    let mut attempt = 0;
    let committed = loop {
        let retry_available = attempt + 1 < retry_attempts();
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {op} reconcile transaction"))?;
        lock_core_classification_view_shared(&txn, None).await?;
        let attempt_preclassified = preclassify_event_parent(&txn, event_id, classifier).await?;
        let event = load_event(&txn, event_id).await?;
        lock_event_for_source_health(&txn, &event, classifier, attempt_preclassified.clone())
            .await?;
        let bracket = PrimarySourceHealthBracket::open(&txn, &event.btc_parent_header_hash).await?;
        let affected = apply_revocation_change(&txn, event_id, &change, now).await?;
        if affected == 0 {
            if noop_revocation_is_complete(txn, event_id, &change, op).await? {
                return Ok(());
            }
            if !retry_available {
                bail!("failed to {op} merge_mining_event {event_id} after retry budget");
            }
            attempt += 1;
            continue;
        }
        match reconcile_one_event_in_txn(
            &txn,
            event_id,
            classifier,
            attempt_preclassified,
            PrimaryDiff::Wrapper(&bracket),
            None,
        )
        .await
        {
            Ok(hashes) => {
                bracket.close(&txn).await?;
                let committed = CommittedParentMutation::before_commit(hashes);
                txn.commit()
                    .await
                    .with_context(|| format!("commit {op} reconcile"))?;
                break committed;
            }
            Err(err) if is_reconcile_lock_set_changed(&err) && retry_available => {
                txn.rollback()
                    .await
                    .with_context(|| format!("rollback {op} reconcile after lock-set change"))?;
                debug!(event_id, attempt, op, "retrying after lock-set change");
                attempt += 1;
            }
            Err(err) => {
                let _ = txn.rollback().await;
                return Err(err);
            }
        }
    };
    committed.cascade(client, classifier).await
}

async fn apply_revocation_change(
    txn: &Transaction<'_>,
    event_id: i64,
    change: &RevocationChange,
    now: i64,
) -> Result<u64> {
    match change {
        RevocationChange::Revoke { reason } => txn
            .execute(
                "UPDATE merge_mining_event \
                 SET revoked_at = $2, revocation_reason = $3 \
                 WHERE id = $1 AND revoked_at IS NULL",
                &[&event_id, &now, reason],
            )
            .await
            .context("revoke merge_mining_event"),
        RevocationChange::Restore => txn
            .execute(
                "UPDATE merge_mining_event \
                 SET revoked_at = NULL, revocation_reason = NULL \
                 WHERE id = $1 AND revoked_at IS NOT NULL",
                &[&event_id],
            )
            .await
            .context("restore merge_mining_event"),
    }
}

async fn noop_revocation_is_complete(
    txn: Transaction<'_>,
    event_id: i64,
    change: &RevocationChange,
    op: &str,
) -> Result<bool> {
    let state = super::event_revoked_state(&txn, event_id).await?;
    txn.rollback()
        .await
        .with_context(|| format!("rollback no-op {op} transaction"))?;
    match state {
        Some(revoked) => Ok(revoked == change.desired_revoked()),
        None => bail!("merge_mining_event id {event_id} not found"),
    }
}

/// Apply base-evidence UPDATEs (`mutate`) to one parent's events and optionally reconcile
/// its read model (`reconcile_anchor`) in the same transaction, acquiring advisory locks before row locks.
pub async fn update_parent_events<F>(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    parent_hash: &[u8],
    mutate: F,
    reconcile_anchor: Option<i64>,
    label: &str,
) -> Result<()>
where
    F: AsyncFn(&Transaction<'_>) -> Result<()>,
{
    let mut attempts = 0;
    let committed = loop {
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {label} transaction"))?;
        if reconcile_anchor.is_some() {
            lock_core_classification_view_shared(&txn, None).await?;
        } else {
            lock_core_canonical_view_shared(&txn).await?;
        }
        match reconcile_anchor {
            Some(anchor_event_id) => {
                let event = load_event(&txn, anchor_event_id).await?;
                lock_event_for_source_health(&txn, &event, classifier, None).await?;
            }
            None => lock_block_hash(&txn, parent_hash).await?,
        }
        mutate(&txn).await?;
        let reconcile_result = match reconcile_anchor {
            Some(anchor_event_id) => {
                reconcile_one_event_in_txn(
                    &txn,
                    anchor_event_id,
                    classifier,
                    None,
                    PrimaryDiff::Reconcile,
                    None,
                )
                .await
            }
            None => Ok(Vec::new()),
        };
        match reconcile_result {
            Ok(changed_hashes) => {
                let committed = CommittedParentMutation::before_commit(changed_hashes);
                txn.commit()
                    .await
                    .with_context(|| format!("commit {label} transaction"))?;
                break committed;
            }
            Err(err) if is_reconcile_lock_set_changed(&err) && attempts + 1 < retry_attempts() => {
                txn.rollback().await.with_context(|| {
                    format!("rollback {label} transaction after lock-set change")
                })?;
                attempts += 1;
            }
            Err(err) => {
                let _ = txn.rollback().await;
                return Err(err);
            }
        }
    };
    committed
        .cascade(client, classifier)
        .await
        .with_context(|| {
            format!(
                "reconcile dependents for parent {}",
                hex::encode(parent_hash)
            )
        })
}

/// Bitcoin Core-attested canonical block payload for [`write_core_canonical`].
pub struct CoreCanonicalWrite<'a> {
    pub header: &'a Header,
    pub height: i32,
    pub coinbase: Option<BitcoinCoreBlockCoinbase>,
}

/// Run `operation` inside a transaction holding the exclusive canonical-view barrier through commit.
pub async fn run_exclusive_core_canonical_view_transaction<T, F>(
    client: &mut Client,
    label: &str,
    operation: F,
) -> Result<T>
where
    F: AsyncFnOnce(&Transaction<'_>) -> Result<T>,
{
    let txn = client
        .transaction()
        .await
        .with_context(|| format!("begin {label} transaction"))?;
    lock_core_canonical_view_exclusive(&txn).await?;
    let output = operation(&txn).await?;
    txn.commit()
        .await
        .with_context(|| format!("commit {label} transaction"))?;
    Ok(output)
}

/// Write one Core-attested canonical `block` row under the parent advisory lock and
/// `source_health` bracket, returning a [`CommittedParentMutation`] token for post-commit cascade.
pub async fn write_core_canonical<F>(
    client: &mut Client,
    write: CoreCanonicalWrite<'_>,
    in_txn_extra: F,
    label: &str,
) -> Result<CommittedParentMutation>
where
    F: AsyncFnOnce(&Transaction<'_>) -> Result<()>,
{
    write_core_canonical_validated(client, write, async |_txn| Ok(()), in_txn_extra, label).await
}

/// Write one Core canonical row after running `validate` under the exclusive canonical-view barrier.
pub async fn write_core_canonical_validated<V, F>(
    client: &mut Client,
    write: CoreCanonicalWrite<'_>,
    validate: V,
    in_txn_extra: F,
    label: &str,
) -> Result<CommittedParentMutation>
where
    V: AsyncFnOnce(&Transaction<'_>) -> Result<()>,
    F: AsyncFnOnce(&Transaction<'_>) -> Result<()>,
{
    let hash_bytes = write.header.block_hash().to_byte_array().to_vec();
    run_exclusive_core_canonical_view_transaction(client, label, async |txn| {
        validate(txn).await?;
        lock_block_hash(txn, &hash_bytes).await?;
        let bracket = PrimarySourceHealthBracket::open(txn, &hash_bytes).await?;
        upsert_core_canonical_header_with_coinbase(txn, write.header, write.height, write.coinbase)
            .await?;
        in_txn_extra(txn).await?;
        bracket.close(txn).await?;
        Ok(CommittedParentMutation::before_commit(vec![
            hash_bytes.clone(),
        ]))
    })
    .await
}

/// Locally revoke one merge-mining event and reconcile its parent read model in the same transaction.
pub async fn revoke_merge_mining_event(
    client: &mut Client,
    event_id: i64,
    reason: &str,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    set_event_revocation(
        client,
        event_id,
        RevocationChange::Revoke {
            reason: reason.to_owned(),
        },
        classifier,
    )
    .await
}

/// Restore one revoked merge-mining event and reconcile its parent read model in the same transaction.
pub async fn restore_merge_mining_event(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    set_event_revocation(client, event_id, RevocationChange::Restore, classifier).await
}

/// Mark a `block` row's Core coinbase fetch as failed without demoting an already `complete` status.
pub async fn record_coinbase_failure<C: GenericClient>(
    client: &C,
    height: i32,
    hash: BlockHash,
) -> Result<()> {
    let hash_bytes = hash.to_byte_array().to_vec();
    let failed = CoreCoinbaseStatus::Failed.as_db_str();
    let complete = CoreCoinbaseStatus::Complete.as_db_str();
    client
        .execute(
            "UPDATE block \
             SET btc_coinbase_status = $3, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE btc_height = $1 \
               AND btc_header_hash = $2 \
               AND btc_coinbase_status <> $4",
            &[&height, &hash_bytes, &failed, &complete],
        )
        .await
        .with_context(|| format!("record Bitcoin Core coinbase failure at height {height}"))?;
    Ok(())
}
