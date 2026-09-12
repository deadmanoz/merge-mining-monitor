//! Qbit native merge-mining proof parsing.
//!
//! Qbit's extended header is NOT a classic `CAuxPow`. Wire order: 80-byte
//! pure child header; non-witness parent coinbase transaction; parent merkle
//! branch (one-byte count + 32-byte sibling hashes) + signed i32 index; chain
//! merkle branch + signed i32 index; 80-byte parent header. There is NO
//! legacy 32-byte `hashBlock` field between the coinbase transaction and the
//! parent branch, and none is synthesized (never store a placeholder hash).
//!
//! Rules are pinned to Qbit source revision
//! `70fea84f5becfb57463247af09790df5ddd424f8` via the executable reference
//! adapter (`src/stale_blocks_analysis/qbit.py` at merge-mining-research
//! `e3dc6d6`), whose mainnet control fixtures are mirrored in
//! `fixtures/qbit/`. Semantics that differ from classic CAuxPow:
//!
//! - The chain-commitment fold runs in INTERNAL byte order over
//!   `sha256d(pure child header)`, and the coinbase scriptSig commits the
//!   DISPLAY-order (reversed) fold result. Classic folds the reversed leaf
//!   and commits wire order; the two conventions are not interchangeable.
//! - Mainnet accepts display-order commitments at EVERY height from zero,
//!   including the legacy no-marker rule (without `fabe6d6d`, the first
//!   occurrence of the display-order root must start at scriptSig byte
//!   offset <= 20). There is no either-order mode.
//! - The child's effective target is ALWAYS its own pure-header nBits, never
//!   a wrapper-supplied target: the parent header hash must meet the CHILD
//!   target, and the parent's own nBits is never consulted for validity.
//!
//! Scope boundary: validating this proof envelope is NOT validating Qbit
//! body consensus or Bitcoin parent placement. `child_height` is a caller
//! claim (explorer/import provenance); it feeds only the version-cadence
//! gate. There is deliberately no genesis-hash pin here: chain identity is
//! the importer/producer's job, and the live producer authenticates it in
//! `chains::auxpow_family::qbit` by matching the decoded child header hash
//! against the height's `getblockhash` result and pinning mainnet genesis at
//! height zero.

use super::*;

/// Qbit mainnet merged-mining chain id: the value the version field's
/// chain-id bits (`(version >> 13) & 0xFFFF`) must carry on a merged block,
/// and the LCG slot input. Pinned from revision `70fea84` chainparams.
pub const QBIT_AUXPOW_CHAIN_ID: u32 = 47;

/// Maximum accepted parent (coinbase) merkle branch length.
const QBIT_PARENT_BRANCH_CAP: usize = 31;

/// Maximum accepted chain merkle branch length. Also bounds `1 << len` for
/// the tree-size and slot checks well inside u32.
const QBIT_CHAIN_BRANCH_CAP: usize = 30;

/// Qbit mainnet proof-of-work limit (`0x0000ffff…ff`, 60 f's): the inclusive
/// upper bound every child target must satisfy.
fn qbit_pow_limit() -> Target {
    let mut limit = [0xffu8; 32];
    limit[0] = 0x00;
    limit[1] = 0x00;
    Target::from_be_bytes(limit)
}

/// The outcome of parsing a Qbit extended header: a direct (non-merged) child
/// header that must meet its own target, or a fully verified merged-mining
/// proof. Mirrors [`ParsedNamecoinBlock`] so the producer slice can skip
/// non-merged blocks without allocating proof state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedQbitBlock {
    /// A non-merged child header (AuxPoW version flag clear); its own hash
    /// met its own target.
    Direct(ParsedHeader),
    /// A fully parsed and envelope-verified merged-mining proof, boxed to
    /// keep the enum small.
    Auxpow(Box<ParsedQbitAuxpow>),
}

