//! The durable `poll_pending_reconcile` drain for the Hathor producer: a
//! best-effort per-tick retry of held heights. Split out of `capture` to keep
//! the per-height state-machine file within the size gate.

use anyhow::Result;
use tokio_postgres::Client;
use tracing::warn;

use crate::chains::hathor::capture::{
    HathorCaptureContext, HathorHeightOutcome, process_hathor_height,
};
use crate::chains::hathor::rpc::HathorRpc;
use mmm_store::{
    PendingReconcileRow, bump_pending_attempts, delete_pending_reconcile, list_pending_reconcile,
};

/// Cap on best-effort reconcile retries before a stuck height ages out loudly,
/// so a permanent transient failure never starves the live tip.
const MAX_PENDING_ATTEMPTS: i32 = 20;

/// Drain the durable `poll_pending_reconcile` queue for this source (best-effort,
/// each tick): every row is a held height that is re-run.
pub(crate) async fn drain_pending(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
) -> Result<()> {
    let pending = list_pending_reconcile(client, context.source_id()).await?;
    for row in pending {
        // Best-effort per row: a single poisoned row (a height that hard-errors
        // in process_hathor_height) must not abort the drain of the others. The
        // row stays queued and is retried next tick.
        if let Err(err) = drain_reconcile(client, rpc, context, &row).await {
            warn!(
                row_id = row.id,
                height = row.height,
                error = %err,
                "draining a pending row failed; continuing with the rest"
            );
        }
    }
    Ok(())
}

/// Re-run a held height. A cursor-blocking horizon hold is left queued (it clears
/// when a later command refreshes the Core cache); a best-effort hold bumps the
/// attempt count and ages the row out past [`MAX_PENDING_ATTEMPTS`]; any definitive
/// resolution deletes the row.
async fn drain_reconcile(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    row: &PendingReconcileRow,
) -> Result<()> {
    let outcome = process_hathor_height(client, rpc, context, row.height).await?;
    match outcome {
        // A cursor-blocking horizon hold is not aged out; it persists until the
        // height resolves after a later command refreshes the Core cache.
        HathorHeightOutcome::TableHorizonHold => Ok(()),
        // Still a best-effort hold: bump attempts; age out loudly past the cap so
        // a permanently-stuck rescan never accumulates forever.
        HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => {
            let attempts = bump_pending_attempts(client, row.id).await?;
            if attempts >= MAX_PENDING_ATTEMPTS {
                warn!(
                    height = row.height,
                    attempts, "aging out a stuck Hathor reconcile hold"
                );
                delete_pending_reconcile(client, row.id).await?;
            }
            Ok(())
        }
        // Any definitive resolution (written or skipped) clears the row.
        _ => delete_pending_reconcile(client, row.id).await,
    }
}
