//! Bitcoin Core backbone cursor, target, and progress persistence.

use anyhow::{Context, Result, anyhow};
use bitcoin::hashes::Hash as _;
use serde_json::{Value, json};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, GenericClient};

use mmm_read_model::run_exclusive_core_canonical_view_transaction;

use super::{
    BackboneIntegrityError, BackboneIntegrityFailure, BitcoinCoreBackboneSource,
    BitcoinCoreBackboneTip, REPAIR_OWNED_ERROR_CODES, SYNC_MODE_CONTIGUOUS,
    TARGET_TIP_CHANGED_ERROR_CODE, guard_existing_link, target_stability_failure,
};

/// In-memory mirror of the `bitcoin_core_sync_state` cursor row for one batch.
#[derive(Debug, Clone)]
pub(super) struct SyncState {
    pub(super) target_tip_height: Option<i32>,
    pub(super) target_tip_hash: Option<Vec<u8>>,
    pub(super) contiguous_complete_height: i32,
    pub(super) last_error_code: Option<String>,
    pub(super) last_error_height: Option<i32>,
    /// Keep a coinbase error visible when a later height succeeds in the same batch.
    pub(super) preserve_error: bool,
}

/// Minimal canonical-row projection used by the backbone topology walk.
#[derive(Debug, Clone)]
pub(super) struct CanonicalHeightRow {
    pub(super) hash: Vec<u8>,
    pub(super) prev_hash: Vec<u8>,
    pub(super) coinbase_status: String,
}

pub(super) async fn guard_no_pending_core_reconcile<C: GenericClient>(
    client: &C,
    source_id: i64,
) -> Result<()> {
    let pending: bool = client
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM bitcoin_core_reconcile_queue WHERE source_id = $1 \
             )",
            &[&source_id],
        )
        .await
        .context("check pending Bitcoin Core reconcile work before ordinary sync")?
        .get(0);
    if pending {
        anyhow::bail!("Bitcoin Core reconcile queue is pending; defer ordinary sync");
    }
    Ok(())
}

/// Idempotently create or read the contiguous sync-state row.
pub(super) async fn load_or_init_sync_state<C: GenericClient>(
    client: &C,
    source_id: i64,
) -> Result<SyncState> {
    let row = client
        .query_one(
            "INSERT INTO bitcoin_core_sync_state (source_id, sync_mode, created_at, updated_at) \
             VALUES ($1, $2, extract(epoch from now())::bigint, extract(epoch from now())::bigint) \
             ON CONFLICT (source_id, sync_mode) DO UPDATE SET updated_at = bitcoin_core_sync_state.updated_at \
             RETURNING target_tip_height, target_tip_hash, contiguous_complete_height, \
                       last_error_code, last_error_height",
            &[&source_id, &SYNC_MODE_CONTIGUOUS],
        )
        .await
        .context("load Bitcoin Core sync state")?;
    Ok(SyncState {
        target_tip_height: row.get(0),
        target_tip_hash: row.get(1),
        contiguous_complete_height: row.get(2),
        last_error_code: row.get(3),
        last_error_height: row.get(4),
        preserve_error: false,
    })
}

