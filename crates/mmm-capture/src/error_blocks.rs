//! Embedded consensus-invalid, full-proof-of-work Bitcoin error-block registry.
//!
//! The source of truth lives in `merge-mining-research`'s
//! `data/error-blocks/error_blocks.csv`. This compact pinned mirror lets live
//! capture classify a witnessed catalogued error block without asking Bitcoin
//! Core to treat an invalid block as stale or unknown. It intentionally carries
//! only the identity, canonical-context height, and primary rejection token;
//! the research dataset remains the complete evidence record.

use std::collections::HashMap;
use std::str::FromStr;
use std::sync::LazyLock;

use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;

const ERROR_BLOCKS_CSV: &str = include_str!("../../../data/consensus/error_blocks.csv");

/// Canonical rejection token for a parent whose timestamp is not strictly
/// greater than its predecessor median time past.
pub const TIME_BELOW_MTP: &str = "time_below_mtp";

/// Canonical rejection token for a header that carried the previous difficulty
/// epoch's target across a retarget boundary.
pub const NBITS_RETARGET_NOT_APPLIED: &str = "nbits_retarget_not_applied";

/// A pinned, mechanically re-derived Bitcoin consensus failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ErrorBlock {
    /// Height in the canonical context used by the research validator.
    pub height: i32,
    /// Primary `rejection_reason` token from the research error-block catalogue.
    pub rejection_reason: &'static str,
}

static ERROR_BLOCKS: LazyLock<HashMap<[u8; 32], ErrorBlock>> = LazyLock::new(parse_registry);

/// Return catalogued error-block metadata for a stored-order hash.
pub fn lookup(hash: &[u8]) -> Option<ErrorBlock> {
    let hash: [u8; 32] = hash.try_into().ok()?;
    ERROR_BLOCKS.get(&hash).copied()
}

/// Number of pinned catalogue entries, primarily for invariant tests and
/// operator-facing diagnostics.
pub fn len() -> usize {
    ERROR_BLOCKS.len()
}

/// Iterate every stored-order parent hash in the pinned catalogue.
pub fn hashes() -> impl Iterator<Item = [u8; 32]> {
    ERROR_BLOCKS.keys().copied()
}

fn parse_registry() -> HashMap<[u8; 32], ErrorBlock> {
    let mut entries = HashMap::new();
    for (line_number, line) in ERROR_BLOCKS_CSV.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') || line == "height,hash,rejection_reason" {
            continue;
        }
        let mut fields = line.split(',');
        let height = fields
            .next()
            .and_then(|value| value.parse::<i32>().ok())
            .filter(|height| *height >= 0)
            .unwrap_or_else(|| panic!("invalid error-block height at line {}", line_number + 1));
        let hash = fields
            .next()
            .and_then(|value| BlockHash::from_str(value).ok())
            .map(|hash| hash.to_byte_array())
            .unwrap_or_else(|| panic!("invalid error-block hash at line {}", line_number + 1));
        let rejection_reason = fields
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| panic!("invalid error-block reason at line {}", line_number + 1));
        assert!(
            fields.next().is_none(),
            "invalid error-block field count at line {}",
            line_number + 1
        );
        assert!(
            entries
                .insert(
                    hash,
                    ErrorBlock {
                        height,
                        rejection_reason,
                    },
                )
                .is_none(),
            "duplicate error-block hash at line {}",
            line_number + 1
        );
    }
    assert!(!entries.is_empty(), "error-block registry is empty");
    entries
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinned_registry_exposes_known_entries() {
        let hash =
            BlockHash::from_str("00000000000000000000c3d95a4bdc068dfe0c6d1e7ad13045c6f570e58d9ed7")
                .unwrap();
        assert_eq!(hashes().count(), len());
        assert_eq!(
            lookup(&hash.to_byte_array()),
            Some(ErrorBlock {
                height: 946_213,
                rejection_reason: TIME_BELOW_MTP,
            })
        );
        let hathor_649674 =
            BlockHash::from_str("00000000000000000008c80c1f8c101f8aa1fabd59d63ab1350bd1d5dba425e6")
                .unwrap();
        assert_eq!(
            lookup(&hathor_649674.to_byte_array()),
            Some(ErrorBlock {
                height: 649_674,
                rejection_reason: "bip34_coinbase_height_missing",
            })
        );
        let f2pool_957780 =
            BlockHash::from_str("0000000000000000000198e12592edbe83c84a78f75b3f8d67a3fe2075ef2ffb")
                .unwrap();
        assert_eq!(
            lookup(&f2pool_957780.to_byte_array()),
            Some(ErrorBlock {
                height: 957_780,
                rejection_reason: TIME_BELOW_MTP,
            })
        );
    }
}
