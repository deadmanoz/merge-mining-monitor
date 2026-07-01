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

use super::{
    CoreCoinbaseStatus, DEFAULT_CASCADE_BUDGET, PreclassifiedParent,
    RECONCILE_LOCK_SET_RETRY_LIMIT, classify_payload_parent, is_reconcile_lock_set_changed,
    load_event, lock_event_for_source_health, lock_parent_hash_in_txn,
    lock_payload_parent_read_model_in_txn, preclassify_event_parent,
    reconcile_dependents_after_changes_with_budget, reconcile_one_event_in_txn,
    upsert_core_canonical_header_with_coinbase,
};

fn retry_attempts() -> usize {
    RECONCILE_LOCK_SET_RETRY_LIMIT.max(1)
}

/// Internal capture knob for an optional pre-decided parent classification.
/// `Some` is the preclassified path
/// (`capture_preclassified_in_txn`), where the caller already ran the Core
/// verdict to gate the write; `None` lets `capture_event` classify the parent
/// itself (the ordinary live-producer path via `capture_in_txn`).
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
    /// A wrapper opened a bracket BEFORE its own base mutation and owns the
    /// primary diff; the reconcile must not diff the primary hash (it would
    /// double-apply or use a post-mutation `before`). The reconcile still owns
    /// the synthesized predecessor/competitor diffs, whose hashes never
    /// overlap the wrapper-owned primary.
    Wrapper(&'a PrimarySourceHealthBracket),
}

/// A committed parent-level mutation whose dependent reconcile cascade has not
/// run yet. Returned by [`write_core_canonical`] so callers can interleave
/// their own post-commit bookkeeping (the backbone's sync-state updates)
/// before cascading; everything else in this module cascades inline.
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
    if changed_hashes.is_empty() {
        return Ok(());
    }
    reconcile_dependents_after_changes_with_budget(
        client,
        &changed_hashes,
        classifier,
        cascade_budget,
    )
    .await
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
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<i64>,
{
    capture_event(
        client,
        source_id,
        classifier,
        payload,
        chain_label,
        upsert,
        CaptureEventOptions::default(),
    )
    .await
}

/// [`capture_in_txn`] variant for callers that already had to classify the
/// parent before deciding whether a write is allowed.
///
/// This keeps the gating decision and the transactional write on the same Core
/// verdict. It is intentionally narrow; ordinary live producers should keep
/// using [`capture_in_txn`] so the mutation module owns their preclassification.
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
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<i64>,
{
    capture_event(
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
    .await
}

/// Capture one merge-mining event: the shared per-block transactional sequence
/// for every producer.
///
/// Preclassify the parent (which may update `payload`), then run the bounded
/// retry loop: begin transaction, acquire the payload's read-model lock set
/// plus the parent-hash lock, open the source-health bracket, perform the
/// chain-specific `upsert`, reconcile the event in-transaction unless the
/// parent is `near`, close the bracket, and commit, rolling back and retrying
/// on a lock-set change. Dependents are cascaded after commit.
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
) -> Result<i64>
where
    F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<i64>,
{
    let preclassified = match options.parent_classification {
        Some(classification) => {
            apply_classification_proof(payload, classification.to_proof())?;
            Some(classification)
        }
        None => classify_payload_parent(client, payload, classifier).await?,
    };
    let mut attempts = 0;
    let (event_id, changed_hashes) = loop {
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {chain_label} capture transaction"))?;
        lock_payload_parent_read_model_in_txn(&txn, payload, preclassified.as_ref()).await?;
        // Ensure the parent is locked even for near / target-failing payloads (the
        // helper above no-ops for those), then open the source-health bracket
        // BEFORE the upsert. The wrapper owns the primary parent diff for ALL
        // kinds; near skips reconcile, and the injected upsert may also un-revoke
        // the event in-txn (Hathor), which the bracket captures because it
        // brackets the whole callback.
        lock_parent_hash_in_txn(&txn, &payload.btc_parent_header_hash).await?;
        let bracket =
            PrimarySourceHealthBracket::open(&txn, &payload.btc_parent_header_hash).await?;
        let event_id = upsert(&txn, source_id, payload).await?;
        let reconcile_result = if payload.btc_parent_kind != ParentKind::Near {
            reconcile_one_event_in_txn(
                &txn,
                event_id,
                classifier,
                preclassified.clone().map(PreclassifiedParent::trusted),
                PrimaryDiff::Wrapper(&bracket),
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
                break (event_id, changed_hashes);
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
    Ok(event_id)
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
        let attempt_preclassified = preclassify_event_parent(client, event_id, classifier).await?;
        let txn = client
            .transaction()
            .await
            .with_context(|| format!("begin {op} reconcile transaction"))?;
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
        // Advisory locks first, row locks second (the global order every
        // capture/revoke/restore transaction uses). For an anchored reconcile,
        // pre-acquire the anchor's full reconcile lock set so the later
        // in-reconcile acquisition is a re-entrant subset.
        match reconcile_anchor {
            Some(anchor_event_id) => {
                let event = load_event(&txn, anchor_event_id).await?;
                lock_event_for_source_health(&txn, &event, classifier, None).await?;
            }
            None => lock_parent_hash_in_txn(&txn, parent_hash).await?,
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

/// Write one Bitcoin Core-attested canonical block row (backbone sync and
/// Core-block enrichment), with the parent advisory lock and source-health
/// bracket around the upsert plus the injected in-transaction extra (the
/// backbone's coinbase-failure column update; a no-op elsewhere).
///
/// Returning the token (instead of cascading inline) lets the backbone interleave
/// its sync-state bookkeeping between commit and cascade exactly as before,
/// while `#[must_use]` keeps the cascade structurally non-forgettable.
pub async fn write_core_canonical<F>(
    client: &mut Client,
    write: CoreCanonicalWrite<'_>,
    in_txn_extra: F,
    label: &str,
) -> Result<CommittedParentMutation>
where
    F: AsyncFnOnce(&Transaction<'_>) -> Result<()>,
{
    let hash_bytes = write.header.block_hash().to_byte_array().to_vec();
    let txn = client
        .transaction()
        .await
        .with_context(|| format!("begin {label} transaction"))?;
    lock_parent_hash_in_txn(&txn, &hash_bytes).await?;
    let bracket = PrimarySourceHealthBracket::open(&txn, &hash_bytes).await?;
    upsert_core_canonical_header_with_coinbase(&txn, write.header, write.height, write.coinbase)
        .await?;
    in_txn_extra(&txn).await?;
    bracket.close(&txn).await?;
    txn.commit()
        .await
        .with_context(|| format!("commit {label} transaction"))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocation_change_direction_and_labels() {
        let revoke = RevocationChange::Revoke {
            reason: "x".to_owned(),
        };
        assert_eq!(revoke.op(), "revoke");
        assert!(revoke.desired_revoked());
        assert_eq!(RevocationChange::Restore.op(), "restore");
        assert!(!RevocationChange::Restore.desired_revoked());
    }
}
