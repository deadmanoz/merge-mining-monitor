//! Hathor RFC-0006 BTC-parent reconstruction helpers, and the block's own
//! proof-of-work check.

use std::str::FromStr;

use anyhow::Result;
use bitcoin::BlockHash;
use bitcoin::pow::Target;
use tracing::{debug, error};

use crate::chains::hathor::auxpow::{HathorReconstruction, reconstruct_from_blobs};
use crate::chains::hathor::rpc::HathorTransaction;
use mmm_capture::auxpow::pow_validates_target;

pub(crate) struct HathorReconstructedParent {
    pub(crate) raw: Vec<u8>,
    pub(crate) aux_pow: Vec<u8>,
    pub(crate) recon: HathorReconstruction,
    /// The weight the block declares, see [`declared_weight`].
    pub(crate) weight: f64,
}

/// What a version-3 block's proof establishes.
pub(crate) enum HathorParentReconstruction {
    /// The reconstruction identity holds, the hash meets the block's own
    /// Hathor target, and the parent meets BTC's target: a BTC-difficulty
    /// parent to capture.
    BtcValid(HathorReconstructedParent),
    /// The identity holds and the hash meets the block's own target, but the
    /// parent misses BTC's target: a real Hathor block whose parent is a near
    /// template. The common case for a merge-mined block; no event. Carries
    /// the weight the block declares.
    Near { weight: f64 },
    /// The proof is malformed or inconsistent, or the hash does not meet the
    /// block's own target: nothing trustworthy.
    Malformed,
}

struct HathorReconstructionInputs {
    raw: Vec<u8>,
    aux_pow: Vec<u8>,
    expected: BlockHash,
}

