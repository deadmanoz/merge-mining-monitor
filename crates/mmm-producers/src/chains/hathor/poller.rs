//! The Hathor live poller: the [`ChainPoller`] impl that drives
//! [`process_hathor_height`] each tick and maps its outcomes to the driver's
//! progress. Split out of `capture` to keep the state-machine file within the
//! size gate.

use anyhow::Result;
use tokio_postgres::Client;
use tracing::warn;

use crate::chains::hathor::capture::{
    HathorCaptureContext, HathorHeightOutcome, process_hathor_height,
};
use crate::chains::hathor::rpc::HathorRpcClient;
use crate::chains::spec::{ChainId, by_id};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress};
use mmm_store::upsert_pending_reconcile;

/// Hathor live capture chain. Maps the rich [`HathorHeightOutcome`] to the
/// driver's [`HeightProgress`]: the table-horizon hold is cursor-blocking
/// (`Abort`); best-effort holds enqueue a durable reconcile row and `Hold`.
pub(crate) struct HathorChainPoller {
    state: ChainPollerState,
    rpc: HathorRpcClient,
    context: HathorCaptureContext,
}

impl HathorChainPoller {
    /// Bundle the owned DB client, REST client, and capture context the poller
    /// driver borrows each tick.
    pub(crate) fn new(client: Client, rpc: HathorRpcClient, context: HathorCaptureContext) -> Self {
        Self {
            state: ChainPollerState::new(by_id(ChainId::Hathor), context.source_id(), client),
            rpc,
            context,
        }
    }
}

impl ChainPoller for HathorChainPoller {
    fn poller_state(&self) -> &ChainPollerState {
        &self.state
    }

    fn client_mut(&mut self) -> &mut Client {
        &mut self.state.client
    }

    async fn chain_tip(&self) -> Result<i32> {
        self.rpc.get_chain_tip().await
    }

    async fn refresh_core_cache(&mut self) -> Result<()> {
        self.context
            .refresh_core_header_cache(&mut self.state.client)
            .await?;
        Ok(())
    }

    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let mut outcome =
            process_hathor_height(&mut self.state.client, &self.rpc, &self.context, height).await?;
        if matches!(outcome, HathorHeightOutcome::TableHorizonHold) {
            match self
                .context
                .refresh_core_header_cache(&mut self.state.client)
                .await
            {
                Ok(()) => {
                    outcome = process_hathor_height(
                        &mut self.state.client,
                        &self.rpc,
                        &self.context,
                        height,
                    )
                    .await?;
                }
                Err(error) => warn!(
                    height,
                    error = %error,
                    "failed to refresh the Core header cache after a Hathor horizon hold"
                ),
            }
        }
        Ok(match outcome {
            HathorHeightOutcome::TableHorizonHold => HeightProgress::Abort,
            HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => {
                // Best-effort hold: enqueue a durable reconcile row so a replay
                // hold (dropped by the replay sub-range) is still retried via the
                // drain, regardless of which sub-range surfaced it. For a new-tip
                // height the cursor is gated by the new sub-range's break on Hold
                // and the row re-enqueues each tick until the height resolves, so
                // the queue's aging-out is what matters mainly for replay holds,
                // which the replay range otherwise drops without blocking.
                upsert_pending_reconcile(
                    &self.state.client,
                    self.context.source_id(),
                    height,
                    Some("hathor_hold"),
                )
                .await?;
                HeightProgress::Hold
            }
            _ => HeightProgress::Advance,
        })
    }

    async fn drain_pending(&mut self) -> Result<()> {
        crate::chains::hathor::drain::drain_pending(
            &mut self.state.client,
            &self.rpc,
            &self.context,
        )
        .await
    }
}