/// A fully parsed Qbit merged-mining proof. `auxpow_bytes` is the stored-bytes
/// contract for the producer slice: exactly the region from the parent
/// coinbase transaction through the parent header (`raw[80..]` for a valid
/// exact extended header), the region [`decode_qbit_auxpow_proof`] re-accepts.
/// Unlike [`ParsedAuxpowBlock`] there is no `hash_block`: the wire format has
/// no such field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedQbitAuxpow {
    pub child_header: ParsedHeader,
    pub parent_header: ParsedHeader,
    pub parent_coinbase_txid: Txid,
    pub parent_coinbase_script: Vec<u8>,
    pub parent_coinbase_outputs: Vec<TxOut>,
    pub parent_coinbase_output_addresses: Vec<String>,
    /// Proves the parent coinbase is in the parent block's merkle tree
    /// (index verified to be exactly 0).
    pub coinbase_branch: MerkleBranch,
    /// Proves `sha256d(pure child header)` is in the merged-mining chain
    /// merkle tree committed by the parent coinbase (index verified against
    /// the deterministic LCG slot).
    pub chain_branch: MerkleBranch,
    pub auxpow_bytes: Vec<u8>,
}

/// Parse and envelope-verify one exact Qbit extended header (no trailing
/// bytes accepted). `child_height` is the caller-claimed native height and
/// feeds only the version-cadence gate. Checks run in the reference
/// adapter's order so error causes match: version gate, child-target gate,
/// then for merged blocks the structural reads, EOF, branch-index bounds,
/// parent merkle inclusion, chain commitment, and the parent-hash-versus-
/// CHILD-target proof-of-work gate.
pub fn parse_qbit_extended_header(raw: &[u8], child_height: u32) -> Result<ParsedQbitBlock> {
    ensure!(
        raw.len() >= Header::SIZE,
        "Qbit extended header is shorter than an 80-byte header"
    );
    let child_header = parse_header(raw[0..Header::SIZE].try_into().unwrap())
        .context("parse Qbit child header")?;
    // The version layout is defined on the unsigned u32 bit pattern.
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
    let auxpow = read_qbit_auxpow(&mut reader)?;
    ensure!(reader.is_eof(), "trailing bytes on Qbit AuxPoW header");
    verify_qbit_proof(&child_header, &auxpow, child_target)?;
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

/// Split the exact Qbit extended-header prefix out of a full `getblock <hash>
/// 0` block. Qbit's RPC returns the complete block, but
/// [`parse_qbit_extended_header`] deliberately rejects trailing bytes, so the
/// producer must hand it exactly the header/proof region and nothing more.
///
/// Bounded by the same structural reads the parser uses (branch caps, fixed
/// 80-byte headers, one non-witness coinbase transaction), so a hostile block
/// cannot make this scan past its own bytes. Verification is NOT performed
/// here: this only locates the region, and the returned prefix is re-read and
/// fully verified by [`parse_qbit_extended_header`].
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
    read_qbit_auxpow(&mut reader)?;
    Ok(&raw[..reader.position()])
}

/// Enforce Qbit mainnet's version layout and return whether the block is
/// merged-mined. The AuxPoW flag is bit 8 and is meaningful only under the
/// `001` top-bit shape (`version & 0xE0000000 == 0x20000000`); with that
/// shape, reserved bits `0x00001E00` must be clear; any version using bit 8
/// or above without the shape is non-canonical; a merged version must carry
/// chain id [`QBIT_AUXPOW_CHAIN_ID`] in bits 13..29; after genesis the
/// version must satisfy `4 <= version < 0x80000000`.
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

/// Decode the child's own nBits into its effective target: the compact
/// encoding must be well-formed and the target inside `(0, pow_limit]`.
/// This is the ONLY target Qbit proofs are checked against.
fn qbit_child_target(bits: CompactTarget) -> Result<Target> {
    well_formed_compact_target(bits)
        .filter(|target| *target != Target::ZERO && *target <= qbit_pow_limit())
        .context("invalid Qbit header target")
}

/// Read one Qbit merkle branch: a single count byte (a multi-byte
/// CompactSize prefix is >= 0xfd > every cap, so it is over the limit or
/// non-canonical either way) followed by the bounded 32-byte sibling reads.
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

/// The raw fields of one Qbit AuxPoW payload (coinbase tx through parent
/// header), before verification and assembly. Shared structural reader for
/// [`parse_qbit_extended_header`] and the stored-blob re-decoder
/// [`decode_qbit_auxpow_proof`].
struct QbitRawAuxpow {
    coinbase_txid: Txid,
    coinbase_script: Vec<u8>,
    coinbase_outputs: Vec<TxOut>,
    coinbase_branch: MerkleBranch,
    chain_branch: MerkleBranch,
    parent_header: ParsedHeader,
}

