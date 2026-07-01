//! The durable `poll_pending_reconcile` drain for the Hathor producer: best-effort
//! per-tick retry of held heights (`reconcile`) and completion of crash-interrupted
//! write-before-revoke supersessions (`supersede`). Split out of `capture` to keep
//! the per-height state-machine file within the size gate.

use std::collections::HashSet;

use anyhow::{Context, Result};
use tokio_postgres::Client;
use tracing::warn;

use crate::chains::hathor::capture::{
    HathorCaptureContext, HathorHeightOutcome, PENDING_KIND_RECONCILE, PENDING_KIND_SUPERSEDE,
    process_hathor_height,
};
use crate::chains::hathor::rpc::HathorRpc;
use mmm_capture::capture::HATHOR_REVOKE_SUPERSEDED;
use mmm_read_model::revoke_merge_mining_event;
use mmm_store::{
    PendingReconcileRow, bump_pending_attempts, delete_pending_reconcile, hathor_events_at_height,
    list_pending_reconcile,
};

/// Cap on best-effort reconcile retries before a stuck height ages out loudly,
/// so a permanent transient failure never starves the live tip.
const MAX_PENDING_ATTEMPTS: i32 = 20;

/// Drain the durable `poll_pending_reconcile` queue for this source (best-effort,
/// each tick). `reconcile` rows re-run the height; `supersede` rows complete an
/// in-progress capture-then-revoke once the replacement exists.
pub(crate) async fn drain_pending(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
) -> Result<()> {
    let pending = list_pending_reconcile(client, context.source_id()).await?;
    for row in pending {
        // Best-effort per row: a single poisoned row (a failing revoke, or a
        // height that hard-errors in process_hathor_height) must not abort the
        // drain of the others. The row stays queued and is retried next tick.
        let result = match row.kind.as_str() {
            PENDING_KIND_SUPERSEDE => drain_supersede(client, context, &row).await,
            PENDING_KIND_RECONCILE => drain_reconcile(client, rpc, context, &row).await,
            other => {
                warn!(
                    kind = other,
                    "unknown poll_pending_reconcile kind; deleting"
                );
                delete_pending_reconcile(client, row.id).await
            }
        };
        if let Err(err) = result {
            warn!(
                row_id = row.id,
                height = row.height,
                kind = row.kind.as_str(),
                error = %err,
                "draining a pending row failed; continuing with the rest"
            );
        }
    }
    Ok(())
}

/// Re-run a held height. A cursor-blocking horizon hold is left queued (it clears
/// when Bitcoin Core becomes available / catches up and returns a verdict, or an
/// offline run regenerates the nBits table); a best-effort hold bumps the attempt
/// count and ages the row out past [`MAX_PENDING_ATTEMPTS`]; any definitive
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
        // height resolves (Bitcoin Core answers, or an offline run regenerates the
        // nBits table).
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

/// Complete a crash-interrupted write-before-revoke: finish revoking the
/// originally-superseded events ONLY once the replacement event is present and
/// active, never revoking a newer/manual/restored row at the height. If the
/// replacement never landed, drop the marker and let a rescan retry. Always
/// deletes the row at the end.
async fn drain_supersede(
    client: &mut Client,
    context: &HathorCaptureContext,
    row: &PendingReconcileRow,
) -> Result<()> {
    // Defense in depth: complete the revoke ONLY if the replacement event exists
    // and is active; otherwise the supersession never completed, so drop the
    // marker (a rescan re-detects and retries). Never revoke without a live Y.
    if let (Some(new_hash), Some(superseded_ids)) =
        (&row.new_child_block_hash, &row.superseded_event_ids)
    {
        let events = hathor_events_at_height(client, context.source_id(), row.height).await?;
        let replacement_active = events
            .iter()
            .any(|event| event.is_active && &event.child_block_hash == new_hash);
        if replacement_active {
            // Revoke ONLY the originally-captured superseded events that are
            // still active, never a newer/manual/restored event at this height.
            let active_ids: HashSet<i64> = events
                .iter()
                .filter(|e| e.is_active)
                .map(|e| e.event_id)
                .collect();
            for &event_id in superseded_ids {
                if active_ids.contains(&event_id) {
                    revoke_merge_mining_event(
                        client,
                        event_id,
                        HATHOR_REVOKE_SUPERSEDED,
                        context.parent_classifier(),
                    )
                    .await
                    .with_context(|| format!("revoke superseded Hathor event {event_id}"))?;
                }
            }
        }
    }
    delete_pending_reconcile(client, row.id).await
}
