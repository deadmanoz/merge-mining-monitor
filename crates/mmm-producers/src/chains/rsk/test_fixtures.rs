//! Shared cfg(test) fixtures for the RSK capture and traversal test modules.

use std::path::{Path, PathBuf};

#[cfg(test)]
use bitcoin::block::Header;
#[cfg(test)]
use bitcoin::consensus::serialize;

use crate::chains::rsk::rpc::RskBlock;

/// Miner address present in the test registry (resolves to `f2pool`).
#[cfg(test)]
pub(crate) const KNOWN_MINER_HEX: &str = "12d3178a62ef1f520944534ed04504609f7307a1";
/// Miner address absent from the test registry (stays unresolved).
#[cfg(test)]
pub(crate) const UNKNOWN_MINER_HEX: &str = "0123456789abcdef0123456789abcdef01234567";
/// A second distinct miner address, for multi-uncle ordering fixtures.
#[cfg(test)]
pub(crate) const SECOND_MINER_HEX: &str = "4e5dabc28e4a0f5e5b19fcb56b28c5a1989352c1";

/// Workspace path to the `fixtures/rsk/<name>.json` RPC fixture (resolved
/// relative to this crate's manifest dir).
fn rsk_fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("mmm-producers crate lives under workspace crates/")
        .join("fixtures/rsk")
        .join(format!("{name}.json"))
}

/// Deserialize a named `fixtures/rsk` file into an [`RskBlock`], panicking with
/// the path on read/parse failure (test-only helper).
pub fn load_rsk_block_fixture(name: &str) -> RskBlock {
    let path = rsk_fixture_path(name);
    let json = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read RSK fixture {}: {err}", path.display()));
    serde_json::from_str(&json).unwrap_or_else(|err| {
        panic!(
            "failed to deserialize RSK fixture {}: {err}",
            path.display()
        )
    })
}

/// An 80-byte BTC parent header whose PoW satisfies `bits`, for fixtures that
/// need a parent passing (or, with a tight `bits`, failing) its own target.
#[cfg(test)]
pub(crate) fn header_meeting_bits(bits: u32) -> Header {
    mmm_capture::test_support::header_meeting_bits(bits, 1_700_000_000, 0)
}

/// Synthesize an [`RskBlock`] with all merge-mining fields populated as RSKj-style
/// `0x` hex, deriving a deterministic hash from `height` so prefetch-ordering
/// tests can recover the height from the block.
#[cfg(test)]
pub(crate) fn rsk_block_with(
    height: i64,
    timestamp: i64,
    miner_hex: &str,
    btc_header: Header,
    uncles: Vec<&str>,
) -> RskBlock {
    RskBlock {
        hash: format!("0x{:064x}", height),
        number: format!("0x{height:x}"),
        timestamp: format!("0x{timestamp:x}"),
        miner: format!("0x{miner_hex}"),
        difficulty: Some("0x1000".to_owned()),
        bitcoin_merged_mining_header: Some(format!("0x{}", hex::encode(serialize(&btc_header)))),
        bitcoin_merged_mining_coinbase_transaction: Some("0xdeadbeef".to_owned()),
        bitcoin_merged_mining_merkle_proof: Some("0xcafebabe".to_owned()),
        hash_for_merged_mining: Some(format!("0x{:064x}", 0xa5a5a5a5u32)),
        uncles: uncles.into_iter().map(str::to_owned).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backed_rsk_rpc_fixtures_deserialize() {
        for (name, number, miner) in [
            ("canonical-valid", "0xb1fa8", KNOWN_MINER_HEX),
            ("canonical-near", "0xc3500", UNKNOWN_MINER_HEX),
            ("uncle-valid", "0xc3501", KNOWN_MINER_HEX),
            ("canonical-with-uncles", "0xb1fa9", KNOWN_MINER_HEX),
            ("uncle-second-miner", "0xb200d", SECOND_MINER_HEX),
        ] {
            let block = load_rsk_block_fixture(name);
            assert_eq!(block.number, number, "{name} number");
            assert_eq!(block.miner, format!("0x{miner}"), "{name} miner");
            assert!(
                block
                    .bitcoin_merged_mining_header
                    .as_deref()
                    .is_some_and(|header| header.starts_with("0x")),
                "{name} must preserve the RSKj hex prefix"
            );
        }

        let pre_rskip92 = load_rsk_block_fixture("pre-rskip92");
        assert_eq!(
            pre_rskip92.bitcoin_merged_mining_header.as_deref(),
            Some("0x")
        );

        let malformed = load_rsk_block_fixture("malformed-header");
        assert_eq!(
            malformed.bitcoin_merged_mining_header.as_deref(),
            Some("0xnotvalidhex")
        );
    }
}
