//! Stored-bytes decoding: aux markers, blob summaries, proof detail, and
//! coinbase tag/address presentation helpers.

use super::*;

/// The AuxPoW marker decoded from a parent coinbase `scriptSig`: the 44-byte
/// `[magic][aux_merkle_root][merkle_size][merkle_nonce]` blob the merged-mining
/// spec places in the Bitcoin coinbase. `aux_merkle_root` is a hash newtype so it
/// serialises through `Display` in the repo's reversed/display order (the reverse
/// of its raw scriptSig bytes), matching every other API hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxMarker {
    pub aux_merkle_root: TxMerkleNode,
    pub merkle_size: u32,
    pub merkle_nonce: u32,
}

/// The slot-index + parent-header-hash subset of a decoded CAuxPow-only blob
/// (`merge_mining_event.aux_merkle_proof`): the chain's slot index
/// (`nChainIndex`) and the embedded parent header hash (used to gate the slot
/// against the event's own `btc_parent_header_hash`). The production `/block`
/// read path uses the richer `AuxpowProofDetail` via `decode_auxpow_proof`; this
/// narrow subset (and its `auxpow_blob_summary` constructor) is gated behind
/// `test-support` and backs only the fixture-contract test.
#[cfg(any(test, feature = "test-support"))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxpowBlobSummary {
    pub slot_index: u32,
    pub parent_header_hash: BlockHash,
}

/// One merkle branch (`CMerkleBranch`) out of a CAuxPow record: the sibling
/// hashes from leaf up to root, plus the side-mask / index that positions the
/// folded hash at each level.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxMerkleBranchDetail {
    /// The branch side-mask / `nIndex`. Non-negative by construction:
    /// `decode_auxpow_proof` rejects the whole proof if either branch index is
    /// negative, so a corrupt blob falls back to no breakdown rather than a
    /// structured proof with a nonsensical index.
    pub index: u32,
    pub siblings: Vec<TxMerkleNode>,
}

/// The decoded structure of a stored CAuxPow-only blob: the two merkle proofs
/// (`coinbase_branch` from the coinbase txid up to the parent transaction merkle
/// root, `blockchain_branch` from the aux block hash up to the marker's
/// `aux_merkle_root`), the redundant `CAuxPow::hashBlock`, the slot index, and
/// the embedded parent header hash. This is the human breakdown the read API
/// renders in place of the raw proof bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxpowProofDetail {
    pub slot_index: u32,
    pub parent_header_hash: BlockHash,
    /// `CAuxPow::hashBlock`, the redundant convenience hash the verifier ignores;
    /// Namecoin conventionally writes it as all-zero, so do NOT treat it as the
    /// actual parent block hash (that is `parent_header_hash`).
    pub hash_block: BlockHash,
    pub coinbase_branch: AuxMerkleBranchDetail,
    pub blockchain_branch: AuxMerkleBranchDetail,
}

/// Decode the AuxPoW marker out of a parent coinbase `scriptSig`. Magic-required
/// form only: returns `None` when the `0xFA BE 6D 6D` magic is absent (the rare
/// legacy no-magic variant that places the root in the first 20 bytes is not
/// decoded) or when the bytes after the magic are truncated.
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
        // The 32 scriptSig bytes are wire/internal order; `from_byte_array`
        // stores them as-is and `Display` reverses for the display-order hex.
        aux_merkle_root: TxMerkleNode::from_byte_array(root),
        merkle_size,
        merkle_nonce,
    })
}

/// Extract printable ASCII tag runs from a Bitcoin coinbase scriptSig.
///
/// This is intentionally presentational: it mirrors the historical frontend
/// display rule for the raw coinbase tag rather than trying to parse pool
/// identity. Printable bytes are `0x20..=0x7e`; every other byte splits a run.
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

/// Re-parse a stored CAuxPow-only blob (the `aux_merkle_proof` BYTEA, i.e. the
/// `auxpow_bytes` region) and return its slot index and embedded parent header
/// hash. Returns `None` on any parse failure or a negative `nChainIndex` (so a
/// corrupt index never becomes a huge positive slot). The full
/// `[child header][CAuxPow]` blob is *not* accepted here: it would mis-parse the
/// child header as a transaction and error out.
#[cfg(any(test, feature = "test-support"))]
pub fn auxpow_blob_summary(aux_merkle_proof: &[u8]) -> Option<AuxpowBlobSummary> {
    let detail = decode_auxpow_proof(aux_merkle_proof)?;
    Some(AuxpowBlobSummary {
        slot_index: detail.slot_index,
        parent_header_hash: detail.parent_header_hash,
    })
}

/// Fully decode a stored CAuxPow-only blob into its two merkle branches plus the
/// slot index, parent header hash, and redundant `hashBlock`. This is the
/// production `/block` read path over a stored `aux_merkle_proof`; the
/// test-only `auxpow_blob_summary` keeps only the slot index and parent header
/// hash from this same decode. Full-consumption EOF check and negative-index
/// rejection; the full `[child header][CAuxPow]` blob is not accepted.
pub fn decode_auxpow_proof(aux_merkle_proof: &[u8]) -> Option<AuxpowProofDetail> {
    let mut reader = Reader::new(aux_merkle_proof);
    let auxpow = read_auxpow(&mut reader).ok()?;
    // The stored blob is exactly the CAuxPow region, so a correct parse consumes
    // it fully. Requiring EOF rejects the full `[child header][CAuxPow]` blob
    // (80 bytes longer) even in the unlikely event it mis-parses without error.
    if !reader.is_eof() {
        return None;
    }
    // Both branch indices must be non-negative; a negative `nIndex` in either
    // branch means a corrupt blob, so reject the whole proof (the caller then
    // shows the raw-bytes fallback) rather than emitting a structured proof with
    // a nonsensical index. `slot_index` is the chain-branch index.
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

/// Derive Bitcoin mainnet payout addresses from coinbase outputs. Shared by the
/// live parser and the offline `reclassify-pools` historical re-resolution so
/// both attribute address-only pools through identical address derivation.
/// Non-address scripts (OP_RETURN, witness commitments, unparseable) are
/// skipped.
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
