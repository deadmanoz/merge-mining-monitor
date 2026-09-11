//! BTC orphan classification against the persisted Core header cache.

use bitcoin::{CompactTarget, Transaction, consensus::deserialize};

use crate::auxpow::parse_bip34_height;
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
    "hathor",
    "huntercoin",
    "i0coin",
    "ixcoin",
    "myriadcoin",
    "namecoin",
    "qbit",
    "syscoin",
    "terracoin",
    "unobtanium",
];

pub fn is_strict_bip34_chain(chain: &str) -> bool {
    STRICT_BIP34_CHAINS.contains(&chain)
}

/// Return a BIP34 height when the stored parent coinbase evidence is strong
/// enough for strict orphan classification.
///
/// Hathor's legacy rows may contain a height-shaped script that was not
/// validated as a Bitcoin coinbase script. Require the complete serialized
/// transaction for Hathor and bind its sole input script to the separately
/// stored script before accepting the height. Other strict chains retain the
/// established script-only evidence contract.
pub fn strict_bip34_height_from_evidence(
    chain: &str,
    script_sig: &[u8],
    tx_bytes: Option<&[u8]>,
) -> Option<i32> {
    if !is_strict_bip34_chain(chain) {
        return None;
    }

    if chain == "hathor" {
        let tx: Transaction = deserialize(tx_bytes?).ok()?;
        if !tx.is_coinbase() || tx.input[0].script_sig.as_bytes() != script_sig {
            return None;
        }
    }

    parse_bip34_height(script_sig).filter(|&height| height >= BIP34_HEIGHT)
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
    if strict_height.is_some_and(|height| height > nbits.horizon_height()) {
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
    use bitcoin::{
        Amount, OutPoint, ScriptBuf, Sequence, TxIn, TxOut, Witness, absolute,
        consensus::serialize, transaction,
    };

    fn serialized_transaction(script_sig: &[u8], coinbase: bool) -> Vec<u8> {
        let mut previous_output = OutPoint::null();
        if !coinbase {
            previous_output.vout = 0;
        }
        serialize(&Transaction {
            version: transaction::Version::ONE,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn {
                previous_output,
                script_sig: ScriptBuf::from_bytes(script_sig.to_vec()),
                sequence: Sequence::MAX,
                witness: Witness::default(),
            }],
            output: vec![TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::new(),
            }],
        })
    }

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

    #[test]
    fn strict_height_in_the_current_epoch_but_above_the_core_horizon_stays_pending() {
        let (verdict, reason) = classify_btc_orphan_with(
            &table(),
            1_495,
            CompactTarget::from_consensus(0x1d00_aaaa),
            Some(301_000),
        );
        assert_eq!(verdict, BtcOrphanVerdict::Pending);
        assert_eq!(reason, "above_nbits_height_horizon");
    }

    #[test]
    fn hathor_strict_height_requires_matching_coinbase_transaction() {
        let script = [0x03, 0xe0, 0x93, 0x04];
        let tx = serialized_transaction(&script, true);

        assert_eq!(
            strict_bip34_height_from_evidence("hathor", &script, Some(&tx)),
            Some(300_000)
        );
        assert_eq!(
            strict_bip34_height_from_evidence("hathor", &script, None),
            None
        );
        assert_eq!(
            strict_bip34_height_from_evidence("hathor", &script, Some(&[0xff])),
            None
        );

        let ordinary_tx = serialized_transaction(&script, false);
        assert_eq!(
            strict_bip34_height_from_evidence("hathor", &script, Some(&ordinary_tx)),
            None
        );

        let mismatched_tx = serialized_transaction(&[0x03, 0xe1, 0x93, 0x04], true);
        assert_eq!(
            strict_bip34_height_from_evidence("hathor", &script, Some(&mismatched_tx)),
            None
        );
    }

    #[test]
    fn non_hathor_strict_chains_keep_script_only_height_evidence() {
        let script = [0x03, 0xe0, 0x93, 0x04];

        assert_eq!(
            strict_bip34_height_from_evidence("namecoin", &script, None),
            Some(300_000)
        );
        assert_eq!(
            strict_bip34_height_from_evidence("rsk", &script, None),
            None
        );
    }
}
