//! Mainnet endpoint checks and failed-height retention for shared family
//! capture.

use anyhow::{Context, Result, ensure};
use bitcoin::BlockHash;

use crate::chains::bitcoind_rpc::BitcoindRpc;
use crate::chains::spec::{
    CaptureFailurePolicy, FamilySpec, FetchStrategy, RawBlockAuthentication,
};

/// Refuse an endpoint whose genesis block is not the chain's. Reachable from
/// the integration tests (cfg-gated re-export) so the one genesis call a poller
/// or backfill makes at startup can be counted at the transport boundary.
pub async fn ensure_mainnet_endpoint(
    rpc: &impl BitcoindRpc,
    family: &'static FamilySpec,
) -> Result<()> {
    let genesis = match family.fetch {
        FetchStrategy::QbitExtendedHeader { genesis_block_hash }
        | FetchStrategy::RawBlock {
            authentication:
                RawBlockAuthentication::StrictClassic {
                    genesis_block_hash, ..
                },
        } => genesis_block_hash,
        _ => return Ok(()),
    };
    let actual = rpc
        .get_block_hash(0)
        .await
        .context("authenticate mainnet genesis")?;
    ensure_genesis(family.label, &actual, genesis)
}

pub(super) fn ensure_genesis(label: &str, actual: &BlockHash, genesis: &str) -> Result<()> {
    ensure!(
        actual.to_string() == genesis,
        "{} endpoint height 0 is {actual} but mainnet genesis is {genesis}; refusing to capture from a non-mainnet node",
        label
    );
    Ok(())
}

/// Preserve failures during replay as well as new work: under
/// [`CaptureFailurePolicy::HoldAnyHeightFailure`] a failed height records a
/// capture error that only a later successful processing of the same height
/// clears. Do not persist transport error strings, which can contain endpoint
/// credentials. The caller always gets the original error back; a failure to
/// record the capture error is attached to it as context rather than replacing
/// it.
pub(super) async fn retain_failed_height<T>(
    client: &tokio_postgres::Client,
    context: &super::AuxpowCaptureContext,
    height: i32,
    result: Result<T>,
) -> Result<T> {
    let error = match result {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    if context.family().failure_policy != CaptureFailurePolicy::HoldAnyHeightFailure {
        return Err(error);
    }
    let recorded = async {
        mmm_store::record_capture_error(
            client,
            context.source_id(),
            height,
            None,
            mmm_store::CAPTURE_ERROR_HEIGHT_CAPTURE_FAILED,
            Some("Height capture failed; inspect producer logs and replay this height"),
            mmm_capture::capture::now_epoch_seconds()?,
        )
        .await
    }
    .await;
    match recorded {
        Ok(()) => Err(error),
        Err(record_error) => Err(error.context(format!(
            "height {height} failed and its capture error could not be recorded: {record_error:#}"
        ))),
    }
}
