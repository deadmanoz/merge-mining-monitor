//! Read-model mutation commands.
//!
//! Every parent-level base-evidence mutation (producer capture, revoke/restore,
//! offline pool reclassification, Core canonical block writes) enters the read
//! model through one of the four entry points here. The module owns the
//! orchestration ritual that callers previously hand-rolled with divergent
//! coverage: advisory lock ordering, classifier preclassification, the
//! source-health before/after snapshot bracket, primary-diff ownership, the
//! in-transaction reconcile, bounded rollback-and-retry on a reconcile lock-set
//! change, and the post-commit dependent cascade.
//!
//! Callers describe WHAT changed (an upsert callback, a revocation flip, a set
//! of event updates, a Core header + coinbase); this module decides HOW the
//! derived state is kept consistent. Chain- and command-specific SQL stays in
//! the injected callbacks, so byte-format details stay out of this module.

use anyhow::{Context, Result, bail};
use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use tokio_postgres::{Client, GenericClient, Transaction};
use tracing::debug;

use crate::source_health_sql::ParentContribution;
use mmm_bitcoin_core::BitcoinCoreBlockCoinbase;
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::capture::{MergeMiningEventPayload, ParentKind, apply_classification_proof};
use mmm_store::EventWriteOutcome;

mod historical_queue;
#[cfg(feature = "db-integration")]
pub use historical_queue::drain_historical_reconcile_queue_with_budget_for_test;
use historical_queue::enqueue_historical_parent;
pub use historical_queue::{
    drain_historical_reconcile_queue, drain_historical_reconcile_queue_with_nbits_table,
};

