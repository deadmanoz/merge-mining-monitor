//! Shared test helpers for unit tests and external integration-test binaries (`test-support` feature).

use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget, Target, TxMerkleNode};

/// Build a BTC header satisfying `bits` by grinding the nonce (`merkle_seed` in the first 4 merkle-root bytes).
pub fn header_meeting_bits(bits: u32, time: u32, merkle_seed: u32) -> Header {
    header_meeting_bits_with_prev(bits, time, merkle_seed, BlockHash::all_zeros())
}

pub fn header_meeting_bits_with_prev(
    bits: u32,
    time: u32,
    merkle_seed: u32,
    prev_blockhash: BlockHash,
) -> Header {
    let target = Target::from_compact(CompactTarget::from_consensus(bits));
    let mut merkle = [0u8; 32];
    merkle[..4].copy_from_slice(&merkle_seed.to_le_bytes());
    let mut header = Header {
        version: Version::ONE,
        prev_blockhash,
        merkle_root: TxMerkleNode::from_byte_array(merkle),
        time,
        bits: CompactTarget::from_consensus(bits),
        nonce: 0,
    };
    loop {
        if target.is_met_by(header.block_hash()) {
            return header;
        }
        header.nonce = match header.nonce.checked_add(1) {
            Some(next) => next,
            None => panic!(
                "header_meeting_bits exhausted the u32 nonce space for bits \
                 {bits:#010x} (time={time}, merkle_seed={merkle_seed})"
            ),
        };
    }
}

/// Path to `fixtures/<name>.<extension>` anchored at the workspace root.
pub fn fixture_path(name: &str, extension: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("mmm-capture crate lives under workspace crates/")
        .join("fixtures")
        .join(format!("{name}.{extension}"))
}

pub fn load_raw_namecoin_fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(&format!("namecoin/{name}"), "bin");
    std::fs::read(&path).unwrap_or_else(|err| panic!("failed to read fixture {name}: {err}"))
}

pub fn parse_auxpow_fixture(name: &str) -> anyhow::Result<Box<crate::auxpow::ParsedAuxpowBlock>> {
    let raw = std::fs::read(fixture_path(&format!("namecoin/{name}"), "bin"))?;
    match crate::auxpow::parse_namecoin_block(&raw)? {
        crate::auxpow::ParsedNamecoinBlock::Auxpow(parsed) => Ok(parsed),
        crate::auxpow::ParsedNamecoinBlock::NonAuxpow(_) => {
            anyhow::bail!("{name} is not AuxPoW")
        }
    }
}
