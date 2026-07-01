//! AuxPoW commitment verification, PoW target checks, and BIP34 height.

use super::*;

/// Verify the full CAuxPow commitment for a parsed AuxPoW block whose child block
/// hash is `child_block_hash` and merged-mining chain id is `chain_id`.
///
/// This is the trust boundary for capturing against a configurable (possibly
/// third-party) RPC: the own-node producers get this from their validating node,
/// but Elastos may run against the public endpoint, so it is enforced before any
/// write. Ports the Namecoin/Bitcoin `CAuxPow::check` invariants:
///
/// 1. the parent coinbase scriptSig carries EXACTLY ONE `fabe6d6d` marker,
///    immediately followed by `[aux_merkle_root:32][tree_size:4][nonce:4]`;
/// 2. `tree_size == 1 << chain_branch_len` (overflow-safe; depth < 32);
/// 3. both branch indices are non-negative and the coinbase branch index is 0;
/// 4. the chain branch index equals the deterministic LCG slot for `chain_id`;
/// 5. the parent coinbase txid folds up the coinbase branch to the parent header
///    merkle root (wire byte order);
/// 6. the child block hash folds up the chain branch to the committed aux merkle
///    root (the standard AuxPoW reversal: the reversed leaf folds to the reversed
///    committed root). Pinned against ELA 360062 / 1500000 / 2000000.
pub fn verify_auxpow_commitment(
    parsed: &ParsedAuxpowBlock,
    child_block_hash: BlockHash,
    chain_id: u32,
) -> Result<()> {
    // 1. Exactly one fabe6d6d marker, with room for the 40-byte commitment.
    let script = parsed.parent_coinbase_script.as_slice();
    let mut markers = script
        .windows(AUXPOW_MAGIC.len())
        .enumerate()
        .filter(|(_, window)| *window == AUXPOW_MAGIC)
        .map(|(idx, _)| idx);
    let magic_pos = markers
        .next()
        .context("AuxPoW magic (fabe6d6d) absent from the parent coinbase scriptSig")?;
    ensure!(
        markers.next().is_none(),
        "multiple AuxPoW magic markers in the parent coinbase scriptSig"
    );
    let after = magic_pos + AUXPOW_MAGIC.len();
    ensure!(
        script.len() >= after + 40,
        "AuxPoW commitment truncated after the fabe6d6d marker"
    );
    let aux_merkle_root: [u8; 32] = script[after..after + 32].try_into().unwrap();
    let tree_size = u32::from_le_bytes(script[after + 32..after + 36].try_into().unwrap());
    let nonce = u32::from_le_bytes(script[after + 36..after + 40].try_into().unwrap());

    // 2. tree_size == 1 << branch_len, overflow-safe.
    let branch_len = parsed.proof.chain_branch.hashes.len();
    ensure!(
        branch_len < 32,
        "AuxPoW chain merkle branch is too deep ({branch_len} >= 32)"
    );
    let expected_size = 1u32
        .checked_shl(branch_len as u32)
        .context("AuxPoW chain merkle branch length is unrepresentable")?;
    ensure!(
        tree_size == expected_size,
        "AuxPoW tree size {tree_size} != 1 << {branch_len}"
    );

    // 3. Non-negative indices; coinbase is the first parent transaction.
    ensure!(
        parsed.proof.coinbase_branch.index >= 0,
        "negative parent coinbase merkle branch index"
    );
    ensure!(
        parsed.proof.chain_branch.index >= 0,
        "negative AuxPoW chain merkle branch index"
    );
    ensure!(
        parsed.proof.coinbase_branch.index == 0,
        "parent coinbase is not the first transaction (branch index {})",
        parsed.proof.coinbase_branch.index
    );

    // 4. Deterministic chain slot (cannot be forged into an arbitrary position).
    let expected_slot = auxpow_expected_index(nonce, chain_id, branch_len);
    let chain_index = u32::try_from(parsed.proof.chain_branch.index).unwrap();
    ensure!(
        chain_index == expected_slot,
        "AuxPoW chain slot {chain_index} != deterministic slot {expected_slot} for chain id {chain_id}"
    );

    // 5. Coinbase merkle proof (wire order): parent coinbase txid -> parent header
    //    merkle root.
    let coinbase_root = fold_merkle_branch(
        parsed.parent_coinbase_txid.to_byte_array(),
        &parsed.proof.coinbase_branch,
    );
    ensure!(
        coinbase_root == parsed.parent_header.header.merkle_root.to_byte_array(),
        "parent coinbase merkle proof does not reach the parent header merkle root"
    );

    // 6. Chain merkle proof (AuxPoW reversal): the reversed child block hash folds
    //    to the reversed committed aux merkle root.
    let mut child_leaf = child_block_hash.to_byte_array();
    child_leaf.reverse();
    let chain_root = fold_merkle_branch(child_leaf, &parsed.proof.chain_branch);
    let mut expected_root = aux_merkle_root;
    expected_root.reverse();
    ensure!(
        chain_root == expected_root,
        "child block hash does not fold to the committed AuxPoW merkle root"
    );

    Ok(())
}

