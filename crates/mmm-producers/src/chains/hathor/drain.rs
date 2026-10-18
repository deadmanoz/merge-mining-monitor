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
    for row in pending {
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

async fn drain_reconcile(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    row: &PendingReconcileRow,
) -> Result<()> {
    let outcome = process_hathor_height(client, rpc, context, row.height).await?;
    match outcome {
        HathorHeightOutcome::LineagePending => Ok(()),
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
        _ => delete_pending_reconcile(client, row.id).await,
    }
}
