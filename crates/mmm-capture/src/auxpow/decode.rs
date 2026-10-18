//! Stored-bytes decoding: aux markers, proof detail, and coinbase tag/address helpers.

use super::*;

/// Decoded 44-byte `[magic][aux_merkle_root][merkle_size][merkle_nonce]` coinbase marker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxMarker {
    pub aux_merkle_root: TxMerkleNode,
    pub merkle_size: u32,
    pub merkle_nonce: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxMerkleBranchDetail {
    pub index: u32,
    pub siblings: Vec<TxMerkleNode>,
}

/// Decoded structure of a stored `CAuxPow`-only blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxpowProofDetail {
    pub slot_index: u32,
    pub parent_header_hash: BlockHash,
    /// Redundant `CAuxPow::hashBlock` (ignored by verifier; often all-zero in Namecoin).
    pub hash_block: BlockHash,
    pub coinbase_branch: AuxMerkleBranchDetail,
    pub blockchain_branch: AuxMerkleBranchDetail,
}

/// Decode the `0xFABE6D6D`-prefixed AuxPoW marker from a parent coinbase `scriptSig`.
pub fn decode_aux_marker(coinbase_script: &[u8]) -> Option<AuxMarker> {
    const MAGIC: [u8; 4] = [0xfa, 0xbe, 0x6d, 0x6d];
    let magic_pos = coinbase_script
        .windows(MAGIC.len())
        .position(|w| w == MAGIC)?;
    let after = magic_pos + MAGIC.len();
    let root_end = after + 32;
    let size_end = root_end + 4;
    let nonce_end = size_end + 4;
    if coinbase_script.len() < nonce_end {
        return None;
    }
    let root: [u8; 32] = coinbase_script[after..root_end].try_into().ok()?;
    let merkle_size = u32::from_le_bytes(coinbase_script[root_end..size_end].try_into().ok()?);
    let merkle_nonce = u32::from_le_bytes(coinbase_script[size_end..nonce_end].try_into().ok()?);
    Some(AuxMarker {
        aux_merkle_root: TxMerkleNode::from_byte_array(root),
        merkle_size,
        merkle_nonce,
    })
}

/// Extract printable ASCII (`0x20..=0x7e`) runs of length >= 4 from a coinbase scriptSig.
pub fn extract_coinbase_tag(coinbase_script: &[u8]) -> Option<String> {
    fn flush_run(run: &mut String, tags: &mut Vec<String>) {
        let trimmed = run.trim();
        if trimmed.len() >= 4 {
            tags.push(trimmed.to_owned());
        }
        run.clear();
    }

    let mut tags = Vec::new();
    let mut run = String::new();
    for byte in coinbase_script {
        if (0x20..=0x7e).contains(byte) {
            run.push(char::from(*byte));
        } else {
            flush_run(&mut run, &mut tags);
        }
    }
    flush_run(&mut run, &mut tags);

    (!tags.is_empty()).then(|| tags.join(" "))
}

/// Decode a stored `CAuxPow`-only blob, requiring exact EOF and non-negative branch indices.
pub fn decode_auxpow_proof(aux_merkle_proof: &[u8]) -> Option<AuxpowProofDetail> {
    let mut reader = Reader::new(aux_merkle_proof);
    let auxpow = read_auxpow(&mut reader).ok()?;
    if !reader.is_eof() {
        return None;
    }
    let slot_index = u32::try_from(auxpow.chain_branch.index).ok()?;
    let coinbase_index = u32::try_from(auxpow.coinbase_branch.index).ok()?;
    Some(AuxpowProofDetail {
        slot_index,
        parent_header_hash: auxpow.parent_header.hash(),
        hash_block: auxpow.hash_block,
        coinbase_branch: AuxMerkleBranchDetail {
            index: coinbase_index,
            siblings: auxpow.coinbase_branch.hashes,
        },
        blockchain_branch: AuxMerkleBranchDetail {
            index: slot_index,
            siblings: auxpow.chain_branch.hashes,
        },
    })
}

/// Derive Bitcoin mainnet payout addresses from coinbase outputs, skipping non-address scripts.
pub fn output_addresses(outputs: &[TxOut]) -> Vec<String> {
    outputs
        .iter()
        .filter_map(|output| {
            Address::from_script(&output.script_pubkey, Network::Bitcoin)
                .ok()
                .map(|address| address.to_string())
        })
        .collect()
}
