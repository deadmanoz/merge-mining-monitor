//! What an RSK merge-mining proof says about its Bitcoin parent block.

use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;

/// The parent coinbase's txid when the RSK merge-mining proof shows it is the parent
/// block's only transaction, which makes it the header's merkle root. RSK sends that proof
/// as an empty hash list or, in early history, as a one-transaction partial merkle tree
/// (`01000000 01 <root> 01 01`). Any other proof leaves the txid unknown.
pub(crate) fn coinbase_only_txid(header: &Header, merkle_proof: Option<&[u8]>) -> Option<Vec<u8>> {
    let root = header.merkle_root.to_byte_array();
    let proof = merkle_proof?;
    let single_tx_tree = proof.len() == 39
        && proof[..5] == [1, 0, 0, 0, 1]
        && proof[37..] == [1, 1]
        && proof[5..37] == root;
    (proof.is_empty() || single_tx_tree).then(|| root.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::block::Version;
    use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};

    #[test]
    fn only_a_coinbase_only_proof_yields_the_parent_coinbase_txid() {
        let root = [7u8; 32];
        let header = Header {
            version: Version::TWO,
            prev_blockhash: BlockHash::all_zeros(),
            merkle_root: TxMerkleNode::from_byte_array(root),
            time: 0,
            bits: CompactTarget::from_consensus(0x1d00_ffff),
            nonce: 0,
        };
        let single_tx_tree = |hash: [u8; 32]| [&[1, 0, 0, 0, 1][..], &hash, &[1, 1]].concat();
        assert_eq!(coinbase_only_txid(&header, None), None);
        assert_eq!(coinbase_only_txid(&header, Some(&[])), Some(root.to_vec()));
        assert_eq!(coinbase_only_txid(&header, Some(&[0u8; 32])), None);
        assert_eq!(
            coinbase_only_txid(&header, Some(&single_tx_tree(root))),
            Some(root.to_vec())
        );
        assert_eq!(
            coinbase_only_txid(&header, Some(&single_tx_tree([8u8; 32]))),
            None
        );
    }
}
