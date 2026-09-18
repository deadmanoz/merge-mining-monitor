//! Optional mainnet and classic proof authentication for shared family capture.

use anyhow::{Context, Result, ensure};
use bitcoin::BlockHash;
use mmm_capture::auxpow::{
    ParsedNamecoinBlock, parse_namecoin_block, parse_verified_classic_block,
};

use crate::chains::bitcoind_rpc::BitcoindRpc;
use crate::chains::spec::{
    CaptureFailurePolicy, FamilySpec, FetchStrategy, RawBlockAuthentication,
};

pub(super) async fn ensure_mainnet_endpoint(
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

pub(super) fn parse_raw_candidate(
    raw: &[u8],
    family: &FamilySpec,
    requested_hash: &BlockHash,
    height: i32,
) -> Result<ParsedNamecoinBlock> {
    match family.fetch {
        FetchStrategy::RawBlock {
            authentication: RawBlockAuthentication::StrictClassic { chain_id, .. },
        } => parse_verified_classic_block(raw, *requested_hash, height, chain_id),
        _ => parse_namecoin_block(raw),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn genesis_guard_rejects_another_network() {
        let hash: BlockHash = "00000000804bbc6a621a9dbb564ce469f492e1ccf2d70f8a6b241e26a277afa2"
            .parse()
            .unwrap();
        ensure_genesis("Terracoin", &hash, &hash.to_string()).unwrap();
        assert!(ensure_genesis("Terracoin", &hash, "wrong").is_err());
    }
}
