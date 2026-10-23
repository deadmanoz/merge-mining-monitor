//! Mainnet endpoint checks and failed-height retention for shared family capture.

use anyhow::{Context, Result, ensure};
use bitcoin::BlockHash;

use crate::chains::bitcoind_rpc::BitcoindRpc;
use crate::chains::spec::{
    CaptureFailurePolicy, FamilySpec, FetchStrategy, RawBlockAuthentication,
};

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

/// Record a generic `capture_error` (never transport error strings, which can
/// contain endpoint credentials) under [`CaptureFailurePolicy::HoldAnyHeightFailure`].
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
    if crate::live_loop::must_propagate(&error) {
        return Err(error);
    }
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
