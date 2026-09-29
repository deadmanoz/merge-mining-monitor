//! Whether an AuxPoW parent header belongs to Bitcoin.
//!
//! Merge mining lets a child chain commit to any SHA-256 parent, so a parent
//! header that meets its own declared target can be a Bitcoin Cash block, and
//! a share that misses it can be a Bitcoin Cash share. This rule decides
//! lineage from local evidence only: where the header's prev sits in the
//! Bitcoin chain, the coinbase BIP34 height where the child chain carries real
//! Bitcoin coinbase data, and Bitcoin's difficulty history from the
//! Core-derived [`NbitsTable`]. Callers check the pinned error-block catalogue
//! first: a catalogued block can carry wrong bits by design (block 717,696
//! kept the previous epoch's bits).
//!
//! Bitcoin Core indexes a header only after its `bad-diffbits` check, so a
//! Bitcoin header's bits are exactly the epoch's bits for its height. A share
//! that misses its own target is not a block and never reaches that check,
//! which is why shares on a known Bitcoin prev are admitted whatever their
//! bits: pools briefly keep the previous epoch's bits in templates across a
//! retarget.

use bitcoin::CompactTarget;

use crate::nbits_table::{
    DAA_EPOCH_INTERVAL, NbitsLookup, NbitsTable, WeakVerdict, daa_epoch_start,
};

/// How far past the cached Core tip a coinbase height may claim to be before
/// it is another chain's height rather than a block the cache has not reached
/// yet: one day of Bitcoin blocks.
pub const FUTURE_HEIGHT_TOLERANCE: i32 = 144;

/// A header above the cached horizon on a prev the cache does not know is at
/// most a few blocks ahead of it. When a retarget falls that close to the
/// horizon, the header may already carry the next epoch's bits, which the
/// cache cannot know yet.
pub const RETARGET_PENDING_WINDOW: i32 = 6;