/// Decode + reconstruct the BTC parent from a version-3 transaction, then
/// check the block's own proof of work: the hash must meet the target the
/// block's declared weight sets. The identity alone costs nothing to
/// fabricate over the untrusted REST API, and so does the weight, which is
/// why the caller holds a declared weight against the weight of the blocks
/// captured at the height before it may displace them.
pub(crate) fn reconstruct_or_skip(
    height: i32,
    tx: &HathorTransaction,
) -> Result<HathorParentReconstruction> {
    let Some(inputs) = decode_reconstruction_inputs(height, tx)? else {
        return Ok(HathorParentReconstruction::Malformed);
    };
    let recon = match reconstruct_from_blobs(&inputs.raw, &inputs.aux_pow, inputs.expected) {
        Ok((_aux, recon)) => recon,
        Err(err) => {
            error!(height, error = %err, "Hathor reconstruction failed; skipping");
            return Ok(HathorParentReconstruction::Malformed);
        }
    };

    let Some(weight) = declared_weight(&inputs.raw, recon.funds_graph_split) else {
        error!(height, "Hathor block declares no readable weight; skipping");
        return Ok(HathorParentReconstruction::Malformed);
    };
    if !meets_hathor_target(recon.header.block_hash(), weight) {
        error!(
            height,
            weight, "Hathor block hash does not meet its declared target; skipping"
        );
        return Ok(HathorParentReconstruction::Malformed);
    }

    if !pow_validates_target(&recon.header) {
        // The common case: the embedded BTC header only met Hathor's (easier)
        // target, not BTC's, so it is a `near` template, not a real BTC block.
        // Expected for ~all merge-mined blocks; debug, not error, so a healthy
        // poller does not flood the log.
        debug!(
            height,
            "reconstructed Hathor parent fails its own PoW target; skipping (near)"
        );
        return Ok(HathorParentReconstruction::Near { weight });
    }

    Ok(HathorParentReconstruction::BtcValid(
        HathorReconstructedParent {
            raw: inputs.raw,
            aux_pow: inputs.aux_pow,
            recon,
            weight,
        },
    ))
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

/// The block's declared weight: the first graph field, a big-endian IEEE-754
/// double immediately after the funds|graph split.
pub(crate) fn declared_weight(raw: &[u8], funds_graph_split: usize) -> Option<f64> {
    let bytes: [u8; 8] = raw
        .get(funds_graph_split..funds_graph_split + 8)?
        .try_into()
        .ok()?;
    Some(f64::from_be_bytes(bytes))
}

/// Hathor's proof-of-work rule: the hash, read as a 256-bit number, is below
/// `2^(256 - weight)`.
fn meets_hathor_target(hash: BlockHash, weight: f64) -> bool {
    hathor_target(weight).is_some_and(|target| target.is_met_by(hash))
}

/// `2^(256 - weight)` as a 256-bit target, `None` for a weight outside
/// `(0, 256)` (a NaN included). The weight is a double, so the target carries
/// a 53-bit mantissa placed at the integer power of two; a difference at that
/// precision cannot separate real work from none.
fn hathor_target(weight: f64) -> Option<Target> {
    if !(weight > 0.0 && weight < 256.0) {
        return None;
    }
    let exponent = 256.0 - weight;
    let integer = exponent.floor();
    // 2^fraction in [1, 2) with 52 fractional bits: the target is
    // `mantissa * 2^(integer - 52)`.
    let mantissa = ((exponent - integer).exp2() * (1u64 << 52) as f64) as u64;
    let shift = integer as i32 - 52;
    let mut bytes = [0u8; 32];
    for bit in 0..53i32 {
        if (mantissa >> bit) & 1 == 1 {
            let position = bit + shift;
            if (0..256).contains(&position) {
                bytes[31 - (position / 8) as usize] |= 1 << (position % 8);
            }
        }
    }
    Some(Target::from_be_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::hathor::auxpow::forge_with_weight;

    fn fixture() -> (HathorTransaction, i32, f64) {
        let j: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hathor/1971823.json"
        )))
        .unwrap();
        (
            HathorTransaction {
                raw: j["raw_hex"].as_str().unwrap().to_owned(),
                aux_pow: Some(j["aux_pow_hex"].as_str().unwrap().to_owned()),
                hash: j["tx_id"].as_str().unwrap().to_owned(),
                timestamp: j["timestamp"].as_i64().unwrap_or(0),
            },
            j["hathor_height"].as_i64().unwrap() as i32,
            j["weight"].as_f64().unwrap(),
        )
    }

    fn power_of_two(bit: usize) -> Target {
        let mut bytes = [0u8; 32];
        bytes[31 - bit / 8] = 1 << (bit % 8);
        Target::from_be_bytes(bytes)
    }

    #[test]
    fn the_declared_weight_is_read_at_the_split_and_the_fixture_meets_it() {
        let (tx, height, weight) = fixture();
        let raw = hex::decode(&tx.raw).unwrap();
        let aux_pow = hex::decode(tx.aux_pow.as_deref().unwrap()).unwrap();
        let expected = BlockHash::from_str(&tx.hash).unwrap();
        let (_aux, recon) = reconstruct_from_blobs(&raw, &aux_pow, expected).unwrap();

        let declared = declared_weight(&raw, recon.funds_graph_split).unwrap();
        assert!(
            (declared - weight).abs() < 1e-9,
            "declared {declared}, fixture {weight}"
        );
        assert!(meets_hathor_target(expected, declared));
        // A weight far above the work the hash carries is not met.
        assert!(!meets_hathor_target(expected, 200.0));
        assert!(matches!(
            reconstruct_or_skip(height, &tx).unwrap(),
            HathorParentReconstruction::BtcValid(_)
        ));
    }

    #[test]
    fn a_target_is_the_power_of_two_the_weight_names() {
        assert_eq!(hathor_target(68.0), Some(power_of_two(188)));
        assert_eq!(hathor_target(255.0), Some(power_of_two(1)));
        assert_eq!(hathor_target(1.0), Some(power_of_two(255)));
        // A fractional weight lands between its neighbouring powers of two.
        let half = hathor_target(67.5).unwrap();
        assert!(power_of_two(188) < half && half < power_of_two(189));
        for out_of_range in [0.0, -1.0, 256.0, 300.0, f64::NAN, f64::INFINITY] {
            assert_eq!(hathor_target(out_of_range), None, "{out_of_range}");
        }
    }

    #[test]
    fn a_forged_trivial_weight_reconstructs_as_near_with_the_weight_it_declares() {
        let (tx, height, _) = fixture();
        let raw = hex::decode(&tx.raw).unwrap();
        let aux_pow = hex::decode(tx.aux_pow.as_deref().unwrap()).unwrap();
        let expected = BlockHash::from_str(&tx.hash).unwrap();
        let (_aux, recon) = reconstruct_from_blobs(&raw, &aux_pow, expected).unwrap();
        let (forged_raw, forged_hash) =
            forge_with_weight(&raw, &aux_pow, recon.funds_graph_split, 1e-6).unwrap();
        assert_ne!(forged_hash, expected);
        let forged = HathorTransaction {
            raw: hex::encode(forged_raw),
            aux_pow: tx.aux_pow.clone(),
            hash: forged_hash.to_string(),
            timestamp: tx.timestamp,
        };
        match reconstruct_or_skip(height, &forged).unwrap() {
            HathorParentReconstruction::Near { weight } => {
                assert!((weight - 1e-6).abs() < f64::EPSILON, "{weight}");
            }
            _ => panic!("a self-consistent block declaring trivial work reconstructs as near"),
        }
    }

    #[test]
    fn a_broken_identity_is_malformed_not_near() {
        let (mut tx, height, _) = fixture();
        let last = tx.raw.pop().unwrap();
        tx.raw.push(if last == '0' { '1' } else { '0' });
        assert!(matches!(
            reconstruct_or_skip(height, &tx).unwrap(),
            HathorParentReconstruction::Malformed
        ));
    }
}
