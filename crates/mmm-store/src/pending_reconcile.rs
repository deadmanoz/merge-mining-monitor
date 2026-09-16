//! The poll_pending_reconcile queue of held heights plus the revocation-reason
//! retag.

use anyhow::{Context, Result};
use tokio_postgres::{Client, GenericClient};

/// Retag one child block's row (by hash, or a hashless row by parent) already
/// revoked with `from_reason` to `to_reason`
/// WITHOUT changing its revoked status (so no read-model reconcile is needed) and
/// without touching active rows, manual revokes, or already-`to_reason` rows. The
/// Elastos classifier-conflict path uses this to make a previously reversible
/// (`elastos_non_btc`) revocation sticky, so a later Valid recapture cannot clear
/// it and restore a classifier-rejected block. Returns the number of rows retagged.
pub async fn retag_revocation_reason(
    client: &Client,
    source_id: i64,
    height: i32,
    child_block_hash: &[u8],
    btc_parent_header_hash: &[u8],
    from_reason: &str,
    to_reason: &str,
) -> Result<u64> {
    client
        .execute(
            "UPDATE merge_mining_event SET revocation_reason = $6 \
              WHERE source_id = $1 AND child_height = $2 \
                AND (child_block_hash = $3 \
                     OR (child_block_hash IS NULL AND btc_parent_header_hash = $4)) \
                AND revoked_at IS NOT NULL AND revocation_reason = $5",
            &[
                &source_id,
                &height,
                &child_block_hash,
                &btc_parent_header_hash,
                &from_reason,
                &to_reason,
            ],
        )
        .await
        .context("retag revocation reason")
}

/// A durable `poll_pending_reconcile` work item: one held height a producer
/// re-runs each tick until it resolves or ages out. `reason` records why it
/// was held.
#[derive(Debug, Clone)]
pub struct PendingReconcileRow {
    pub id: i64,
    pub height: i32,
    pub reason: Option<String>,
    pub attempts: i32,
}

/// Enqueue (or refresh) a held height, idempotent on (source, height).
pub async fn upsert_pending_reconcile<C: GenericClient>(
    client: &C,
    source_id: i64,
    height: i32,
    reason: Option<&str>,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO poll_pending_reconcile (source_id, height, reason, attempts) \
             VALUES ($1, $2, $3, 0) \
             ON CONFLICT (source_id, height) DO UPDATE SET reason = EXCLUDED.reason",
            &[&source_id, &height, &reason],
        )
        .await
        .context("enqueue poll_pending_reconcile")?;
    Ok(())
}

/// List a source's held heights ordered by height ascending. The ordering is
/// the contract: the poller drains oldest-height-first so a stuck low height
/// ages out before newer ones.
pub async fn list_pending_reconcile(
    client: &Client,
    source_id: i64,
) -> Result<Vec<PendingReconcileRow>> {
    let rows = client
        .query(
            "SELECT id, height, reason, attempts \
               FROM poll_pending_reconcile \
              WHERE source_id = $1 \
              ORDER BY height",
            &[&source_id],
        )
        .await
        .context("list poll_pending_reconcile")?;
    Ok(rows
        .iter()
        .map(|row| PendingReconcileRow {
            id: row.get("id"),
            height: row.get("height"),
            reason: row.get("reason"),
            attempts: row.get("attempts"),
        })
        .collect())
}

/// Delete a resolved work item by primary key `id`.
pub async fn delete_pending_reconcile<C: GenericClient>(client: &C, id: i64) -> Result<()> {
    client
        .execute("DELETE FROM poll_pending_reconcile WHERE id = $1", &[&id])
        .await
        .context("delete poll_pending_reconcile")?;
    Ok(())
}

/// Record a failed drain attempt; returns the new attempt count.
pub async fn bump_pending_attempts(client: &Client, id: i64) -> Result<i32> {
    let row = client
        .query_one(
            "UPDATE poll_pending_reconcile \
                SET attempts = attempts + 1 \
              WHERE id = $1 \
          RETURNING attempts",
            &[&id],
        )
        .await
        .context("bump poll_pending_reconcile attempts")?;
    Ok(row.get("attempts"))
}