mod core_suffix;
mod core_suffix_status;
#[cfg(feature = "db-integration")]
pub use core_suffix::drain_core_reconcile_queue_with_budget_for_test;
pub use core_suffix::{
    CoreCanonicalReplacement, CoreSuffixReplacementInput, CoreSuffixReplacementSummary,
    ExpectedCoreCanonicalRow, drain_core_reconcile_queue, replace_core_canonical_suffix,
    replace_core_canonical_suffix_validated,
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

/// Internal capture knob for an optional pre-decided parent classification.
/// `Some` is the preclassified path
/// (`capture_preclassified_in_txn`), where the caller already ran the Core
/// verdict to gate the write; an enabled classifier re-resolves it under the
/// shared barrier. `None` lets `capture_event` classify the parent itself (the
/// ordinary live-producer path via `capture_in_txn`).
#[derive(Default)]
struct CaptureEventOptions {
    parent_classification: Option<ParentClassification>,
}

/// Proof token that a mutation wrapper snapshotted the primary parent's
/// source-health contribution BEFORE its base-evidence mutation ran.
///
/// Constructed only by [`PrimarySourceHealthBracket::open`] (which requires the
/// caller to already hold the parent advisory lock) and consumed by
/// [`PrimarySourceHealthBracket::close`], so the diff cannot be applied twice.
/// [`super::reconcile_one_event_in_txn`] accepts wrapper-owned primary-diff
/// claims only as `PrimaryDiff::Wrapper(&bracket)`: a caller cannot claim
/// ownership without holding an opened bracket. The one property the type
/// cannot prove, that `open` ran before the mutation, lives in the four
/// audited entry points of this module instead of in every caller.
pub(crate) struct PrimarySourceHealthBracket {
    parent_hash: Vec<u8>,
    before: ParentContribution,
}

impl PrimarySourceHealthBracket {
    /// Snapshot the parent's current source-health contribution. The caller
    /// MUST already hold the parent-hash advisory lock.
    pub(crate) async fn open<C: GenericClient>(client: &C, parent_hash: &[u8]) -> Result<Self> {
        let before =
            crate::source_health_sql::snapshot_parent_contribution(client, parent_hash).await?;
        Ok(Self {
            parent_hash: parent_hash.to_vec(),
            before,
        })
    }

    /// The bracketed primary parent hash. `reconcile_one_event_in_txn` reads this
    /// to skip the wrapper-owned hash when applying its synthesized
    /// predecessor/competitor diffs, so the primary diff is never double-applied.
    pub(crate) fn parent_hash(&self) -> &[u8] {
        &self.parent_hash
    }

    /// Snapshot the parent's post-mutation contribution and apply the diff to
    /// `source_health`. Consumes the bracket.
    pub(crate) async fn close<C: GenericClient>(self, client: &C) -> Result<()> {
        let after =
            crate::source_health_sql::snapshot_parent_contribution(client, &self.parent_hash)
                .await?;
        crate::source_health_sql::apply_source_health_diff(client, &self.before, &after).await
    }
}

/// Who applies the primary parent's source-health diff during an in-transaction
/// reconcile. Replaces the old invisible `primary_owned_by_caller: bool`.
pub(crate) enum PrimaryDiff<'a> {
    /// No wrapper: the reconcile snapshots before/after itself (cascade, bulk
    /// repair, and reclassify paths, where the reconcile's `before` is
    /// genuinely pre-mutation).
    Reconcile,
    /// The caller owns source-health accounting for the wider mutation, through
    /// either a later base-table rebuild or a multi-parent bracket, so this
    /// reconcile skips its incremental primary diff.
    BulkImport,
    /// A wrapper opened a bracket BEFORE its own base mutation and owns the
    /// primary diff; the reconcile must not diff the primary hash (it would
    /// double-apply or use a post-mutation `before`). The reconcile still owns
    /// the synthesized predecessor/competitor diffs, whose hashes never
    /// overlap the wrapper-owned primary.
    Wrapper(&'a PrimarySourceHealthBracket),
}

/// A committed parent-level mutation whose dependent reconcile cascade has not
/// run yet. Returned by [`write_core_canonical`] so its transaction commits
/// before callers cascade dependents; everything else in this module cascades
/// inline.
///
/// `#[must_use]` + the repo's `-D warnings` gate turn a forgotten cascade (the
/// bug class fixed in 7d8f616) into a build failure.
#[must_use = "a committed read-model mutation must cascade its dependents; call .cascade()"]
pub struct CommittedParentMutation {
    changed_hashes: Vec<Vec<u8>>,
}

impl CommittedParentMutation {
    /// Reconcile dependents of the committed change (descendant events, derived
    /// child blocks, stale-competitor blocks) under the standard mutation cascade
    /// budget.
    pub async fn cascade(
        self,
        client: &mut Client,
        classifier: &ConfiguredParentClassifier,
    ) -> Result<()> {
        cascade_changed(
            client,
            classifier,
            self.changed_hashes,
            DEFAULT_CASCADE_BUDGET,
        )
        .await
    }
}

/// Run the post-commit dependent cascade for `changed_hashes`, or no-op on an
/// empty set. The single inline-cascade tail shared by `capture_event`,
/// `set_event_revocation`, and `update_parent_events`; `write_core_canonical`
/// instead defers this via the `CommittedParentMutation` token. The empty-set
/// short-circuit is the near / no-reconcile-anchor / no-membership-change
/// case where nothing derived can have moved.
async fn cascade_changed(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    changed_hashes: Vec<Vec<u8>>,
    cascade_budget: usize,
) -> Result<()> {
    cascade_changed_with_nbits_table(client, classifier, changed_hashes, cascade_budget, None).await
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

/// Shared per-block capture transaction sequence for every AuxPoW producer.
///
/// Thin producer-facing wrapper over `capture_event`. The read-model mutation
/// module owns the orchestration (preclassify, lock ordering, the source-health
/// bracket, the bounded lock-set-change retry loop, and the post-commit
/// dependent cascade); the single chain-specific operation, the upsert, is
/// injected as `upsert`.
///
/// The classified `payload` is passed INTO the callback rather than captured at
/// the call site: the mutation module holds the only `&mut` borrow of `payload`
/// (it needs it for classification), so a callback that also captured the
/// payload would create overlapping borrows at the call site. Chain-specific
/// extras (RSK's `rsk_merge_mining_evidence` sidecar) are captured by the
/// callback and never appear in this signature, so no byte-format concern leaks
/// into the shared callback.
///
/// `upsert` is [`AsyncFn`] (not `AsyncFnOnce`): the retry loop may invoke it more
/// than once.
pub async fn capture_in_txn<F>(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
    chain_label: &str,
    upsert: F,
) -> Result<i64>
where
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<EventWriteOutcome>,
{
    let outcome = capture_event(
        client,
        source_id,
        classifier,
        payload,
        chain_label,
        upsert,
        CaptureEventOptions::default(),
    )
    .await?;
    Ok(outcome.event_id)
}

/// [`capture_in_txn`] variant for callers that already had to classify the
/// parent before deciding whether a write is allowed.
///
/// When the classifier is enabled, the supplied gating decision is re-resolved
/// after the shared Core-view barrier is held and rejected if it changed. A
/// disabled classifier accepts the caller's offline verdict directly. It is
/// intentionally narrow; ordinary live producers should keep using
/// [`capture_in_txn`] so the mutation module owns their classification.
pub async fn capture_preclassified_in_txn<F>(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
    parent_classification: ParentClassification,
    chain_label: &str,
    upsert: F,
) -> Result<i64>
where
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<EventWriteOutcome>,
{
    let outcome = capture_event(
        client,
        source_id,
        classifier,
        payload,
        chain_label,
        upsert,
        CaptureEventOptions {
            parent_classification: Some(parent_classification),
        },
    )
    .await?;
    Ok(outcome.event_id)
}

/// Write one historical observation inside a caller-owned chain transaction.
///
/// Historical publication imports intentionally stop at base evidence here.
/// A parent whose read-model inputs changed is enqueued durably in the same
/// transaction, then [`drain_historical_reconcile_queue`] rebuilds parent and
/// dependent state after the chain snapshot commits. Provenance and
/// presentation-only refreshes skip that redundant work. Keeping advisory
/// read-model locks out of the chain transaction prevents a broad import from
/// retaining one lock per parent until commit.
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
///
/// Callers that discover additional historical repair work after the base
/// import must enqueue it before attempting the primary rebuild. The queue
/// retains both an unfinished primary and its committed dependent-cascade
/// seeds across process failures.
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
    // Lock every removal target in the same stable event-row order used by the
    // historical queue worker. The saved parent hashes are enqueued only after
    // deletion, preserving the shared event -> queue order.
    let rows = txn
        .query(
            "SELECT e.btc_parent_header_hash \
             FROM merge_mining_event e \
             WHERE e.source_id = $1 \
               AND NOT EXISTS ( \
                    SELECT 1 FROM historical_event_provenance error_provenance \
                    WHERE error_provenance.event_id = e.id \
                      AND error_provenance.artifact_scope = 'error-block-observations' \
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

    let removed = txn
        .execute(
            "DELETE FROM merge_mining_event e \
             WHERE e.source_id = $1 \
               AND NOT EXISTS ( \
                    SELECT 1 FROM historical_event_provenance error_provenance \
                    WHERE error_provenance.event_id = e.id \
                      AND error_provenance.artifact_scope = 'error-block-observations' \
               ) \
               AND NOT EXISTS ( \
                    SELECT 1 FROM historical_event_provenance p \
                    WHERE p.event_id = e.id \
                      AND p.publication_ref = $2 \
                      AND p.chain = $3 \
               )",
            &[&source_id, &publication_ref, &chain],
        )
        .await
        .context("remove events absent from authoritative historical snapshot")?;
    for hash in parent_hashes {
        enqueue_historical_parent(txn, &hash).await?;
    }
    Ok(removed)
}

/// Replace the manifest-backed provenance view for one complete authoritative
/// snapshot.
///
/// Every prior pinned publication for the chain is superseded; additive
/// `operator-csv` provenance remains independent. The delete and all
/// replacement provenance rows share the caller's chain transaction, so a
/// failed import restores the previous snapshot intact.
pub async fn clear_authoritative_historical_provenance_in_transaction(
    txn: &Transaction<'_>,
    chain: &str,
) -> Result<()> {
    txn.execute(
        "DELETE FROM historical_event_provenance \
         WHERE chain = $1 \
           AND publication_ref <> 'operator-csv' \
           AND artifact_scope <> 'error-block-observations'",
        &[&chain],
    )
    .await
    .context("clear prior authoritative historical provenance snapshot")?;
    Ok(())
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

/// Capture one merge-mining event: the shared per-block transactional sequence
/// for every producer.
///
/// Run the bounded retry loop: begin a transaction, take the shared Core-view
/// barrier, classify the parent (which may update `payload`), acquire the
/// payload's read-model lock set plus the parent-hash lock, open the
/// source-health bracket, perform the chain-specific `upsert`, reconcile the
/// event in-transaction unless the parent is `near`, close the bracket, and
/// commit. Dependents are cascaded after commit. Holding the shared barrier
/// from classification through commit prevents a suffix switch from landing
/// between those two points.
///
/// The classified `payload` is passed INTO the callback rather than captured at
/// the call site: this function holds the only `&mut` borrow of `payload` (it
/// needs it for [`super::classify_payload_parent`]), so a callback that also
/// captured the payload would create overlapping borrows at the call site.
/// Chain-specific extras (RSK's `rsk_merge_mining_evidence` sidecar) are
/// captured by the callback and never appear in this signature.
///
/// `upsert` is [`AsyncFn`] (not `AsyncFnOnce`): the retry loop may invoke it
/// more than once.
async fn capture_event<F>(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    payload: &mut MergeMiningEventPayload,
    chain_label: &str,
    upsert: F,
    options: CaptureEventOptions,
) -> Result<EventWriteOutcome>
where
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<EventWriteOutcome>,
{
    let supplied_preclassification = options.parent_classification;
    let mut attempts = 0;
    let (outcome, changed_hashes) = loop {
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {chain_label} capture transaction"))?;
        lock_core_classification_view_shared(&txn, None).await?;
        let preclassified = match &supplied_preclassification {
            Some(supplied) if classifier.is_enabled() => {
                let current = classify_payload_parent(&txn, payload, classifier).await?;
                if current.as_ref() != Some(supplied) {
                    bail!(
                        "preclassified parent verdict changed before {chain_label} capture acquired the Core-view barrier"
                    );
                }
                current
            }
            Some(supplied) => {
                apply_classification_proof(payload, supplied.to_proof())?;
                Some(supplied.clone())
            }
            None => classify_payload_parent(&txn, payload, classifier).await?,
        };
        lock_payload_parent_read_model_in_txn(&txn, payload, preclassified.as_ref()).await?;
        // Ensure the parent is locked even for near / target-failing payloads (the
        // helper above no-ops for those), then open the source-health bracket
        // BEFORE the upsert. The wrapper owns the primary parent diff for ALL
        // kinds; near skips reconcile, and the injected upsert may also un-revoke
        // the event in-txn (Hathor), which the bracket captures because it
        // brackets the whole callback.
        lock_block_hash(&txn, &payload.btc_parent_header_hash).await?;
        let bracket =
            PrimarySourceHealthBracket::open(&txn, &payload.btc_parent_header_hash).await?;
        let outcome = upsert(&txn, source_id, payload).await?;
        let reconcile_result = if payload.btc_parent_kind != ParentKind::Near {
            reconcile_one_event_in_txn(
                &txn,
                outcome.event_id,
                classifier,
                preclassified.clone().map(PreclassifiedParent::trusted),
                PrimaryDiff::Wrapper(&bracket),
                None,
            )
            .await
        } else {
            Ok(Vec::new())
        };
        match reconcile_result {
            Ok(changed_hashes) => {
                bracket.close(&txn).await?;
                txn.commit()
                    .await
                    .with_context(|| format!("commit {chain_label} capture transaction"))?;
                break (outcome, changed_hashes);
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
    cascade_changed(client, classifier, changed_hashes, DEFAULT_CASCADE_BUDGET).await?;
    Ok(outcome)
}

/// The two directions of `set_event_revocation`.
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

    /// The `revoked_at IS NOT NULL` state this change drives toward.
    fn desired_revoked(&self) -> bool {
        matches!(self, Self::Revoke { .. })
    }
}

/// Revoke or restore one merge-mining event, keeping the event mutation and
/// its parent read-model reconcile in one transaction.
///
/// Idempotent: changing an event already in the desired state is a no-op
/// `Ok(())`. A concurrent flip between the UPDATE and the state re-check
/// consumes a retry attempt.
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
    let changed_hashes = loop {
        let retry_available = attempt + 1 < retry_attempts();
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {op} reconcile transaction"))?;
        lock_core_classification_view_shared(&txn, None).await?;
        let attempt_preclassified = preclassify_event_parent(&txn, event_id, classifier).await?;
        // Pre-acquire the reconcile lock set and open the source-health bracket
        // BEFORE the revoked_at UPDATE (the membership change). This wrapper
        // owns the primary diff.
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
                txn.commit()
                    .await
                    .with_context(|| format!("commit {op} reconcile"))?;
                break hashes;
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
    cascade_changed(client, classifier, changed_hashes, DEFAULT_CASCADE_BUDGET).await
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

/// Apply a set of base-evidence UPDATEs to one parent's events and reconcile
/// that parent's read model in the same transaction (the reclassify-pools
/// path).
///
/// `mutate` runs inside the transaction and must be re-invocable: the bounded
/// retry loop may run it again after a lock-set-change rollback. When
/// `reconcile_anchor` is `Some(event_id)`, that event anchors the parent
/// read-model rebuild (any active event of the parent works, they share
/// `btc_parent_header_hash`); the reconcile owns the primary source-health
/// diff, exactly like the bulk-repair paths, because base-evidence updates
/// that need this path (pool attribution rows) never move source_health. When
/// `None`, the UPDATEs commit without a reconcile (and therefore without a
/// cascade).
///
/// Lock ordering matches revoke/restore: the advisory locks (the anchor
/// event's full reconcile lock set, or the bare parent hash when there is no
/// reconcile) are acquired BEFORE `mutate` takes any event row locks, so a
/// concurrent producer holding the parent advisory lock can never deadlock
/// against this path's row locks.
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
    let changed_hashes = loop {
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {label} transaction"))?;
        if reconcile_anchor.is_some() {
            lock_core_classification_view_shared(&txn, None).await?;
        } else {
            lock_core_canonical_view_shared(&txn).await?;
        }
        // Advisory locks first, row locks second (the global order every
        // capture/revoke/restore transaction uses). For an anchored reconcile,
        // pre-acquire the anchor's full reconcile lock set so the later
        // in-reconcile acquisition is a re-entrant subset.
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
                txn.commit()
                    .await
                    .with_context(|| format!("commit {label} transaction"))?;
                break changed_hashes;
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
    cascade_changed(client, classifier, changed_hashes, DEFAULT_CASCADE_BUDGET)
        .await
        .with_context(|| {
            format!(
                "reconcile dependents for parent {}",
                hex::encode(parent_hash)
            )
        })
}

/// One Bitcoin Core-attested canonical block to persist through
/// `write_core_canonical`: the header + height define the parent row;
/// `coinbase`, when present, drives the monotonic `btc_coinbase_status`
/// advance toward `complete`.
pub struct CoreCanonicalWrite<'a> {
    pub header: &'a Header,
    pub height: i32,
    pub coinbase: Option<BitcoinCoreBlockCoinbase>,
}

/// Run one caller-supplied operation while holding the exclusive
/// canonical-view barrier through commit. Ordinary Core writers use this for
/// their final source/topology validation, row mutation, and cursor bookkeeping
/// so no shared classifier or competing writer can change the scanned view.
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

/// Write one Bitcoin Core-attested canonical block row (backbone sync and
/// Core-block enrichment), with the parent advisory lock and source-health
/// bracket around the upsert plus the injected in-transaction extra (the
/// backbone's sync-state or coinbase-failure bookkeeping; a no-op elsewhere).
///
/// Returning the token instead of cascading inline keeps dependent work after
/// commit, while `#[must_use]` makes the cascade structurally non-forgettable.
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

/// Write one Core canonical row after validating its source observation while
/// holding the exclusive canonical-view barrier.
/// The validator runs before any row mutation, so a suffix switch that committed
/// after the caller fetched Core evidence cannot be undone by a delayed
/// ordinary writer, and two catch-up writers cannot both fill the same empty
/// height from different source views.
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
        bracket.close(txn).await
    })
    .await?;
    Ok(CommittedParentMutation {
        changed_hashes: vec![hash_bytes],
    })
}

/// Locally revoke one merge-mining event and reconcile its parent read model
/// in the same transaction. See `set_event_revocation`.
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

/// Restore one revoked merge-mining event and reconcile its parent read model
/// in the same transaction. See `set_event_revocation`.
pub async fn restore_merge_mining_event(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    set_event_revocation(client, event_id, RevocationChange::Restore, classifier).await
}

/// Mark a `block` row's Core coinbase fetch as failed, monotonically.
///
/// The `btc_coinbase_status <> 'complete'` guard makes this advisory: it will
/// never demote a row that already reached `complete`, so a late failure record
/// from a retried fetch cannot regress a good coinbase. Injected as the backbone's
/// `in_txn_extra` inside `write_core_canonical` (and a no-op for every other
/// canonical write), so the failure status lands in the same transaction as the
/// header upsert under the parent advisory lock. Detailed failure text stays in
/// `bitcoin_core_sync_state`, which is the sync-level diagnostics table.
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
