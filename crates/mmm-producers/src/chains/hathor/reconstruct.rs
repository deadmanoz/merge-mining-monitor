//! Hathor RFC-0006 BTC-parent reconstruction helpers.

use std::str::FromStr;

use anyhow::Result;
use bitcoin::BlockHash;
use tracing::{debug, error};

use crate::chains::hathor::auxpow::{HathorReconstruction, reconstruct_from_blobs};
use crate::chains::hathor::rpc::HathorTransaction;
use mmm_capture::auxpow::pow_validates_target;

pub(crate) struct HathorReconstructedParent {
    pub(crate) raw: Vec<u8>,
    pub(crate) aux_pow: Vec<u8>,
    pub(crate) recon: HathorReconstruction,
}

struct HathorReconstructionInputs {
    raw: Vec<u8>,
    aux_pow: Vec<u8>,
    expected: BlockHash,
}

/// Decode + reconstruct the BTC parent from a version-3 transaction. Returns
/// `None` for a malformed proof (the caller maps to `MalformedSkipped`).
pub(crate) fn reconstruct_or_skip(
    height: i32,
    tx: &HathorTransaction,
) -> Result<Option<HathorReconstructedParent>> {
    let Some(inputs) = decode_reconstruction_inputs(height, tx)? else {
        return Ok(None);
    };
    let Some(recon) =
        reconstruct_valid_btc_parent(height, &inputs.raw, &inputs.aux_pow, inputs.expected)?
    else {
        return Ok(None);
    };

    Ok(Some(HathorReconstructedParent {
        raw: inputs.raw,
        aux_pow: inputs.aux_pow,
        recon,
    }))
}

fn decode_reconstruction_inputs(
    height: i32,
    tx: &HathorTransaction,
) -> Result<Option<HathorReconstructionInputs>> {
    let Some(aux_hex) = tx.aux_pow.as_ref() else {
        error!(height, "Hathor version-3 block has no aux_pow; skipping");
        return Ok(None);
    };
    let (Ok(raw), Ok(aux_pow)) = (hex::decode(&tx.raw), hex::decode(aux_hex)) else {
        error!(height, "Hathor raw/aux_pow is not valid hex; skipping");
        return Ok(None);
    };
    let Ok(expected) = BlockHash::from_str(&tx.hash) else {
        error!(height, "Hathor tx.hash is not a valid block hash; skipping");
        return Ok(None);
    };

    Ok(Some(HathorReconstructionInputs {
        raw,
        aux_pow,
        expected,
    }))
}

fn reconstruct_valid_btc_parent(
    height: i32,
    raw: &[u8],
    aux_pow: &[u8],
    expected: BlockHash,
) -> Result<Option<HathorReconstruction>> {
    match reconstruct_from_blobs(raw, aux_pow, expected) {
        Ok((_aux, recon)) => {
            if !pow_validates_target(&recon.header) {
                // The common case: the embedded BTC header only met Hathor's
                // (easier) target, not BTC's, so it is a `near` template, not a
                // real BTC block. Expected for ~all merge-mined blocks; debug,
                // not error, so a healthy poller does not flood the log.
                debug!(
                    height,
                    "reconstructed Hathor parent fails its own PoW target; skipping (near)"
                );
                return Ok(None);
            }
            Ok(Some(recon))
        }
        Err(err) => {
            error!(height, error = %err, "Hathor reconstruction failed; skipping");
            Ok(None)
        }
    }
}