/// Pin or re-confirm a one-shot sync target.
pub(super) async fn verify_or_set_target_tip<C: GenericClient>(
    client: &C,
    source_id: i64,
    state: &mut SyncState,
    tip: BitcoinCoreBackboneTip,
) -> Result<()> {
    let tip_hash = tip.hash.to_byte_array().to_vec();
    if state.target_tip_height == Some(tip.height)
        && let Some(existing_hash) = &state.target_tip_hash
        && existing_hash != &tip_hash
    {
        let message = anyhow!(
            "Bitcoin Core target tip changed at height {}: existing={}, current={}",
            tip.height,
            hex::encode(existing_hash),
            tip.hash
        )
        .to_string();
        let failure = BackboneIntegrityFailure::new(
            BackboneIntegrityError::TargetTipChanged,
            TARGET_TIP_CHANGED_ERROR_CODE,
            tip.height,
            message,
            json!({
                "existing_hash": hex::encode(existing_hash),
                "current_hash": tip.hash.to_string(),
            }),
        );
        failure.persist(client, source_id).await?;
        return Err(failure.into_error());
    }
    client
        .execute(
            "UPDATE bitcoin_core_sync_state \
             SET target_tip_height = $3, target_tip_hash = $4, updated_at = extract(epoch from now())::bigint \
             WHERE source_id = $1 AND sync_mode = $2",
            &[&source_id, &SYNC_MODE_CONTIGUOUS, &tip.height, &tip_hash],
        )
        .await
        .context("store Bitcoin Core sync target tip")?;
    state.target_tip_height = Some(tip.height);
    state.target_tip_hash = Some(tip_hash);
    Ok(())
}

