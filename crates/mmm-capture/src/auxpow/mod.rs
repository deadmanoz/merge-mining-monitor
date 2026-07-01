//! Namecoin-family AuxPoW parsing.
//!
//! Namecoin-family producers work from raw `getblock <hash> 0` bytes. Namecoin's
//! AuxPoW payload sits between the 80-byte child header and the child
//! transaction vector when the child version carries `VERSION_AUXPOW`.
//!
//! Hashes are stored as rust-bitcoin newtypes (`BlockHash`, `Txid`,
//! `TxMerkleNode`). Bytes-in-memory are wire (internal) byte order matching
//! Bitcoin Core's `uint256` convention; `Display`/`to_string()` reverses to
//! produce the hex form used by RPC and block explorers.

use anyhow::{Context, Result, bail, ensure};
use bitcoin::block::Header;
use bitcoin::consensus::{deserialize, deserialize_partial, serialize};
use bitcoin::hashes::{Hash as _, sha256d};
use bitcoin::{
    Address, Block, BlockHash, CompactTarget, Network, Target, Transaction, TxMerkleNode, Txid,
};

pub use bitcoin::TxOut;

/// The child-header version bit (`1 << 8`) that flags a Namecoin-family block as
/// carrying an AuxPoW payload after the 80-byte header. `parse_namecoin_block`
/// and `parse_auxpow_header_blob` gate on `version & VERSION_AUXPOW != 0`;
/// Elastos has no such bit (its child version is 0, AuxPoW is structural).
pub const VERSION_AUXPOW: i32 = 1 << 8;

/// The outcome of parsing full `getblock <hash> 0` bytes for a Namecoin-family
/// chain: either a plain child header (no AuxPoW version bit) or a fully parsed
/// AuxPoW block. `parse_namecoin_block` returns this so the producer can skip
/// non-AuxPoW blocks without allocating proof state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedNamecoinBlock {
    /// A child header that does NOT carry the `VERSION_AUXPOW` bit: there is no
    /// AuxPoW payload to capture, so the producer skips it.
    NonAuxpow(ParsedHeader),
    /// A fully parsed AuxPoW block, boxed to keep the enum small (the payload is
    /// large relative to the header-only variant).
    Auxpow(Box<ParsedAuxpowBlock>),
}

/// A parsed 80-byte Bitcoin block header (rust-bitcoin `Header`). Used for both
/// child and parent headers across the AuxPoW types, and for the Elastos 80-byte
/// prefix header (whose real block hash includes a trailing height field and is
/// computed by the caller, NOT via [`hash`](ParsedHeader::hash)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedHeader {
    pub header: Header,
}

impl ParsedHeader {
    /// The Bitcoin block hash of this 80-byte header (double-SHA256, wire byte
    /// order). For an Elastos prefix header this is NOT the Elastos block hash:
    /// Elastos hashes an 84-byte header (the 80-byte prefix plus a trailing
    /// height), so the caller computes that hash separately.
    pub fn hash(&self) -> BlockHash {
        self.header.block_hash()
    }

    /// The header `nTime` (seconds). Carried for nBits-table horizon
    /// classification of the parent header by time.
    pub fn time(&self) -> u32 {
        self.header.time
    }

    /// The header `nBits` compact target. Fed to `validates_target` for the
    /// PoW-target gate and to the nBits-table difficulty classification.
    pub fn bits(&self) -> CompactTarget {
        self.header.bits
    }

    /// Re-serialise the header to its 80-byte consensus encoding (wire order),
    /// round-tripping it back to the stored `*_header` BYTEA.
    pub fn consensus_bytes(&self) -> Vec<u8> {
        serialize(&self.header)
    }
}

/// A fully parsed AuxPoW block: the child header plus its (optionally absent)
/// coinbase identity, the parent header and parent coinbase identity, and the
/// `CAuxPow` proof. `auxpow_bytes` is exactly the consumed `CAuxPow` region (no
/// child-header prefix, no trailing tx vector); it is stored as
/// `merge_mining_event.aux_merkle_proof` and re-decoded by `decode_auxpow_proof`
/// on the read path. The `child_coinbase_*` fields are absent for
/// header+CAuxPow-only blobs (Fractal, Elastos), where the producer supplies the
/// child height from RPC instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAuxpowBlock {
    pub child_header: ParsedHeader,
    pub child_height: Option<i32>,
    pub child_coinbase_txid: Option<Txid>,
    pub child_coinbase_script: Option<Vec<u8>>,
    pub child_coinbase_outputs: Vec<TxOut>,
    pub child_coinbase_output_addresses: Vec<String>,
    pub parent_header: ParsedHeader,
    pub proof: AuxpowProof,
    pub parent_coinbase_txid: Txid,
    pub parent_coinbase_script: Vec<u8>,
    pub parent_coinbase_outputs: Vec<TxOut>,
    pub parent_coinbase_output_addresses: Vec<String>,
    pub auxpow_bytes: Vec<u8>,
}

