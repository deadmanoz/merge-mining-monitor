//! Bitcoin nBits classification over Core-derived cached headers.
//!
//! Callers supply the sparse header cache persisted by `mmm-store`: every
//! 2,016-block epoch boundary and the highest confirmed horizon header. This
//! crate deliberately has no embedded chain history or offline seed.

use std::collections::HashMap;

use anyhow::{Result, ensure};
use bitcoin::CompactTarget;

pub const DAA_EPOCH_INTERVAL: i32 = 2016;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitcoinEpochHeader {
    pub height: i32,
    pub block_time: i64,
    pub bits: u32,
}

#[derive(Debug, Clone)]
pub struct NbitsTable {
    covered_max_height: i32,
    horizon_height: i32,
    horizon_time: i64,
    by_epoch: HashMap<i32, u32>,
    epochs_by_time: Vec<(i64, i32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NbitsLookup {
    Found(u32),
    BelowTable,
    AboveTable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NbitsVerdict {
    Valid,
    Contaminant,
    Indeterminate,
    AboveTableHorizon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeakVerdict {
    Match,
    NonBtcEpochBits,
    BelowFloor,
    AboveHorizon,
}

impl NbitsTable {
    pub fn from_bitcoin_core_headers(headers: &[BitcoinEpochHeader]) -> Result<Self> {
        let horizon = headers
            .last()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Bitcoin Core header cache is empty"))?;
        ensure!(
            headers
                .windows(2)
                .all(|pair| pair[0].height < pair[1].height),
            "Bitcoin Core header cache is not strictly ordered by height"
        );
        let epoch_rows = headers
            .iter()
            .copied()
            .filter(|header| header.height % DAA_EPOCH_INTERVAL == 0)
            .collect::<Vec<_>>();
        let first = epoch_rows
            .first()
            .copied()
            .ok_or_else(|| anyhow::anyhow!("Bitcoin Core header cache has no epoch boundary"))?;
        ensure!(
            first.height == 0,
            "Bitcoin Core epoch cache is not anchored at genesis"
        );
        for pair in epoch_rows.windows(2) {
            ensure!(
                pair[1].height == pair[0].height + DAA_EPOCH_INTERVAL,
                "Bitcoin Core epoch cache has a gap between {} and {}",
                pair[0].height,
                pair[1].height
            );
        }
        let last_epoch = epoch_rows
            .last()
            .copied()
            .expect("non-empty epoch rows have a last member");
        ensure!(
            horizon.height >= last_epoch.height,
            "Bitcoin Core horizon precedes its latest cached epoch"
        );
        ensure!(
            horizon.height < last_epoch.height + DAA_EPOCH_INTERVAL,
            "Bitcoin Core horizon exceeds its latest cached epoch interval"
        );
        let by_epoch = epoch_rows
            .iter()
            .map(|header| (header.height, header.bits))
            .collect::<HashMap<_, _>>();
        let mut epochs_by_time = epoch_rows
            .iter()
            .map(|header| (header.block_time, header.height))
            .collect::<Vec<_>>();
        epochs_by_time.sort_unstable();
        ensure!(
            epochs_by_time.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "Bitcoin Core epoch headers are not strictly ordered by time"
        );
        Ok(Self {
            covered_max_height: last_epoch.height,
            horizon_height: horizon.height,
            horizon_time: horizon.block_time,
            by_epoch,
            epochs_by_time,
        })
    }

    pub fn covered_max_height(&self) -> i32 {
        self.covered_max_height
    }

    pub fn horizon_height(&self) -> i32 {
        self.horizon_height
    }

    pub fn horizon_time(&self) -> i64 {
        self.horizon_time
    }

    pub fn height_coverage_max(&self) -> i32 {
        self.covered_max_height + DAA_EPOCH_INTERVAL - 1
    }

    pub fn covered_max_time(&self) -> Option<i64> {
        Some(self.horizon_time)
    }

    pub fn expected_nbits(&self, height: i32) -> NbitsLookup {
        if height < 0 {
            return NbitsLookup::BelowTable;
        }
        if height > self.height_coverage_max() {
            return NbitsLookup::AboveTable;
        }
        self.by_epoch
            .get(&daa_epoch_start(height))
            .copied()
            .map(NbitsLookup::Found)
            .unwrap_or(NbitsLookup::AboveTable)
    }

    pub fn epoch_height_for_time(&self, time: i64) -> Option<i32> {
        if time > self.horizon_time || time < self.epochs_by_time.first()?.0 {
            return None;
        }
        let index = self
            .epochs_by_time
            .partition_point(|&(start, _)| start <= time);
        self.epochs_by_time
            .get(index - 1)
            .map(|&(_, height)| height)
    }

    pub fn classify_nbits_by_time(
        &self,
        header_time: i64,
        actual_bits: CompactTarget,
    ) -> WeakVerdict {
        if header_time > self.horizon_time {
            return WeakVerdict::AboveHorizon;
        }
        let Some(epoch) = self.epoch_height_for_time(header_time) else {
            return WeakVerdict::BelowFloor;
        };
        let actual = actual_bits.to_consensus();
        let matches = [
            epoch.checked_sub(DAA_EPOCH_INTERVAL),
            Some(epoch),
            epoch.checked_add(DAA_EPOCH_INTERVAL),
        ]
        .into_iter()
        .flatten()
        .filter_map(|height| self.by_epoch.get(&height))
        .any(|&expected| actual == expected);
        if matches {
            WeakVerdict::Match
        } else {
            WeakVerdict::NonBtcEpochBits
        }
    }

    pub fn classify_nbits(
        &self,
        bip34_height: Option<i32>,
        actual_bits: CompactTarget,
    ) -> NbitsVerdict {
        let Some(height) = bip34_height else {
            return NbitsVerdict::Indeterminate;
        };
        match self.expected_nbits(height) {
            NbitsLookup::Found(expected) => classify_against_expected_nbits(expected, actual_bits),
            NbitsLookup::BelowTable => NbitsVerdict::Indeterminate,
            NbitsLookup::AboveTable => NbitsVerdict::AboveTableHorizon,
        }
    }
}

pub fn daa_epoch_start(height: i32) -> i32 {
    height.div_euclid(DAA_EPOCH_INTERVAL) * DAA_EPOCH_INTERVAL
}

pub fn classify_against_expected_nbits(expected: u32, actual: CompactTarget) -> NbitsVerdict {
    if actual.to_consensus() == expected {
        NbitsVerdict::Valid
    } else {
        NbitsVerdict::Contaminant
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers() -> Vec<BitcoinEpochHeader> {
        [
            (0, 1_000, 0x1d00_ffff),
            (2016, 2_000, 0x1c00_ffff),
            (4032, 3_000, 0x1b00_ffff),
            (4500, 3_400, 0x1b00_ffff),
        ]
        .into_iter()
        .map(|(height, block_time, bits)| BitcoinEpochHeader {
            height,
            block_time,
            bits,
        })
        .collect()
    }

    #[test]
    fn core_headers_define_height_and_time_classification() {
        let table = NbitsTable::from_bitcoin_core_headers(&headers()).unwrap();
        assert_eq!(table.expected_nbits(2017), NbitsLookup::Found(0x1c00_ffff));
        assert_eq!(table.expected_nbits(6048), NbitsLookup::AboveTable);
        assert_eq!(table.epoch_height_for_time(2500), Some(2016));
        assert_eq!(
            table.classify_nbits_by_time(2500, CompactTarget::from_consensus(0x1b00_ffff)),
            WeakVerdict::Match
        );
    }

    #[test]
    fn cache_requires_a_genesis_anchor_and_contiguous_epochs() {
        let missing_genesis = [BitcoinEpochHeader {
            height: 2016,
            block_time: 2_000,
            bits: 0x1c00_ffff,
        }];
        assert!(NbitsTable::from_bitcoin_core_headers(&missing_genesis).is_err());

        let gap = [
            BitcoinEpochHeader {
                height: 0,
                block_time: 1_000,
                bits: 0x1d00_ffff,
            },
            BitcoinEpochHeader {
                height: 4032,
                block_time: 3_000,
                bits: 0x1b00_ffff,
            },
        ];
        assert!(NbitsTable::from_bitcoin_core_headers(&gap).is_err());
    }
}
