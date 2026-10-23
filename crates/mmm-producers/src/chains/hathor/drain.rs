//! Durable `poll_pending_reconcile` drain for Hathor held heights.

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

const MAX_PENDING_ATTEMPTS: i32 = 20;

pub(crate) async fn drain_pending(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
) -> Result<()> {
    let pending = list_pending_reconcile(client, context.source_id()).await?;
    let live = context.parent_classifier().is_live();
    let limit = if live { 25 } else { pending.len() };
    let mut unresolved = pending.len() > limit;
    for row in pending.into_iter().take(limit) {
        match drain_reconcile(client, rpc, context, &row).await {
            Ok(settled) => unresolved |= !settled,
            Err(err) => {
                if crate::live_loop::must_propagate(&err) {
                    return Err(err);
                }
                unresolved = true;
                warn!(
                    row_id = row.id,
                    height = row.height,
                    error = %err,
                    "draining a pending row failed; continuing with the rest"
                );
            }
        }
    }
    if live && unresolved {
        return Err(crate::bitcoin_epoch_cache::LocalRecoveryPending("capture_pending").into());
    }
    Ok(())
}

async fn drain_reconcile(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    row: &PendingReconcileRow,
) -> Result<bool> {
    let outcome = process_hathor_height(client, rpc, context, row.height).await?;
    match outcome {
        HathorHeightOutcome::LineagePending => Ok(false),
        HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => {
            let attempts = bump_pending_attempts(client, row.id).await?;
            if attempts >= MAX_PENDING_ATTEMPTS {
                warn!(
                    height = row.height,
                    attempts, "aging out a stuck Hathor reconcile hold"
                );
                delete_pending_reconcile(client, row.id).await?;
            }
            Ok(attempts >= MAX_PENDING_ATTEMPTS)
        }
        _ => delete_pending_reconcile(client, row.id)
            .await
            .map(|()| true),
    }
}
