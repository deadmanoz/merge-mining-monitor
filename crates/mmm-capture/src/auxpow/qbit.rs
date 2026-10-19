//! Qbit native merge-mining proof parsing (`[child header:80][non-witness parent coinbase tx][parent branch + i32][chain branch + i32][parent header:80]`, no `hashBlock` field).
//!
//! Pinned to Qbit `70fea84f5becfb57463247af09790df5ddd424f8` (`fixtures/qbit/`).
//! Semantics that differ from classic CAuxPow:
//!
//! - The chain-commitment fold runs in INTERNAL byte order over
//!   `sha256d(pure child header)`, and the coinbase scriptSig commits the
//!   DISPLAY-order (reversed) fold result, as classic `CAuxPow::check` does
//!   (the marker placement and footer are read by the shared
//!   `read_commitment_footer`). Elastos instead folds the reversed leaf and
//!   commits wire order; the two conventions are not interchangeable.
//! - Mainnet accepts display-order commitments at EVERY height from zero,
//!   including the legacy no-marker rule (without `fabe6d6d`, the first
//!   occurrence of the display-order root must start at scriptSig byte
//!   offset <= 20). There is no either-order mode.
//! - The child's effective target is ALWAYS its own pure-header nBits, never
//!   a wrapper-supplied target: the parent header hash must meet the CHILD
//!   target, and the parent's own nBits is never consulted for validity.

use super::*;

/// Qbit mainnet merged-mining chain id (`(version >> 13) & 0xFFFF`).
pub const QBIT_AUXPOW_CHAIN_ID: u32 = 47;

/// A Qbit AuxPoW proof that is malformed or does not commit to its block, as distinct
/// from a block that fails its own version, target or body checks. Callers find it with
/// `anyhow::Error::downcast_ref`.
#[derive(Debug)]
pub struct QbitProofError(anyhow::Error);

impl std::fmt::Display for QbitProofError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}

impl std::error::Error for QbitProofError {}

fn proof_error(err: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(QbitProofError(err))
}

const QBIT_PARENT_BRANCH_CAP: usize = 31;
const QBIT_CHAIN_BRANCH_CAP: usize = 30;

