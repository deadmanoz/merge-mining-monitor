//! The poll_pending_reconcile work queue plus revocation-reason retag.

use anyhow::{Context, Result};
use tokio_postgres::{Client, GenericClient};

/// Retag a same-height row already revoked with `from_reason` to `to_reason`
/// WITHOUT changing its revoked status (so no read-model reconcile is needed) and
/// without touching active rows, manual revokes, or already-`to_reason` rows. The
/// Elastos classifier-conflict path uses this to make a previously reversible
/// (`elastos_non_btc`) revocation sticky, so a later Valid recapture cannot clear
/// it and restore a classifier-rejected block. Returns the number of rows retagged.
pub async fn retag_revocation_reason(
    client: &Client,
    source_id: i64,
    height: i32,
    from_reason: &str,
    to_reason: &str,
) -> Result<u64> {
    client
        .execute(
            "UPDATE merge_mining_event SET revocation_reason = $4 \
              WHERE source_id = $1 AND child_height = $2 \
                AND revoked_at IS NOT NULL AND revocation_reason = $3",
            &[&source_id, &height, &from_reason, &to_reason],
        )
        .await
        .context("retag revocation reason")
}

/// A durable `poll_pending_reconcile` work item drained by the poller. `kind`
/// selects the drain path (`reconcile` re-runs the height, `supersede` completes
/// a capture-then-revoke); `new_child_block_hash` / `superseded_event_ids` carry
/// the supersede payload and are NULL for plain reconcile rows.
#[derive(Debug, Clone)]
pub struct PendingReconcileRow {
    pub id: i64,
    pub height: i32,
    pub kind: String,
    pub new_child_block_hash: Option<Vec<u8>>,
    pub superseded_event_ids: Option<Vec<i64>>,
    pub reason: Option<String>,
    pub attempts: i32,
}

/// Enqueue (or refresh) a pending work item, idempotent on (source, height, kind).
pub async fn upsert_pending_reconcile<C: GenericClient>(
    client: &C,
    source_id: i64,
    height: i32,
    kind: &str,
    new_child_block_hash: Option<Vec<u8>>,
    superseded_event_ids: Option<Vec<i64>>,
    reason: Option<&str>,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO poll_pending_reconcile ( \
                source_id, height, kind, new_child_block_hash, superseded_event_ids, \
                reason, attempts \
             ) VALUES ($1, $2, $3, $4, $5, $6, 0) \
             ON CONFLICT (source_id, height, kind) DO UPDATE SET \
                new_child_block_hash = EXCLUDED.new_child_block_hash, \
                superseded_event_ids = EXCLUDED.superseded_event_ids, \
                reason = EXCLUDED.reason",
            &[
                &source_id,
                &height,
                &kind,
                &new_child_block_hash,
                &superseded_event_ids,
                &reason,
            ],
        )
        .await
        .context("enqueue poll_pending_reconcile")?;
    Ok(())
}

/// List a source's pending work items ordered by height ascending. The ordering
/// is the contract: the poller drains oldest-height-first so a stuck low height
/// ages out before newer ones.
pub async fn list_pending_reconcile(
    client: &Client,
    source_id: i64,
) -> Result<Vec<PendingReconcileRow>> {
    let rows = client
        .query(
            "SELECT id, height, kind, new_child_block_hash, superseded_event_ids, \
                    reason, attempts \
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
            kind: row.get("kind"),
            new_child_block_hash: row.get("new_child_block_hash"),
            superseded_event_ids: row.get("superseded_event_ids"),
            reason: row.get("reason"),
            attempts: row.get("attempts"),
        })
        .collect())
}

/// Delete a resolved work item by primary key `id`. Drain paths hold the
/// `PendingReconcileRow` and delete by id; capture-time clears that have only the
/// natural key use [`delete_pending_reconcile_at`] instead.
pub async fn delete_pending_reconcile<C: GenericClient>(client: &C, id: i64) -> Result<()> {
    client
        .execute("DELETE FROM poll_pending_reconcile WHERE id = $1", &[&id])
        .await
        .context("delete poll_pending_reconcile")?;
    Ok(())
}

/// Remove a pending work item by its (source, height, kind) key (e.g. clear a
/// `supersede` marker once the supersession is complete).
pub async fn delete_pending_reconcile_at<C: GenericClient>(
    client: &C,
    source_id: i64,
    height: i32,
    kind: &str,
) -> Result<()> {
    client
        .execute(
            "DELETE FROM poll_pending_reconcile \
              WHERE source_id = $1 AND height = $2 AND kind = $3",
            &[&source_id, &height, &kind],
        )
        .await
        .context("delete poll_pending_reconcile by key")?;
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
