//! Bounded byte cursor and raw transaction, merkle branch, and `CAuxPow` readers.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedTransaction {
    pub(crate) txid: Txid,
    pub(crate) is_coinbase: bool,
    pub(crate) first_input_script: Option<Vec<u8>>,
    pub(crate) outputs: Vec<TxOut>,
}

pub(crate) fn parse_header(raw: [u8; Header::SIZE]) -> Result<ParsedHeader> {
    Ok(ParsedHeader {
        header: deserialize(&raw).context("deserialize Bitcoin block header")?,
    })
}

/// Read one `CAuxPow` record in wire order: parent coinbase tx, `hashBlock`,
/// coinbase merkle branch + i32 index, chain merkle branch + i32 index, and 80-byte parent header.
pub(crate) fn read_auxpow(reader: &mut Reader<'_>) -> Result<ParsedAuxpow> {
    let coinbase_tx = read_transaction(reader).context("parse parent coinbase transaction")?;
    let hash_block =
        BlockHash::from_byte_array(reader.read_array::<32>().context("read AuxPoW hashBlock")?);
    let coinbase_branch_hashes = read_merkle_branch(reader).context("read parent merkle branch")?;
    let coinbase_branch_index = reader.read_i32().context("read parent merkle index")?;
    let chain_branch_hashes =
        read_merkle_branch(reader).context("read child chain merkle branch")?;
    let chain_branch_index = reader.read_i32().context("read child chain index")?;
    let parent_header = parse_header(
        reader
            .read_array::<{ Header::SIZE }>()
            .context("read parent header")?,
    )
    .context("parse parent header")?;

    Ok(ParsedAuxpow {
        coinbase_tx,
        hash_block,
        coinbase_branch: MerkleBranch {
            hashes: coinbase_branch_hashes,
            index: coinbase_branch_index,
        },
        chain_branch: MerkleBranch {
            hashes: chain_branch_hashes,
            index: chain_branch_index,
        },
        parent_header,
    })
}

/// Read a `CMerkleBranch`, bounding `count` against `remaining / 32` before allocating
/// so a corrupt stored proof varint cannot trigger an OOM allocation.
pub(crate) fn read_merkle_branch(reader: &mut Reader<'_>) -> Result<Vec<TxMerkleNode>> {
    let count = reader.read_varint_usize()?;
    ensure!(
        count <= reader.remaining_slice().len() / 32,
        "merkle branch count {count} exceeds the {} remaining bytes",
        reader.remaining_slice().len()
    );
    let mut hashes = Vec::with_capacity(count);
    for _ in 0..count {
        hashes.push(TxMerkleNode::from_byte_array(
            reader
                .read_array::<32>()
                .context("read merkle branch hash")?,
        ));
    }
    Ok(hashes)
}

pub(crate) fn read_transaction(reader: &mut Reader<'_>) -> Result<ParsedTransaction> {
    let (tx, consumed) = deserialize_partial::<Transaction>(reader.remaining_slice())
        .context("parse transaction")?;
    reader.skip(consumed)?;

    Ok(ParsedTransaction {
        txid: tx.compute_txid(),
        is_coinbase: tx.is_coinbase(),
        first_input_script: tx
            .input
            .first()
            .map(|input| input.script_sig.as_bytes().to_vec()),
        outputs: tx.output,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedAuxpow {
    pub(crate) coinbase_tx: ParsedTransaction,
    pub(crate) hash_block: BlockHash,
    pub(crate) coinbase_branch: MerkleBranch,
    pub(crate) chain_branch: MerkleBranch,
    pub(crate) parent_header: ParsedHeader,
}

#[derive(Debug)]
pub(crate) struct Reader<'a> {
    pub(crate) buf: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    pub(crate) fn position(&self) -> usize {
        self.pos
    }

    pub(crate) fn is_eof(&self) -> bool {
        self.pos >= self.buf.len()
    }

    pub(crate) fn skip(&mut self, len: usize) -> Result<()> {
        self.read_slice(len).map(|_| ())
    }

    pub(crate) fn read_u8(&mut self) -> Result<u8> {
        Ok(self.read_array::<1>()?[0])
    }

    pub(crate) fn read_i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.read_array()?))
    }

    pub(crate) fn read_array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let slice = self.read_slice(N)?;
        Ok(slice.try_into().unwrap())
    }

    pub(crate) fn read_slice(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .context("reader position overflow")?;
        ensure!(
            end <= self.buf.len(),
            "unexpected end of input at byte {} while reading {} bytes",
            self.pos,
            len
        );
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn remaining_slice(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    pub(crate) fn read_varint_usize(&mut self) -> Result<usize> {
        usize::try_from(self.read_varint()?).context("varint value overflows usize")
    }

    pub(crate) fn read_varint(&mut self) -> Result<u64> {
        let first = self.read_u8()?;
        match first {
            0x00..=0xfc => Ok(first as u64),
            0xfd => Ok(u16::from_le_bytes(self.read_array()?) as u64),
            0xfe => Ok(u32::from_le_bytes(self.read_array()?) as u64),
            0xff => Ok(u64::from_le_bytes(self.read_array()?)),
        }
    }
}
