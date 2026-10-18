//! Hathor live [`ChainPoller`] implementation.

use anyhow::Result;
use tokio_postgres::Client;

use crate::chains::hathor::capture::{
    HathorCaptureContext, HathorHeightOutcome, process_hathor_height, rescan_hathor_height,
};
use crate::chains::hathor::rpc::HathorRpcClient;
use crate::chains::spec::{ChainId, by_id};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress, RescanOutcome};
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_store::upsert_pending_reconcile;

pub(crate) struct HathorChainPoller {
    state: ChainPollerState,
    rpc: HathorRpcClient,
    context: HathorCaptureContext,
}

impl HathorChainPoller {
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
    fn chain_rpc_metrics(&self) -> Option<mmm_rpc::RpcMetrics> {
        Some(self.rpc.metrics())
    }
    fn parent_classifier(&self) -> Option<&ConfiguredParentClassifier> {
        Some(self.context.parent_classifier())
    }
    fn non_bitcoin_parents(&self) -> usize {
        self.context.non_bitcoin_parents()
    }

    async fn refresh_core_cache(&mut self) -> Result<()> {
        self.context
            .refresh_core_header_cache(&mut self.state.client)
            .await?;
        Ok(())
    }

    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            process_hathor_height(&mut self.state.client, &self.rpc, &self.context, height).await?;
        self.progress_for(height, outcome).await
    }

    async fn rescan_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            rescan_hathor_height(&mut self.state.client, &self.rpc, &self.context, height).await?;
        match outcome {
            RescanOutcome::Unchanged => Ok(HeightProgress::Advance),
            RescanOutcome::Captured(outcome) => self.progress_for(height, outcome).await,
        }
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

impl HathorChainPoller {
    async fn progress_for(
        &mut self,
        height: i32,
        outcome: HathorHeightOutcome,
    ) -> Result<HeightProgress> {
        Ok(match outcome {
            HathorHeightOutcome::LineagePending => HeightProgress::Hold,
            HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => {
                // Enqueue a durable reconcile row so replay holds dropped by the
                // replay sub-range are still retried via `drain_pending`.
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
}
