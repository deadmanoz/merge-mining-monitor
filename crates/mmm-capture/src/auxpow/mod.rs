//! Namecoin-family AuxPoW and Qbit native proof parsing.
//!
//! Hashes use rust-bitcoin newtypes (`BlockHash`, `Txid`, `TxMerkleNode`) in
//! internal wire byte order (`to_byte_array()`), reversing only at display/RPC
//! boundaries.

use anyhow::{Context, Result, bail, ensure};
use bitcoin::block::Header;
use bitcoin::consensus::{deserialize, deserialize_partial, serialize};
use bitcoin::hashes::{Hash as _, sha256d};
use bitcoin::{
    Address, Block, BlockHash, CompactTarget, Network, Target, Transaction, TxMerkleNode, Txid,
};

pub use bitcoin::TxOut;

/// Child-header version bit (`1 << 8`) flagging a Namecoin-family AuxPoW block.
pub const VERSION_AUXPOW: i32 = 1 << 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedNamecoinBlock {
    NonAuxpow(ParsedHeader),
    Auxpow(Box<ParsedAuxpowBlock>),
}

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

    pub fn time(&self) -> u32 {
        self.header.time
    }

    pub fn bits(&self) -> CompactTarget {
        self.header.bits
    }

    pub fn consensus_bytes(&self) -> Vec<u8> {
        serialize(&self.header)
    }
}

/// Parsed AuxPoW block. `auxpow_bytes` is the exact `CAuxPow` region stored in
/// `merge_mining_event.aux_merkle_proof`; `child_coinbase_*` fields are `None`
/// for header+`CAuxPow`-only blobs (Fractal, Elastos).
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

/// Child coinbase identity authenticated against full child block bytes.
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
    pub coinbase_branch: MerkleBranch,
    pub chain_branch: MerkleBranch,
}

mod decode;
mod qbit;
#[cfg(test)]
mod qbit_tests;
mod reader;
#[cfg(test)]
mod tests;
mod verify;

pub use decode::*;
pub use qbit::*;
pub(crate) use reader::*;
pub use verify::*;

/// Parse full `getblock <hash> 0` bytes (`[child header][CAuxPow][child txs]`)
/// for a Namecoin-family chain.
pub fn parse_namecoin_block(raw: &[u8]) -> Result<ParsedNamecoinBlock> {
    let (child_header, prefix) = read_classic_prefix(raw)?;
    let Some((auxpow, auxpow_end)) = prefix else {
        return Ok(ParsedNamecoinBlock::NonAuxpow(child_header));
    };
    let auxpow_start = Header::SIZE;
    let mut reader = Reader::new(raw);
    reader.skip(auxpow_end)?;

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

struct ChildCoinbase {
    height: Option<i32>,
    txid: Option<Txid>,
    script: Option<Vec<u8>>,
    outputs: Vec<TxOut>,
    output_addresses: Vec<String>,
}

impl ChildCoinbase {
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

/// Parse a `[child header][CAuxPow]` blob with no trailing child transaction
/// vector (e.g. Fractal Bitcoin's `getblockheader <hash> false true`).
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

/// Parse full child block bytes and verify the transaction merkle root against
/// the header before extracting the child coinbase.
pub fn parse_child_block_coinbase(raw: &[u8]) -> Result<ParsedChildBlockCoinbase> {
    parse_child_block_coinbase_with_height(raw, parse_bip34_height)
}

fn parse_child_block_coinbase_with_height(
    raw: &[u8],
    height_parser: fn(&[u8]) -> Option<i32>,
) -> Result<ParsedChildBlockCoinbase> {
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
        child_height: height_parser(&script),
        child_coinbase_txid: coinbase.compute_txid(),
        child_coinbase_script: script,
        child_coinbase_outputs: coinbase.output.clone(),
    })
}

/// Attach authenticated child coinbase fields to a `[child header][CAuxPow]`
/// proof after verifying the child header hashes match.
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

pub const ELASTOS_AUXPOW_CHAIN_ID: u32 = 1224;
pub const MAX_ELASTOS_AUXPOW_BYTES: usize = 4096;
const AUXPOW_MAGIC: [u8; 4] = [0xfa, 0xbe, 0x6d, 0x6d];

/// Parse an Elastos `CAuxPow`-only blob.
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

/// Stored-bytes read API for presentation layers (`mmm-api`).
pub mod evidence {
    pub use super::{
        AuxMarker, AuxMerkleBranchDetail, TxOut, decode_aux_marker, decode_auxpow_proof,
        decode_qbit_auxpow_proof, extract_coinbase_tag, output_addresses,
    };
}

mod classic;
pub use classic::parse_verified_classic_block;
use classic::read_classic_prefix;