/// Child reward identity parsed from FULL child block bytes (Fractal's
/// `getblock <hash> 0`), used to enrich a header+CAuxPow-only proof that lacked a
/// child tx vector. Produced by [`parse_child_block_coinbase`] (which first
/// verifies the child tx merkle root against its header) and attached via
/// [`attach_child_block_coinbase`] under a child-header-hash equality gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedChildBlockCoinbase {
    pub child_header: ParsedHeader,
    pub child_height: Option<i32>,
    pub child_coinbase_txid: Txid,
    pub child_coinbase_script: Vec<u8>,
    pub child_coinbase_outputs: Vec<TxOut>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MerkleBranch {
    /// Sibling hashes from leaf up to root, wire (internal) byte order.
    pub hashes: Vec<TxMerkleNode>,
    /// CMerkleBranch nIndex: left/right sibling bitmask.
    pub index: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuxpowProof {
    /// `CAuxPow::hashBlock`. Namecoin conventionally writes this as zero
    /// because the parent block header is already serialised later in the
    /// payload. Do not assume equality with [`ParsedHeader::hash`].
    pub hash_block: BlockHash,
    /// `CAuxPow::(vMerkleBranch, nIndex)`: proves the parent coinbase is
    /// in the parent block's merkle tree.
    pub coinbase_branch: MerkleBranch,
    /// `CAuxPow::(vChainMerkleBranch, nChainIndex)`: proves the child
    /// block hash is in the merged-mining chain merkle tree committed by
    /// the parent coinbase.
    pub chain_branch: MerkleBranch,
}

mod decode;
mod reader;
#[cfg(test)]
mod tests;
mod verify;

pub use decode::*;
pub(crate) use reader::*;
pub use verify::*;

/// Parse full `getblock <hash> 0` bytes for a Namecoin-family chain (Namecoin,
/// Syscoin). Layout: 80-byte child header, then if the child version carries
/// `VERSION_AUXPOW` the `CAuxPow` payload, then the child transaction vector
/// (varint count + transactions). Returns `NonAuxpow` when the version bit is
/// clear. The child coinbase identity is read from the first child transaction;
/// child outputs are NOT formatted as Bitcoin addresses (the embedded BTC pool
/// snapshot carries the BTC payout addresses). `auxpow_bytes` captures only the
/// `CAuxPow` byte range between header end and tx-vector start.
pub fn parse_namecoin_block(raw: &[u8]) -> Result<ParsedNamecoinBlock> {
    ensure!(
        raw.len() >= Header::SIZE,
        "block is shorter than an 80-byte header"
    );

    let child_header =
        parse_header(raw[0..Header::SIZE].try_into().unwrap()).context("parse child header")?;
    if child_header.header.version.to_consensus() & VERSION_AUXPOW == 0 {
        return Ok(ParsedNamecoinBlock::NonAuxpow(child_header));
    }

    let mut reader = Reader::new(raw);
    reader.skip(Header::SIZE)?;
    let auxpow_start = reader.position();
    let auxpow = read_auxpow(&mut reader).context("parse AuxPoW payload")?;
    let auxpow_end = reader.position();

    let child = if !reader.is_eof() {
        let tx_count = reader.read_varint_usize().context("read child tx count")?;
        ensure!(tx_count > 0, "AuxPoW child block has no transactions");
        let coinbase = read_transaction(&mut reader).context("parse child coinbase transaction")?;
        let script = coinbase.first_input_script.clone();
        ChildCoinbase {
            height: script.as_deref().and_then(parse_bip34_height),
            txid: Some(coinbase.txid),
            script,
            outputs: coinbase.outputs,
            // The embedded BTC pool snapshot carries BTC payout addresses.
            // Avoid formatting Namecoin child outputs as Bitcoin addresses.
            output_addresses: Vec::new(),
        }
    } else {
        bail!("AuxPoW child block is missing its transaction vector");
    };

    Ok(ParsedNamecoinBlock::Auxpow(Box::new(
        assemble_auxpow_block(
            child_header,
            child,
            auxpow,
            raw[auxpow_start..auxpow_end].to_vec(),
        ),
    )))
}

/// The child-coinbase-derived fields of a parsed AuxPoW block. Absent for a
/// `[child header][CAuxPow]` blob that carries no child transaction vector
/// (Fractal's `getblockheader <hash> false true`), where the producer supplies
/// the child height from the RPC instead.
struct ChildCoinbase {
    height: Option<i32>,
    txid: Option<Txid>,
    script: Option<Vec<u8>>,
    outputs: Vec<TxOut>,
    output_addresses: Vec<String>,
}

impl ChildCoinbase {
    /// All-absent child-coinbase fields, for header+CAuxPow-only blobs (Fractal,
    /// Elastos) where the producer supplies the child height from RPC.
    fn absent() -> Self {
        Self {
            height: None,
            txid: None,
            script: None,
            outputs: Vec::new(),
            output_addresses: Vec::new(),
        }
    }
}

/// Assemble a [`ParsedAuxpowBlock`] from a parsed child header, the (possibly
/// absent) child-coinbase fields, and the parsed CAuxPoW payload. Shared by
/// [`parse_namecoin_block`] (full `getblock 0` bytes) and
/// [`parse_auxpow_header_blob`] (header+CAuxPow-only blobs).
fn assemble_auxpow_block(
    child_header: ParsedHeader,
    child: ChildCoinbase,
    auxpow: ParsedAuxpow,
    auxpow_bytes: Vec<u8>,
) -> ParsedAuxpowBlock {
    let parent_coinbase_script = auxpow.coinbase_tx.first_input_script.unwrap_or_default();
    ParsedAuxpowBlock {
        child_header,
        child_height: child.height,
        child_coinbase_txid: child.txid,
        child_coinbase_script: child.script,
        child_coinbase_outputs: child.outputs,
        child_coinbase_output_addresses: child.output_addresses,
        parent_header: auxpow.parent_header,
        proof: AuxpowProof {
            hash_block: auxpow.hash_block,
            coinbase_branch: auxpow.coinbase_branch,
            chain_branch: auxpow.chain_branch,
        },
        parent_coinbase_txid: auxpow.coinbase_tx.txid,
        parent_coinbase_script,
        parent_coinbase_outputs: auxpow.coinbase_tx.outputs.clone(),
        parent_coinbase_output_addresses: output_addresses(&auxpow.coinbase_tx.outputs),
        auxpow_bytes,
    }
}

/// Parse a `[child header][CAuxPow]` blob that has NO trailing child transaction
/// vector, as returned by Fractal Bitcoin's `getblockheader <hash> false true`.
///
/// Unlike [`parse_namecoin_block`] (which works from full `getblock <hash> 0`
/// bytes and requires the child tx vector), this stops at the end of the CAuxPow
/// and leaves the child-coinbase fields absent; the caller supplies the child
/// height from the RPC. The caller is responsible for any chain-specific
/// exact-version gate before calling this (Fractal gates on `0x20240100`).
pub fn parse_auxpow_header_blob(raw: &[u8]) -> Result<ParsedAuxpowBlock> {
    ensure!(
        raw.len() >= Header::SIZE,
        "blob is shorter than an 80-byte header"
    );

    let child_header =
        parse_header(raw[0..Header::SIZE].try_into().unwrap()).context("parse child header")?;
    ensure!(
        child_header.header.version.to_consensus() & VERSION_AUXPOW != 0,
        "header+CAuxPow blob does not carry the AuxPoW version bit"
    );

    let mut reader = Reader::new(raw);
    reader.skip(Header::SIZE)?;
    let auxpow_start = reader.position();
    let auxpow = read_auxpow(&mut reader).context("parse AuxPoW payload")?;
    let auxpow_end = reader.position();

    Ok(assemble_auxpow_block(
        child_header,
        ChildCoinbase::absent(),
        auxpow,
        raw[auxpow_start..auxpow_end].to_vec(),
    ))
}

/// Parse the full child block bytes used to enrich a header+CAuxPoW-only
/// proof with child reward identity. The block transaction merkle root must
/// match its header before any coinbase identity is trusted.
pub fn parse_child_block_coinbase(raw: &[u8]) -> Result<ParsedChildBlockCoinbase> {
    let block: Block = deserialize(raw).context("deserialize full child block")?;
    ensure!(
        block.check_merkle_root(),
        "child block transaction merkle root does not match header"
    );
    let coinbase = block
        .txdata
        .first()
        .ok_or_else(|| anyhow::anyhow!("child block has no transactions"))?;
    ensure!(
        coinbase.is_coinbase(),
        "child block first transaction is not coinbase"
    );
    let input = coinbase
        .input
        .first()
        .ok_or_else(|| anyhow::anyhow!("child coinbase transaction has no inputs"))?;
    let script = input.script_sig.as_bytes().to_vec();

    Ok(ParsedChildBlockCoinbase {
        child_header: ParsedHeader {
            header: block.header,
        },
        child_height: parse_bip34_height(&script),
        child_coinbase_txid: coinbase.compute_txid(),
        child_coinbase_script: script,
        child_coinbase_outputs: coinbase.output.clone(),
    })
}

/// Attach full child block coinbase fields to an already-parsed
/// `[child header][CAuxPow]` proof. The hash-pair check prevents a producer
/// from combining a proof for one Fractal child block with reward bytes from
/// another.
pub fn attach_child_block_coinbase(
    parsed: &mut ParsedAuxpowBlock,
    child: ParsedChildBlockCoinbase,
) -> Result<()> {
    ensure!(
        parsed.child_header.hash() == child.child_header.hash(),
        "full child block hash does not match AuxPoW proof child header"
    );

    parsed.child_height = parsed.child_height.or(child.child_height);
    parsed.child_coinbase_txid = Some(child.child_coinbase_txid);
    parsed.child_coinbase_script = Some(child.child_coinbase_script);
    parsed.child_coinbase_outputs = child.child_coinbase_outputs;
    parsed.child_coinbase_output_addresses = Vec::new();
    Ok(())
}

/// Elastos merged-mining AuxPoW chain id: the slot the parent coinbase commits
/// the Elastos block hash into. Verified against live blocks (the LCG slot
/// derivation matches `nChainIndex` at ELA heights 360062 / 1500000 / 2000000).
pub const ELASTOS_AUXPOW_CHAIN_ID: u32 = 1224;

/// Maximum accepted Elastos `auxpow` blob size. A real Elastos CAuxPow is
/// ~0.9-1.0 KB; this bounds an untrusted public-RPC response before parsing
/// (defense in depth behind the RPC-client hex cap).
pub const MAX_ELASTOS_AUXPOW_BYTES: usize = 4096;

/// The merged-mining commitment magic `fabe6d6d` that marks the start of the
/// `[magic][aux_merkle_root:32][tree_size:4][nonce:4]` blob inside a parent
/// coinbase scriptSig.
const AUXPOW_MAGIC: [u8; 4] = [0xfa, 0xbe, 0x6d, 0x6d];

/// Parse an Elastos `auxpow` field: a CAuxPow-only blob (parent coinbase tx,
/// hashBlock, the parent and chain merkle branches, parent header) with NO child
/// transaction vector and NO child-header prefix.
///
/// Unlike [`parse_namecoin_block`] / [`parse_auxpow_header_blob`], the Elastos
/// child header is NOT in the blob and is NOT a Bitcoin 80-byte header: an Elastos
/// block hashes an 84-byte header (the 80-byte Bitcoin prefix plus a trailing
/// `height` field). `child_header` here is that 80-byte prefix, carried only for
/// its `bits`/`time`; the real Elastos block hash (which includes the height) is
/// computed by the caller and passed to [`verify_auxpow_commitment`] and the event
/// evidence. There is no version-bit gate (Elastos child version is 0; AuxPoW is
/// structural).
pub fn parse_elastos_auxpow(
    child_header: ParsedHeader,
    auxpow_blob: &[u8],
) -> Result<ParsedAuxpowBlock> {
    ensure!(
        auxpow_blob.len() <= MAX_ELASTOS_AUXPOW_BYTES,
        "Elastos auxpow blob is {} bytes, over the {MAX_ELASTOS_AUXPOW_BYTES}-byte cap",
        auxpow_blob.len()
    );
    let mut reader = Reader::new(auxpow_blob);
    let auxpow = read_auxpow(&mut reader).context("parse Elastos CAuxPow blob")?;
    // The Elastos `auxpow` field is exactly the CAuxPow region: a correct parse
    // consumes it fully. Trailing bytes are malformed, and storing only the
    // consumed region keeps `decode_auxpow_proof` (the /block read path) able to
    // re-parse the stored `aux_merkle_proof`.
    ensure!(
        reader.is_eof(),
        "trailing bytes after the Elastos CAuxPow blob"
    );
    Ok(assemble_auxpow_block(
        child_header,
        ChildCoinbase::absent(),
        auxpow,
        auxpow_blob.to_vec(),
    ))
}

/// The narrow stored-bytes read API for presentation layers.
///
/// Downstream code (the api projection, fixture-contract tests) decodes
/// ALREADY-STORED CAuxPow blobs and coinbase scripts that this parser wrote.
/// Re-exporting exactly that decoding API here names the sanctioned
/// api -> capture edge: consumers depend on `auxpow::evidence`, never on the
/// parser internals, so wire-format knowledge stays in one module.
pub mod evidence {
    pub use super::{
        AuxMarker, TxOut, decode_aux_marker, decode_auxpow_proof, extract_coinbase_tag,
        output_addresses,
    };
}