/// Read the Qbit AuxPoW payload in exact wire order: non-witness parent
/// coinbase transaction, parent branch + i32 index, chain branch + i32
/// index, 80-byte parent header. Structural only; the commitment and
/// proof-of-work gates live in `verify_qbit_proof`.
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

/// Verify the Qbit proof envelope over a structurally read payload, in the
/// reference adapter's order: coinbase branch index exactly 0; chain index
/// inside the tree width; parent coinbase merkle inclusion (wire order);
/// display-order chain commitment with marker-or-legacy placement plus the
/// `[tree_size:4][nonce:4]` footer and deterministic LCG slot; and the
/// parent header hash meeting the CHILD target.
fn verify_qbit_proof(
    child_header: &ParsedHeader,
    auxpow: &QbitRawAuxpow,
    child_target: Target,
) -> Result<()> {
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
    let (tree_size, nonce) = read_qbit_commitment_footer(&auxpow.coinbase_script, &display_root)?;
    ensure!(
        tree_size == 1u32 << chain_len,
        "Qbit commitment tree size {tree_size} != 1 << {chain_len}"
    );
    let expected_slot = auxpow_expected_index(nonce, QBIT_AUXPOW_CHAIN_ID, chain_len);
    ensure!(
        chain_index as u32 == expected_slot,
        "Qbit commitment chain slot {chain_index} != deterministic slot {expected_slot}"
    );

    ensure!(
        child_target.is_met_by(auxpow.parent_header.hash()),
        "Qbit parent proof fails child header target"
    );
    Ok(())
}

/// Locate the display-order commitment in the parent coinbase scriptSig
/// (first occurrence) and return its `[tree_size:4][nonce:4]` footer. When
/// `fabe6d6d` is present it must occur exactly once and sit immediately
/// before the root; without it the legacy rule requires the root to start
/// at byte offset <= 20.
fn read_qbit_commitment_footer(script: &[u8], display_root: &[u8; 32]) -> Result<(u32, u32)> {
    let root_pos = find_subslice(script, display_root).context(
        "Qbit display-order chain commitment missing from the parent coinbase scriptSig",
    )?;
    match find_subslice(script, &AUXPOW_MAGIC) {
        Some(magic_pos) => {
            ensure!(
                find_subslice(&script[magic_pos + 1..], &AUXPOW_MAGIC).is_none(),
                "multiple Qbit merged-mining markers in the parent coinbase scriptSig"
            );
            ensure!(
                magic_pos + AUXPOW_MAGIC.len() == root_pos,
                "Qbit merged-mining marker must immediately precede the commitment"
            );
        }
        None => ensure!(
            root_pos <= 20,
            "Qbit legacy commitment starts beyond scriptSig byte 20"
        ),
    }
    ensure!(
        script.len() >= root_pos + 40,
        "truncated Qbit commitment footer"
    );
    let tree_size = u32::from_le_bytes(script[root_pos + 32..root_pos + 36].try_into().unwrap());
    let nonce = u32::from_le_bytes(script[root_pos + 36..root_pos + 40].try_into().unwrap());
    Ok((tree_size, nonce))
}

/// First occurrence of `needle` in `haystack`, or `None`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The decoded structure of a stored Qbit proof blob: the slot index, the
/// embedded parent header hash, and the two merkle branches. Deliberately a
/// distinct type from [`AuxpowProofDetail`]: the Qbit wire format has no
/// `hashBlock`, so the classic field stays classic-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QbitProofDetail {
    pub slot_index: u32,
    pub parent_header_hash: BlockHash,
    pub coinbase_branch: AuxMerkleBranchDetail,
    pub blockchain_branch: AuxMerkleBranchDetail,
}

/// Re-parse a stored Qbit proof blob (the `auxpow_bytes` region: coinbase tx
/// through parent header) into its branch breakdown. Structural decode only,
/// mirroring what classic [`decode_auxpow_proof`] does for its format: full
/// consumption required, both branch indices non-negative, no commitment
/// re-verification (the child header is not in the blob). Classic CAuxPow
/// bytes are not accepted here, and Qbit bytes are not accepted by the
/// classic decoder: format selection is explicit, never sniffed.
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
