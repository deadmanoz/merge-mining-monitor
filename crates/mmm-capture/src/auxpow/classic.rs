//! Classic full-block decoding and strict evidence authentication.
use super::*;

/// Decode the shared header/proof prefix once and retain the transaction offset.
pub(super) fn read_classic_prefix(
    raw: &[u8],
) -> Result<(ParsedHeader, Option<(ParsedAuxpow, usize)>)> {
    ensure!(
        raw.len() >= Header::SIZE,
        "block is shorter than an 80-byte header"
    );
    let mut reader = Reader::new(raw);
    let header =
        parse_header(reader.read_array::<{ Header::SIZE }>()?).context("parse child header")?;
    if header.header.version.to_consensus() & VERSION_AUXPOW == 0 {
        return Ok((header, None));
    }
    let proof = read_auxpow(&mut reader).context("parse AuxPoW payload")?;
    Ok((header, Some((proof, reader.position()))))
}

/// Authenticate classic AuxPoW evidence against the supplied child identity.
/// This is not full child-chain consensus validation. Non-AuxPoW blocks retain
/// the existing header-authenticated skip contract; their bodies are not read.
pub fn parse_verified_classic_block(
    raw: &[u8],
    requested_hash: BlockHash,
    requested_height: i32,
    chain_id: u32,
) -> Result<ParsedNamecoinBlock> {
    let (header, prefix) = read_classic_prefix(raw)?;
    ensure!(
        header.hash() == requested_hash,
        "child block does not match requested hash"
    );
    let Some((auxpow, body_offset)) = prefix else {
        return Ok(ParsedNamecoinBlock::NonAuxpow(header));
    };
    ensure!(
        auxpow.coinbase_tx.is_coinbase,
        "parent transaction is not coinbase"
    );
    ensure!(
        (header.header.version.to_consensus() as u32 >> 16) == chain_id,
        "unexpected child chain ID"
    );
    // Strict chain IDs: consensus rejects a parent that carries the child's own
    // chain ID, even when every commitment and the child target check pass.
    ensure!(
        (auxpow.parent_header.header.version.to_consensus() as u32 >> 16) != chain_id,
        "parent header carries the child chain ID"
    );
    // The classic proof interrupts Bitcoin block serialization. Reconstruct the
    // authenticated child body here, where ownership of the wire layout lives.
    let mut child_bytes = raw[..Header::SIZE].to_vec();
    child_bytes.extend_from_slice(&raw[body_offset..]);
    let child = parse_child_block_coinbase_with_height(&child_bytes, parse_child_bip34_height)?;
    ensure!(
        child.child_height == Some(requested_height),
        "child coinbase height does not match RPC height"
    );
    let proof = assemble_auxpow_block(
        header,
        ChildCoinbase {
            height: child.child_height,
            txid: Some(child.child_coinbase_txid),
            script: Some(child.child_coinbase_script),
            outputs: child.child_coinbase_outputs,
            output_addresses: Vec::new(),
        },
        auxpow,
        raw[Header::SIZE..body_offset].to_vec(),
    );
    verify_classic_auxpow_commitment(&proof, requested_hash, chain_id)?;
    ensure!(
        validates_target(proof.parent_header.hash(), proof.child_header.bits()),
        "parent work does not satisfy child target"
    );
    Ok(ParsedNamecoinBlock::Auxpow(Box::new(proof)))
}

#[cfg(test)]
#[path = "classic_tests.rs"]
mod tests;