/// Accept a repaired live target only while it still matches Core and no
/// suffix reconciliation remains pending. Clear repair-owned telemetry while
/// preserving an unrelated producer failure.
pub(super) async fn accept_live_repaired_target<S>(
    client: &mut Client,
    source: &S,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    let target_hash = target.hash.to_byte_array().to_vec();
    let result = run_exclusive_core_canonical_view_transaction(
        client,
        "accept verified Bitcoin Core live repair target",
        async |txn| {
            guard_no_pending_core_reconcile(txn, source_id).await?;
            let state_exists = txn
                .query_opt(
                    "SELECT 1 FROM bitcoin_core_sync_state \
                     WHERE source_id = $1 AND sync_mode = $2 \
                     FOR UPDATE",
                    &[&source_id, &SYNC_MODE_CONTIGUOUS],
                )
                .await
                .context("lock Bitcoin Core sync state before accepting live target")?
                .is_some();
            if !state_exists {
                anyhow::bail!("Bitcoin Core sync state is missing while accepting live target");
            }
            if let Some(failure) = target_stability_failure(source, target).await? {
                return Err(failure.into_error());
            }
            txn.execute(
                "UPDATE bitcoin_core_sync_state \
                 SET target_tip_height = $3, target_tip_hash = $4, \
                     last_error_code = CASE \
                         WHEN last_error_code = ANY($5::text[]) THEN NULL \
                         ELSE last_error_code END, \
                     last_error_height = CASE \
                         WHEN last_error_code = ANY($5::text[]) THEN NULL \
                         ELSE last_error_height END, \
                     last_error = CASE \
                         WHEN last_error_code = ANY($5::text[]) THEN NULL \
                         ELSE last_error END, \
                     last_error_details = CASE \
                         WHEN last_error_code = ANY($5::text[]) THEN '{}'::jsonb \
                         ELSE last_error_details END, \
                     updated_at = extract(epoch from now())::bigint \
                 WHERE source_id = $1 AND sync_mode = $2",
                &[
                    &source_id,
                    &SYNC_MODE_CONTIGUOUS,
                    &target.height,
                    &target_hash,
                    &REPAIR_OWNED_ERROR_CODES.as_slice(),
                ],
            )
            .await
            .context("store verified Bitcoin Core live repair target")?;
            Ok(())
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

/// Point lookup of every canonical row at one height.
pub(super) async fn load_canonical_rows_at_height<C: GenericClient>(
    client: &C,
    height: i32,
) -> Result<Vec<CanonicalHeightRow>> {
    let rows = client
        .query(
            "SELECT btc_header_hash, btc_prev_header_hash, btc_coinbase_status \
             FROM block \
             WHERE kind = 'canonical' AND btc_height = $1 \
             ORDER BY btc_header_hash",
            &[&height],
        )
        .await
        .with_context(|| format!("load canonical rows at height {height}"))?;
    Ok(rows
        .into_iter()
        .map(|row| CanonicalHeightRow {
            hash: row.get(0),
            prev_hash: row.get(1),
            coinbase_status: row.get(2),
        })
        .collect())
}

/// Return whether a skipped complete row still has an unresolved dependent.
pub(super) async fn skipped_complete_has_missing_dependents<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<bool> {
    let row = client
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 \
                 FROM merge_mining_event e \
                 LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                 WHERE e.btc_parent_prev_header_hash = $1 \
                   AND e.btc_parent_kind <> 'near' \
                   AND e.pow_validates_btc_target \
                   AND e.revoked_at IS NULL \
                   AND b.btc_header_hash IS NULL \
             )",
            &[&hash],
        )
        .await
        .context("check skipped complete backbone dependents")?;
    Ok(row.get(0))
}

/// Advance the high-water mark over the complete, linked canonical prefix.
pub(super) async fn advance_contiguous_complete_prefix<C: GenericClient>(
    client: &C,
    source_id: i64,
    state: &mut SyncState,
) -> Result<()> {
    let mut anchor_hash = if state.contiguous_complete_height >= 0 {
        let rows = load_canonical_rows_at_height(client, state.contiguous_complete_height).await?;
        if rows.len() != 1 || rows[0].coinbase_status != "complete" {
            return Ok(());
        }
        Some(rows[0].hash.clone())
    } else {
        None
    };

    let mut new_height = state.contiguous_complete_height;
    while let Some(next_height) = new_height.checked_add(1) {
        let rows = load_canonical_rows_at_height(client, next_height).await?;
        let [row] = rows.as_slice() else {
            break;
        };
        if row.coinbase_status != "complete" {
            break;
        }
        if next_height != 0 && anchor_hash.as_deref() != Some(row.prev_hash.as_slice()) {
            break;
        }
        anchor_hash = Some(row.hash.clone());
        new_height = next_height;
    }

    if new_height > state.contiguous_complete_height {
        state.contiguous_complete_height = new_height;
        client
            .execute(
                "UPDATE bitcoin_core_sync_state \
                 SET contiguous_complete_height = GREATEST(contiguous_complete_height, $3), \
                     updated_at = extract(epoch from now())::bigint \
                 WHERE source_id = $1 AND sync_mode = $2",
                &[
                    &source_id,
                    &SYNC_MODE_CONTIGUOUS,
                    &state.contiguous_complete_height,
                ],
            )
            .await
            .context("advance Bitcoin Core contiguous sync cursor")?;
    }
    Ok(())
}

/// Clear a stale link error only after Core confirms the cached row at that
/// height. The sync-state row remains locked by `load_or_init_sync_state`, so a
/// newer concurrent error is written after this transaction and stays visible.
pub(super) async fn clear_resolved_backbone_link_error<C, S>(
    client: &C,
    source: &S,
    source_id: i64,
    state: &SyncState,
) -> Result<()>
where
    C: GenericClient,
    S: BitcoinCoreBackboneSource,
{
    if state.last_error_code.as_deref() != Some("backbone_link_mismatch") {
        return Ok(());
    }
    let Some(error_height) = state.last_error_height else {
        return Ok(());
    };
    if error_height > state.contiguous_complete_height {
        return Ok(());
    }

    let core_hash = source
        .block_hash(error_height)
        .await
        .with_context(|| format!("revalidate resolved Core link error at height {error_height}"))?;
    let rows = load_canonical_rows_at_height(client, error_height).await?;
    let [row] = rows.as_slice() else {
        return Ok(());
    };
    if row.coinbase_status != "complete"
        || row.hash.as_slice() != core_hash.to_byte_array().as_slice()
    {
        return Ok(());
    }
    guard_existing_link(client, source_id, error_height, row, true).await?;

    client
        .execute(
            "UPDATE bitcoin_core_sync_state s \
             SET last_error_code = NULL, last_error_height = NULL, last_error = NULL, \
                 last_error_details = '{}'::jsonb, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE s.source_id = $1 AND s.sync_mode = $2 \
               AND s.last_error_code = 'backbone_link_mismatch' \
               AND s.last_error_height = $3 \
               AND NOT EXISTS ( \
                   SELECT 1 FROM bitcoin_core_reconcile_queue q \
                   WHERE q.source_id = s.source_id \
               )",
            &[&source_id, &SYNC_MODE_CONTIGUOUS, &error_height],
        )
        .await
        .context("clear resolved Bitcoin Core backbone link error")?;
    Ok(())
}

/// Persist one successful height without masking an unrelated or pending error.
pub(super) async fn update_sync_progress<C: GenericClient>(
    client: &C,
    source_id: i64,
    height: i32,
    state: &SyncState,
) -> Result<()> {
    client
        .execute(
            "WITH current AS MATERIALIZED ( \
                 SELECT s.source_id, s.sync_mode, \
                        $5 AND (s.last_error_height IS NULL OR s.last_error_height = $4) \
                           AND NOT EXISTS ( \
                               SELECT 1 FROM bitcoin_core_reconcile_queue q \
                               WHERE q.source_id = s.source_id \
                           ) AS clear_error \
                 FROM bitcoin_core_sync_state s \
                 WHERE s.source_id = $1 AND s.sync_mode = $2 \
                 FOR UPDATE \
             ) \
             UPDATE bitcoin_core_sync_state s \
             SET contiguous_complete_height = GREATEST(s.contiguous_complete_height, $3), \
                 last_scanned_height = $4, \
                 last_attempted_height = $4, \
                 last_error_code = CASE WHEN current.clear_error THEN NULL ELSE s.last_error_code END, \
                 last_error_height = CASE WHEN current.clear_error THEN NULL ELSE s.last_error_height END, \
                 last_error = CASE WHEN current.clear_error THEN NULL ELSE s.last_error END, \
                 last_error_details = CASE WHEN current.clear_error THEN '{}'::jsonb ELSE s.last_error_details END, \
                 updated_at = extract(epoch from now())::bigint \
             FROM current \
             WHERE s.source_id = current.source_id AND s.sync_mode = current.sync_mode",
            &[
                &source_id,
                &SYNC_MODE_CONTIGUOUS,
                &state.contiguous_complete_height,
                &height,
                &!state.preserve_error,
            ],
        )
        .await
        .context("update Bitcoin Core sync progress")?;
    Ok(())
}

/// Record a stable error code, height, message, and structured details.
///
/// A committed suffix replacement owns the visible status until its durable
/// reconcile queue drains. Repair-only telemetry also cannot displace an
/// unrelated producer error because target acceptance could not reconstruct
/// that error after clearing the repair status. These ownership predicates are
/// deliberately on the target row: PostgreSQL rechecks them after a concurrent
/// row-lock wait.
pub(super) async fn update_sync_error<C: GenericClient>(
    client: &C,
    source_id: i64,
    height: i32,
    code: &str,
    message: &str,
    details: Value,
) -> Result<()> {
    client
        .execute(
            "UPDATE bitcoin_core_sync_state \
             SET last_scanned_height = $3, \
                 last_attempted_height = $3, \
                 last_error_code = $4, \
                 last_error_height = $3, \
                 last_error = $5, \
                 last_error_details = $6, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE source_id = $1 AND sync_mode = $2 \
               AND last_error_code IS DISTINCT FROM 'backbone_reorg_reconcile_pending' \
               AND ( \
                   $4 <> ALL($7::text[]) \
                   OR last_error_code IS NULL \
                   OR last_error_code = ANY($7::text[]) \
               )",
            &[
                &source_id,
                &SYNC_MODE_CONTIGUOUS,
                &height,
                &code,
                &message,
                &Json(&details),
                &REPAIR_OWNED_ERROR_CODES.as_slice(),
            ],
        )
        .await
        .context("record Bitcoin Core sync error")?;
    Ok(())
}