/// The deterministic AuxPoW chain slot: a fixed LCG over the parent coinbase
/// `nonce` and the merged-mining `chain_id`, modulo the tree size
/// (`1 << branch_len`). All arithmetic is u32-wrapping; ported from the
/// merged-mining spec. `branch_len` must be < 32 so `1 << branch_len` fits a u32
/// (the caller enforces this).
pub fn auxpow_expected_index(nonce: u32, chain_id: u32, branch_len: usize) -> u32 {
    debug_assert!(branch_len < 32);
    let mut rand = nonce;
    rand = rand.wrapping_mul(1_103_515_245).wrapping_add(12_345);
    rand = rand.wrapping_add(chain_id);
    rand = rand.wrapping_mul(1_103_515_245).wrapping_add(12_345);
    rand % (1u32 << branch_len)
}

/// Fold a 32-byte leaf up a [`MerkleBranch`] to its root, combining each sibling
/// in the branch `index` side order as `sha256d(left || right)`, all wire byte
/// order. The caller verifies `branch.index >= 0` before calling.
pub(crate) fn fold_merkle_branch(leaf: [u8; 32], branch: &MerkleBranch) -> [u8; 32] {
    let mut acc = leaf;
    let mut index = branch.index as u32;
    for sibling in &branch.hashes {
        let sib = sibling.to_byte_array();
        acc = if index & 1 == 1 {
            sha256d_pair(&sib, &acc)
        } else {
            sha256d_pair(&acc, &sib)
        };
        index >>= 1;
    }
    acc
}

/// Double-SHA256 of `left || right` (the two 32-byte hashes concatenated into 64
/// bytes, wire byte order in and out). The merkle-fold primitive behind
/// [`fold_merkle_branch`]; output is `to_byte_array` order with no reversal.
pub(crate) fn sha256d_pair(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut data = [0u8; 64];
    data[..32].copy_from_slice(left);
    data[32..].copy_from_slice(right);
    sha256d::Hash::hash(&data).to_byte_array()
}

pub fn pow_validates_target(header: &Header) -> bool {
    validates_target(header.block_hash(), header.bits)
}

pub fn validates_target(hash: BlockHash, bits: CompactTarget) -> bool {
    // Reject the malformed compact-target cases the previous helper
    // rejected: sign bit, zero mantissa, zero exponent, exponent > 32.
    // `Target::from_compact` is a decoder and does not reject these, so
    // without this guard a corrupt nBits could spuriously satisfy a low
    // decoded target and mis-classify a parent header as PoW-valid.
    let raw_bits = bits.to_consensus();
    let exponent = (raw_bits >> 24) as usize;
    let mantissa = raw_bits & 0x007f_ffff;
    let sign = raw_bits & 0x0080_0000 != 0;
    if sign || mantissa == 0 || exponent == 0 || exponent > 32 {
        return false;
    }
    let target = Target::from_compact(bits);
    target.is_met_by(hash)
}

pub fn parse_bip34_height(script_sig: &[u8]) -> Option<i32> {
    if script_sig.len() < 2 {
        return None;
    }

    let byte_len = script_sig[0] as usize;
    if !(1..=4).contains(&byte_len) || script_sig.len() < 1 + byte_len {
        return None;
    }

    let mut raw = [0u8; 4];
    raw[..byte_len].copy_from_slice(&script_sig[1..1 + byte_len]);
    let height = i32::from_le_bytes(raw);
    (0..=2_000_000).contains(&height).then_some(height)
}