fn qbit_pow_limit() -> Target {
    let mut limit = [0xffu8; 32];
    limit[0] = 0x00;
    limit[1] = 0x00;
    Target::from_be_bytes(limit)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedQbitBlock {
    Direct(ParsedHeader),
    Auxpow(Box<ParsedQbitAuxpow>),
}

/// Parsed Qbit merged-mining proof. `auxpow_bytes` is `raw[80..]` (parent
/// coinbase tx through parent header; no `hash_block` field).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQbitAuxpow {
    pub child_header: ParsedHeader,
    pub parent_header: ParsedHeader,
    pub parent_coinbase_txid: Txid,
    pub parent_coinbase_script: Vec<u8>,
    pub parent_coinbase_outputs: Vec<TxOut>,
    pub parent_coinbase_output_addresses: Vec<String>,
    pub coinbase_branch: MerkleBranch,
    pub chain_branch: MerkleBranch,
    pub auxpow_bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQbitAuxpowBlock {
    pub proof: ParsedQbitAuxpow,
    pub coinbase: ParsedChildBlockCoinbase,
}

/// Authenticate a full native Qbit block's proof and child transaction body
/// (merkle root and BIP34 height). Direct-mined blocks return `Ok(None)`.
pub fn parse_qbit_block(raw: &[u8], child_height: u32) -> Result<Option<ParsedQbitAuxpowBlock>> {
    let prefix = qbit_extended_header_prefix(raw, child_height)?;
    let ParsedQbitBlock::Auxpow(proof) = parse_qbit_extended_header(prefix, child_height)? else {
        return Ok(None);
    };
    let mut child_bytes = raw[..Header::SIZE].to_vec();
    child_bytes.extend_from_slice(&raw[prefix.len()..]);
    let coinbase = parse_child_block_coinbase_with_height(&child_bytes, parse_child_bip34_height)?;
    let native_height =
        i32::try_from(child_height).context("Qbit native height exceeds signed range")?;
    ensure!(
        coinbase.child_height == Some(native_height),
        "Qbit child coinbase height does not match native height"
    );
    Ok(Some(ParsedQbitAuxpowBlock {
        proof: *proof,
        coinbase,
    }))
}

/// Parse and envelope-verify one exact Qbit extended header (no trailing bytes).
pub fn parse_qbit_extended_header(raw: &[u8], child_height: u32) -> Result<ParsedQbitBlock> {
    ensure!(
        raw.len() >= Header::SIZE,
        "Qbit extended header is shorter than an 80-byte header"
    );
    let child_header = parse_header(raw[0..Header::SIZE].try_into().unwrap())
        .context("parse Qbit child header")?;
    let version = child_header.header.version.to_consensus() as u32;
    let merged = qbit_version_gate(version, child_height)?;
    let child_target = qbit_child_target(child_header.bits())?;

    let mut reader = Reader::new(raw);
    reader.skip(Header::SIZE)?;

    if !merged {
        ensure!(reader.is_eof(), "trailing bytes on direct Qbit header");
        ensure!(
            child_target.is_met_by(child_header.hash()),
            "direct Qbit proof fails child target"
        );
        return Ok(ParsedQbitBlock::Direct(child_header));
    }

    let auxpow_start = reader.position();
    let auxpow = read_qbit_auxpow(&mut reader).map_err(proof_error)?;
    if !reader.is_eof() {
        return Err(proof_error(anyhow::anyhow!(
            "trailing bytes on Qbit AuxPoW header"
        )));
    }
    verify_qbit_proof(&child_header, &auxpow).map_err(proof_error)?;
    ensure!(
        child_target.is_met_by(auxpow.parent_header.hash()),
        "Qbit parent proof fails child header target"
    );
    let auxpow_bytes = raw[auxpow_start..reader.position()].to_vec();

    Ok(ParsedQbitBlock::Auxpow(Box::new(ParsedQbitAuxpow {
        child_header,
        parent_header: auxpow.parent_header,
        parent_coinbase_txid: auxpow.coinbase_txid,
        parent_coinbase_output_addresses: output_addresses(&auxpow.coinbase_outputs),
        parent_coinbase_script: auxpow.coinbase_script,
        parent_coinbase_outputs: auxpow.coinbase_outputs,
        coinbase_branch: auxpow.coinbase_branch,
        chain_branch: auxpow.chain_branch,
        auxpow_bytes,
    })))
}

/// Slice the exact Qbit extended-header prefix from a full `getblock <hash> 0` payload.
pub fn qbit_extended_header_prefix(raw: &[u8], child_height: u32) -> Result<&[u8]> {
    ensure!(
        raw.len() >= Header::SIZE,
        "Qbit block is shorter than an 80-byte header"
    );
    let child_header = parse_header(raw[0..Header::SIZE].try_into().unwrap())
        .context("parse Qbit child header")?;
    let version = child_header.header.version.to_consensus() as u32;
    if !qbit_version_gate(version, child_height)? {
        return Ok(&raw[..Header::SIZE]);
    }
    let mut reader = Reader::new(raw);
    reader.skip(Header::SIZE)?;
    read_qbit_auxpow(&mut reader).map_err(proof_error)?;
    Ok(&raw[..reader.position()])
}

/// Enforce Qbit mainnet's version layout (`001` top-bit shape `0x20000000`,
/// reserved bits `0x00001E00` clear, chain id 47 in bits 13..29 when bit 8 is set,
/// and `4 <= version < 0x80000000` after genesis).
pub(crate) fn qbit_version_gate(version: u32, height: u32) -> Result<bool> {
    if height > 0 {
        ensure!(
            (4..0x8000_0000).contains(&version),
            "Qbit minimum block version is 4 after genesis (got {version:#010x})"
        );
    }
    let shaped = version & 0xE000_0000 == 0x2000_0000;
    ensure!(
        shaped || version & 0xFFFF_FF00 == 0,
        "Qbit non-canonical top-bit version layout ({version:#010x})"
    );
    ensure!(
        !shaped || version & 0x0000_1E00 == 0,
        "Qbit reserved version bits set ({version:#010x})"
    );
    let merged = shaped && version & 0x100 != 0;
    if merged {
        let chain_id = (version >> 13) & 0xFFFF;
        ensure!(
            chain_id == QBIT_AUXPOW_CHAIN_ID,
            "Qbit AuxPoW chain id {chain_id} != mainnet chain id {QBIT_AUXPOW_CHAIN_ID}"
        );
    }
    Ok(merged)
}

fn qbit_child_target(bits: CompactTarget) -> Result<Target> {
    well_formed_compact_target(bits)
        .filter(|target| *target != Target::ZERO && *target <= qbit_pow_limit())
        .context("invalid Qbit header target")
}

fn read_qbit_branch(reader: &mut Reader<'_>, cap: usize) -> Result<Vec<TxMerkleNode>> {
    let count = reader.read_u8().context("read Qbit merkle branch count")? as usize;
    ensure!(
        count <= cap,
        "Qbit merkle branch count {count} exceeds limit {cap}"
    );
    let mut hashes = Vec::with_capacity(count);
    for _ in 0..count {
        hashes.push(TxMerkleNode::from_byte_array(
            reader
                .read_array::<32>()
                .context("read Qbit merkle branch hash")?,
        ));
    }
    Ok(hashes)
}

struct QbitRawAuxpow {
    coinbase_txid: Txid,
    coinbase_script: Vec<u8>,
    coinbase_outputs: Vec<TxOut>,
    coinbase_branch: MerkleBranch,
    chain_branch: MerkleBranch,
    parent_header: ParsedHeader,
}

fn read_qbit_auxpow(reader: &mut Reader<'_>) -> Result<QbitRawAuxpow> {
    // Byte 4 of the tx region is the input-count position; 0x00 there is the
    // BIP144 segwit marker (a real tx never has zero inputs). rust-bitcoin
    // would deserialize the witness encoding happily, so reject it first:
    // Qbit requires the non-witness serialization.
    ensure!(
        reader.remaining_slice().get(4) != Some(&0x00),
        "Qbit parent coinbase must use non-witness encoding"
    );
    let (tx, consumed) = deserialize_partial::<Transaction>(reader.remaining_slice())
        .context("parse Qbit parent coinbase transaction")?;
    reader.skip(consumed)?;
    ensure!(tx.is_coinbase(), "Qbit proof transaction is not coinbase");
    let coinbase_script = tx
        .input
        .first()
        .expect("is_coinbase guarantees exactly one input")
        .script_sig
        .as_bytes()
        .to_vec();

    let coinbase_hashes = read_qbit_branch(reader, QBIT_PARENT_BRANCH_CAP)
        .context("read Qbit parent merkle branch")?;
    let coinbase_index = reader.read_i32().context("read Qbit parent merkle index")?;
    let chain_hashes =
        read_qbit_branch(reader, QBIT_CHAIN_BRANCH_CAP).context("read Qbit chain merkle branch")?;
    let chain_index = reader.read_i32().context("read Qbit chain index")?;
    let parent_header = parse_header(
        reader
            .read_array::<{ Header::SIZE }>()
            .context("read Qbit parent header")?,
    )
    .context("parse Qbit parent header")?;

    Ok(QbitRawAuxpow {
        coinbase_txid: tx.compute_txid(),
        coinbase_script,
        coinbase_outputs: tx.output,
        coinbase_branch: MerkleBranch {
            hashes: coinbase_hashes,
            index: coinbase_index,
        },
        chain_branch: MerkleBranch {
            hashes: chain_hashes,
            index: chain_index,
        },
        parent_header,
    })
}

/// The proof's commitment to `child_header`; the caller checks the parent against the
/// child target.
fn verify_qbit_proof(child_header: &ParsedHeader, auxpow: &QbitRawAuxpow) -> Result<()> {
    ensure!(
        auxpow.coinbase_branch.index == 0,
        "Qbit coinbase branch index must be zero (got {})",
        auxpow.coinbase_branch.index
    );
    let chain_len = auxpow.chain_branch.hashes.len();
    let chain_index = auxpow.chain_branch.index;
    ensure!(
        chain_index >= 0 && (chain_index as u64) < (1u64 << chain_len),
        "Qbit chain index {chain_index} outside tree width {}",
        1u64 << chain_len
    );

    let coinbase_root = fold_merkle_branch(
        auxpow.coinbase_txid.to_byte_array(),
        &auxpow.coinbase_branch,
    );
    ensure!(
        coinbase_root == auxpow.parent_header.header.merkle_root.to_byte_array(),
        "Qbit parent coinbase Merkle root mismatch"
    );

    // INTERNAL-order fold of sha256d(pure child header); the scriptSig
    // commits the DISPLAY-order (reversed) fold result.
    let chain_root = fold_merkle_branch(child_header.hash().to_byte_array(), &auxpow.chain_branch);
    let mut display_root = chain_root;
    display_root.reverse();
    let (tree_size, nonce) = read_commitment_footer(&auxpow.coinbase_script, &display_root)
        .context("Qbit chain commitment")?;
    ensure!(
        tree_size == 1u32 << chain_len,
        "Qbit commitment tree size {tree_size} != 1 << {chain_len}"
    );
    let expected_slot = auxpow_expected_index(nonce, QBIT_AUXPOW_CHAIN_ID, chain_len);
    ensure!(
        chain_index as u32 == expected_slot,
        "Qbit commitment chain slot {chain_index} != deterministic slot {expected_slot}"
    );
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QbitProofDetail {
    pub slot_index: u32,
    pub parent_header_hash: BlockHash,
    pub coinbase_branch: AuxMerkleBranchDetail,
    pub blockchain_branch: AuxMerkleBranchDetail,
}

/// Structurally decode a stored Qbit `aux_merkle_proof` blob into its branch breakdown.
pub fn decode_qbit_auxpow_proof(aux_merkle_proof: &[u8]) -> Option<QbitProofDetail> {
    let mut reader = Reader::new(aux_merkle_proof);
    let auxpow = read_qbit_auxpow(&mut reader).ok()?;
    if !reader.is_eof() {
        return None;
    }
    let slot_index = u32::try_from(auxpow.chain_branch.index).ok()?;
    let coinbase_index = u32::try_from(auxpow.coinbase_branch.index).ok()?;
    Some(QbitProofDetail {
        slot_index,
        parent_header_hash: auxpow.parent_header.hash(),
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
