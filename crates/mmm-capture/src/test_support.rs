//! Canonical test-only helpers shared between the library's own unit tests
//! and the external integration-test binaries under `tests/`.
//!
//! Cargo compiles every `.rs` file under `tests/` as a separate crate that
//! depends on `merge_mining_monitor`, so a helper that must be reachable from
//! both `#[cfg(test)]` unit tests and those external binaries has to live in
//! the library crate. This module is gated `#[cfg(any(test, feature =
//! "test-support"))]`: the library's own unit tests reach it under the `test`
//! cfg, and the integration binaries enable it through the `test-support`
//! feature (pulled in transitively by `db-integration`).
//!
//! These functions own the single nonce-grinding loop for the BTC header
//! builders. Do not reintroduce a second copy in `tests/support/mod.rs` or in
//! mmm-store's `chains/rsk.rs`; call into here instead.

use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget, Target, TxMerkleNode};

/// Build a BTC header that satisfies `bits` by grinding the nonce.
///
/// `merkle_seed` is written little-endian into the first four bytes of the
/// merkle root so callers can produce distinct-but-deterministic headers; the
/// remaining bytes stay zero. A `merkle_seed` of `0` yields an all-zero merkle
/// root.
pub fn header_meeting_bits(bits: u32, time: u32, merkle_seed: u32) -> Header {
    header_meeting_bits_with_prev(bits, time, merkle_seed, BlockHash::all_zeros())
}

/// [`header_meeting_bits`] with an explicit `prev_blockhash`, for tests that
/// need a pow-valid header chained onto a specific predecessor (for example
/// the read-model mutation cascade tests).
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

/// Build a BTC header that fails `bits` by grinding the nonce. Header
/// construction matches [`header_meeting_bits`]; only the loop predicate is
/// inverted.
pub fn header_failing_bits(bits: u32, time: u32, merkle_seed: u32) -> Header {
    let target = Target::from_compact(CompactTarget::from_consensus(bits));
    let mut merkle = [0u8; 32];
    merkle[..4].copy_from_slice(&merkle_seed.to_le_bytes());
    let mut header = Header {
        version: Version::ONE,
        prev_blockhash: BlockHash::all_zeros(),
        merkle_root: TxMerkleNode::from_byte_array(merkle),
        time,
        bits: CompactTarget::from_consensus(bits),
        nonce: 0,
    };
    loop {
        if !target.is_met_by(header.block_hash()) {
            return header;
        }
        header.nonce = match header.nonce.checked_add(1) {
            Some(next) => next,
            None => panic!(
                "header_failing_bits exhausted the u32 nonce space for bits \
                 {bits:#010x} (time={time}, merkle_seed={merkle_seed})"
            ),
        };
    }
}

// ─── Shared integration-test fixture helpers (folded in from the old
//     tests/support/mod.rs at the workspace split) ─────────────────────────

/// Path to a parser fixture under the workspace `fixtures/<name>.<ext>`.
///
/// Anchored through mmm-capture's `CARGO_MANIFEST_DIR` (env! expands where THIS
/// crate is compiled), so the path resolves identically from every consuming
/// crate and working directory while the fixture files live in the shared
/// repository fixture namespace. `name` may include a fixture-family
/// directory, for example `namecoin/500000-valid-parent` or `syscoin/1973`.
pub fn fixture_path(name: &str, extension: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("mmm-capture crate lives under workspace crates/")
        .join("fixtures")
        .join(format!("{name}.{extension}"))
}

/// Panic-on-missing convenience wrapper for raw Namecoin fixture bytes.
pub fn load_raw_namecoin_fixture(name: &str) -> Vec<u8> {
    let path = fixture_path(&format!("namecoin/{name}"), "bin");
    std::fs::read(&path).unwrap_or_else(|err| panic!("failed to read fixture {name}: {err}"))
}

/// Parse a Namecoin AuxPoW fixture into the engine's `ParsedAuxpowBlock`.
///
/// Returns an `anyhow::Result` error on a missing/unreadable fixture rather
/// than panicking; fails if the fixture is a NonAuxpow block.
pub fn parse_auxpow_fixture(name: &str) -> anyhow::Result<Box<crate::auxpow::ParsedAuxpowBlock>> {
    let raw = std::fs::read(fixture_path(&format!("namecoin/{name}"), "bin"))?;
    match crate::auxpow::parse_namecoin_block(&raw)? {
        crate::auxpow::ParsedNamecoinBlock::Auxpow(parsed) => Ok(parsed),
        crate::auxpow::ParsedNamecoinBlock::NonAuxpow(_) => {
            anyhow::bail!("{name} is not AuxPoW")
        }
    }
}

/// Canonical "valid" BTC parent header used by RSK structure/capture tests.
pub fn valid_btc_header() -> Header {
    header_meeting_bits(0x207f_ffff, 1_700_000_000, 0)
}

/// A second valid BTC parent header (different time + merkle seed) used
/// where a test needs two distinct headers, for example a canonical block
/// and its uncle in the same suite.
pub fn valid_btc_header_two() -> Header {
    header_meeting_bits(0x207f_ffff, 1_700_000_001, 0x1111_1111)
}
