//! Shared cfg(test) fixtures for the RSK capture and traversal test modules.

use std::path::{Path, PathBuf};

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
            (
                "canonical-pre-floor-full-header",
                "0x1b8bd",
                "1c070e00b0d3739795b22e0ee036a3ef9cc7cdc0",
            ),
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