/// The lineage evidence for one parent header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParentLineageInput {
    pub bits: CompactTarget,
    pub time: i64,
    /// Whether the header's hash meets the target its own bits declare
    /// (`pow_validates_btc_target`). A header that misses it is a share.
    pub meets_own_target: bool,
    /// The Bitcoin height of the header's prev, when the prev is a canonical
    /// or stale block the Monitor holds, or the Core header cache's horizon.
    pub prev_height: Option<i32>,
    /// The coinbase BIP34 height, for chains whose parent coinbase is real
    /// Bitcoin coinbase data (`btc_orphan::strict_bip34_height_from_evidence`).
    pub strict_height: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lineage {
    Bitcoin(LineageEvidence),
    /// Not a Bitcoin header; the reason names the failed check.
    NotBitcoin(&'static str),
    /// Undecidable until the Core header cache reaches the next epoch.
    Pending(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineageEvidence {
    /// The prev is a known Bitcoin block (and, for a block, the bits are that
    /// height's epoch bits).
    PlacedPrev,
    /// The coinbase height is consistent with the header's time and the bits
    /// are that height's epoch bits.
    StrictHeight,
    /// The bits are the epoch bits at the header's time, or a neighbouring
    /// epoch's.
    EpochTime,
    /// The header is newer than the cache and carries the latest epoch's bits.
    CurrentEpoch,
}

impl LineageEvidence {
    /// Whether the evidence fixes the header's height and bits exactly.
    pub fn is_exact(self) -> bool {
        matches!(self, Self::PlacedPrev | Self::StrictHeight)
    }
}

/// Decide whether a parent header belongs to Bitcoin.
pub fn bitcoin_lineage(input: &ParentLineageInput, table: &NbitsTable) -> Lineage {
    let bits = input.bits.to_consensus();
    let is_share = !input.meets_own_target;

    if let Some(prev_height) = input.prev_height {
        if is_share {
            return Lineage::Bitcoin(LineageEvidence::PlacedPrev);
        }
        return match table.expected_nbits(prev_height + 1) {
            NbitsLookup::Found(expected) if expected == bits => {
                Lineage::Bitcoin(LineageEvidence::PlacedPrev)
            }
            NbitsLookup::Found(_) => Lineage::NotBitcoin("placed_prev_nbits_mismatch"),
            NbitsLookup::AboveTable => Lineage::Pending("next_epoch_not_cached"),
            NbitsLookup::BelowTable => Lineage::NotBitcoin("prev_below_table"),
        };
    }

    let above_horizon = input.time > table.horizon_time();
    if let Some(height) = input.strict_height {
        // Within the cache's time coverage a coinbase height counts only when it
        // falls in the epoch the header's time selects: another chain's heights
        // (Bitcoin Cash runs ahead of Bitcoin, Fractal far ahead) then fall to
        // the time rule instead of waiting for a height the cache never reaches.
        let consistent = above_horizon
            || table
                .epoch_height_for_time(input.time)
                .is_some_and(|epoch| (epoch..epoch + DAA_EPOCH_INTERVAL).contains(&height));
        if consistent {
            match table.expected_nbits(height) {
                NbitsLookup::Found(expected) => {
                    return if expected == bits
                        || (is_share && previous_epoch_bits(table, height) == Some(bits))
                    {
                        Lineage::Bitcoin(LineageEvidence::StrictHeight)
                    } else {
                        Lineage::NotBitcoin("strict_height_nbits_mismatch")
                    };
                }
                NbitsLookup::AboveTable => {
                    return if height > table.horizon_height() + FUTURE_HEIGHT_TOLERANCE {
                        Lineage::NotBitcoin("strict_height_far_future")
                    } else {
                        Lineage::Pending("next_epoch_not_cached")
                    };
                }
                NbitsLookup::BelowTable => {}
            }
        }
    }

    match table.classify_nbits_by_time(input.time, input.bits) {
        WeakVerdict::Match => Lineage::Bitcoin(LineageEvidence::EpochTime),
        WeakVerdict::NonBtcEpochBits => Lineage::NotBitcoin("non_btc_epoch_bits"),
        WeakVerdict::BelowFloor => Lineage::NotBitcoin("time_below_table"),
        WeakVerdict::AboveHorizon => above_horizon_lineage(bits, is_share, table),
    }
}

/// A header newer than the cache, on a prev the cache does not know.
fn above_horizon_lineage(bits: u32, is_share: bool, table: &NbitsTable) -> Lineage {
    let horizon = table.horizon_height();
    let latest = table.expected_nbits(horizon);
    if latest == NbitsLookup::Found(bits)
        || (is_share && previous_epoch_bits(table, horizon) == Some(bits))
    {
        return Lineage::Bitcoin(LineageEvidence::CurrentEpoch);
    }
    let next_retarget = daa_epoch_start(horizon) + DAA_EPOCH_INTERVAL;
    if next_retarget - horizon <= RETARGET_PENDING_WINDOW {
        Lineage::Pending("next_epoch_not_cached")
    } else {
        Lineage::NotBitcoin("non_btc_current_epoch_bits")
    }
}

/// The bits of the epoch before `height`'s: what a pool template that has not
/// yet applied a retarget carries.
fn previous_epoch_bits(table: &NbitsTable, height: i32) -> Option<u32> {
    match table.expected_nbits(daa_epoch_start(height) - 1) {
        NbitsLookup::Found(bits) => Some(bits),
        NbitsLookup::BelowTable | NbitsLookup::AboveTable => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbits_table::BitcoinEpochHeader;

    /// Bitcoin's real retarget history through height 967,961 (2026-09-22).
    fn table() -> NbitsTable {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/bitcoin/epoch-headers.json"
        )))
        .unwrap();
        let headers = fixture["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| BitcoinEpochHeader {
                height: row["height"].as_i64().unwrap() as i32,
                block_time: row["time"].as_i64().unwrap(),
                bits: u32::from_str_radix(row["bits"].as_str().unwrap(), 16).unwrap(),
            })
            .collect::<Vec<_>>();
        NbitsTable::from_bitcoin_core_headers(&headers).unwrap()
    }

    fn input(bits: u32, time: i64, meets_own_target: bool) -> ParentLineageInput {
        ParentLineageInput {
            bits: CompactTarget::from_consensus(bits),
            time,
            meets_own_target,
            prev_height: None,
            strict_height: None,
        }
    }

    /// 2026-04-24, inside the epoch starting at 945,504 (bits 0x17021369).
    const APRIL_2026: i64 = 1_777_000_000;

    #[test]
    fn shares_on_a_known_bitcoin_prev_are_bitcoin_whatever_their_bits() {
        let table = table();
        // A retarget-boundary share still carrying the previous epoch's bits.
        let share = ParentLineageInput {
            prev_height: Some(945_503),
            ..input(0x1702_0684, APRIL_2026, false)
        };
        assert_eq!(
            bitcoin_lineage(&share, &table),
            Lineage::Bitcoin(LineageEvidence::PlacedPrev)
        );
    }

    #[test]
    fn a_block_on_a_known_prev_needs_that_heights_bits() {
        let table = table();
        let block = |bits| ParentLineageInput {
            prev_height: Some(945_600),
            ..input(bits, APRIL_2026, true)
        };
        assert_eq!(
            bitcoin_lineage(&block(0x1702_1369), &table),
            Lineage::Bitcoin(LineageEvidence::PlacedPrev)
        );
        assert_eq!(
            bitcoin_lineage(&block(0x1702_0684), &table),
            Lineage::NotBitcoin("placed_prev_nbits_mismatch")
        );
        // Past the cached epochs the bits cannot be known yet.
        let beyond = ParentLineageInput {
            prev_height: Some(table.height_coverage_max()),
            ..input(0x1702_1ec5, APRIL_2026, true)
        };
        assert_eq!(
            bitcoin_lineage(&beyond, &table),
            Lineage::Pending("next_epoch_not_cached")
        );
    }

    #[test]
    fn production_foreign_parents_are_not_bitcoin() {
        let table = table();
        // Terracoin 3,202,585's parent: Bitcoin Cash-like bits, coinbase height
        // 948,125 in the epoch after the one its time selects.
        let bch = ParentLineageInput {
            strict_height: Some(948_125),
            ..input(0x1801_6046, APRIL_2026, true)
        };
        assert_eq!(
            bitcoin_lineage(&bch, &table),
            Lineage::NotBitcoin("non_btc_epoch_bits")
        );
        // Terracoin 3,202,533's parent: Fractal-like bits and coinbase height.
        let fractal = ParentLineageInput {
            strict_height: Some(1_706_476),
            ..input(0x1900_af63, APRIL_2026, true)
        };
        assert_eq!(
            bitcoin_lineage(&fractal, &table),
            Lineage::NotBitcoin("non_btc_epoch_bits")
        );
        // A Bitcoin Cash share on an unknown prev.
        let share = input(0x1801_5e8a, APRIL_2026, false);
        assert_eq!(
            bitcoin_lineage(&share, &table),
            Lineage::NotBitcoin("non_btc_epoch_bits")
        );
    }

    #[test]
    fn orphans_resolve_by_coinbase_height_or_time() {
        let table = table();
        let strict = ParentLineageInput {
            strict_height: Some(946_000),
            ..input(0x1702_1369, APRIL_2026, true)
        };
        assert_eq!(
            bitcoin_lineage(&strict, &table),
            Lineage::Bitcoin(LineageEvidence::StrictHeight)
        );
        let wrong_bits = ParentLineageInput {
            strict_height: Some(946_000),
            ..input(0x1702_0684, APRIL_2026, true)
        };
        assert_eq!(
            bitcoin_lineage(&wrong_bits, &table),
            Lineage::NotBitcoin("strict_height_nbits_mismatch")
        );
        // Without a coinbase height a neighbouring epoch's bits still match.
        let weak = input(0x1702_0684, APRIL_2026, true);
        assert_eq!(
            bitcoin_lineage(&weak, &table),
            Lineage::Bitcoin(LineageEvidence::EpochTime)
        );
        assert_eq!(
            bitcoin_lineage(&input(0x1d00_ffff, 1_000_000_000, true), &table),
            Lineage::NotBitcoin("time_below_table")
        );
    }

    #[test]
    fn headers_newer_than_the_cache_use_the_latest_epoch() {
        let table = table();
        let fresh = table.horizon_time() + 600;
        assert_eq!(
            bitcoin_lineage(&input(0x1702_1ec5, fresh, true), &table),
            Lineage::Bitcoin(LineageEvidence::CurrentEpoch)
        );
        assert_eq!(
            bitcoin_lineage(&input(0x1801_5e8a, fresh, true), &table),
            Lineage::NotBitcoin("non_btc_current_epoch_bits")
        );
        // A coinbase height far beyond the tip is another chain's.
        let far = ParentLineageInput {
            strict_height: Some(table.height_coverage_max() + 10_000),
            ..input(0x1702_1ec5, fresh, true)
        };
        assert_eq!(
            bitcoin_lineage(&far, &table),
            Lineage::NotBitcoin("strict_height_far_future")
        );
    }

    #[test]
    fn every_bitcoin_epoch_header_is_bitcoin_by_time() {
        let table = table();
        let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/bitcoin/epoch-headers.json"
        )))
        .unwrap();
        for row in fixture["headers"].as_array().unwrap() {
            let bits = u32::from_str_radix(row["bits"].as_str().unwrap(), 16).unwrap();
            let header = input(bits, row["time"].as_i64().unwrap(), true);
            assert!(
                matches!(bitcoin_lineage(&header, &table), Lineage::Bitcoin(_)),
                "Bitcoin header at {} judged foreign",
                row["height"]
            );
        }
    }
}
