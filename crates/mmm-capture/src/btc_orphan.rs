//! BTC orphan classification against the persisted Core header cache.

use bitcoin::CompactTarget;

use crate::nbits_table::{DAA_EPOCH_INTERVAL, NbitsTable, NbitsVerdict, WeakVerdict};

/// BIP34 activation height. Earlier coinbase data cannot support strict height
/// evidence.
pub const BIP34_HEIGHT: i32 = 227_931;

/// Source chains whose parent coinbase script is real Bitcoin coinbase data.
pub const STRICT_BIP34_CHAINS: &[&str] = &[
    "argentum",
    "bitcoin-vault",
    "bitmark",
    "coiledcoin",
    "crown",
    "devcoin",
    "doichain",
    "elastos",
    "emercoin",
    "fractal",
    "geistgeld",
    "groupcoin",
    "huntercoin",
    "i0coin",
    "ixcoin",
    "myriadcoin",
    "namecoin",
    "syscoin",
    "terracoin",
    "unobtanium",
];

pub fn is_strict_bip34_chain(chain: &str) -> bool {
    STRICT_BIP34_CHAINS.contains(&chain)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtcOrphanVerdict {
    Strict,
    Weak,
    Excluded,
    Pending,
}

impl BtcOrphanVerdict {
    pub fn as_db_str(self) -> Option<&'static str> {
        match self {
            Self::Strict => Some("strict_btc_orphan"),
            Self::Weak => Some("weak_btc_orphan"),
            Self::Excluded => Some("excluded"),
            Self::Pending => None,
        }
    }
}

/// Classify a Core-attested-absent BTC-PoW-valid header. The supplied table is
/// derived from the persisted Bitcoin Core header cache for the operation.
pub fn classify_btc_orphan_with(
    nbits: &NbitsTable,
    header_time: i64,
    header_bits: CompactTarget,
    strict_height: Option<i32>,
) -> (BtcOrphanVerdict, &'static str) {
    let strict_height = strict_height.filter(|&height| height >= BIP34_HEIGHT);
    if let Some(height) = strict_height
        && matches!(
            nbits.expected_nbits(height),
            crate::nbits_table::NbitsLookup::AboveTable
        )
    {
        return (BtcOrphanVerdict::Pending, "above_nbits_height_horizon");
    }
    if header_time > nbits.horizon_time() {
        return (BtcOrphanVerdict::Pending, "above_nbits_time_horizon");
    }
    if let Some(height) = strict_height {
        let time_epoch_consistent = nbits
            .epoch_height_for_time(header_time)
            .is_some_and(|epoch| (epoch..epoch + DAA_EPOCH_INTERVAL).contains(&height));
        if time_epoch_consistent {
            match nbits.classify_nbits(Some(height), header_bits) {
                NbitsVerdict::Valid => {
                    return (BtcOrphanVerdict::Strict, "strict_height_nbits_match");
                }
                NbitsVerdict::Contaminant => {
                    return (BtcOrphanVerdict::Excluded, "non_btc_epoch_bits");
                }
                NbitsVerdict::AboveTableHorizon => {
                    return (BtcOrphanVerdict::Pending, "above_nbits_height_horizon");
                }
                NbitsVerdict::Indeterminate => {}
            }
        }
    }
    match nbits.classify_nbits_by_time(header_time, header_bits) {
        WeakVerdict::Match => (BtcOrphanVerdict::Weak, "timestamp_epoch_nbits_match"),
        WeakVerdict::NonBtcEpochBits => (BtcOrphanVerdict::Excluded, "non_btc_epoch_bits"),
        WeakVerdict::BelowFloor => (BtcOrphanVerdict::Excluded, "insufficient_evidence"),
        WeakVerdict::AboveHorizon => (BtcOrphanVerdict::Pending, "above_nbits_time_horizon"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbits_table::BitcoinEpochHeader;

    fn table() -> NbitsTable {
        let headers = (0..=300_384)
            .step_by(DAA_EPOCH_INTERVAL as usize)
            .map(|height| BitcoinEpochHeader {
                height,
                block_time: i64::from(height / DAA_EPOCH_INTERVAL) * 10,
                bits: match height {
                    296_352 => 0x1d00_bbbb,
                    298_368 => 0x1d00_cccc,
                    _ => 0x1d00_aaaa,
                },
            })
            .chain(std::iter::once(BitcoinEpochHeader {
                height: 300_500,
                block_time: 2_000,
                bits: 0x1d00_aaaa,
            }))
            .collect::<Vec<_>>();
        NbitsTable::from_bitcoin_core_headers(&headers).unwrap()
    }

    #[test]
    fn strict_and_weak_paths_use_the_injected_core_table() {
        let table = table();
        assert_eq!(
            classify_btc_orphan_with(
                &table,
                1_475,
                CompactTarget::from_consensus(0x1d00_bbbb),
                Some(297_000),
            )
            .0,
            BtcOrphanVerdict::Strict
        );
        assert_eq!(
            classify_btc_orphan_with(
                &table,
                1_475,
                CompactTarget::from_consensus(0x1d00_cccc),
                Some(297_000),
            )
            .0,
            BtcOrphanVerdict::Excluded
        );
        assert_eq!(
            classify_btc_orphan_with(
                &table,
                1_475,
                CompactTarget::from_consensus(0x1d00_bbbb),
                None,
            )
            .0,
            BtcOrphanVerdict::Weak
        );
        assert_eq!(
            classify_btc_orphan_with(&table, -1, CompactTarget::from_consensus(0x1d00_bbbb), None,)
                .0,
            BtcOrphanVerdict::Excluded
        );
        assert_eq!(
            classify_btc_orphan_with(
                &table,
                2_001,
                CompactTarget::from_consensus(0x1d00_bbbb),
                None,
            )
            .0,
            BtcOrphanVerdict::Pending
        );
    }

    #[test]
    fn strict_height_above_the_cache_horizon_stays_pending() {
        let (verdict, reason) = classify_btc_orphan_with(
            &table(),
            1_475,
            CompactTarget::from_consensus(0x1d00_aaaa),
            Some(500_000),
        );
        assert_eq!(verdict, BtcOrphanVerdict::Pending);
        assert_eq!(reason, "above_nbits_height_horizon");
    }
}
