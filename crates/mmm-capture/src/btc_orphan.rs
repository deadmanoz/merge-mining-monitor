//! BTC orphan classification against the persisted Core header cache.

use bitcoin::{CompactTarget, Transaction, consensus::deserialize};

use crate::auxpow::parse_bip34_height;
use crate::lineage::{Lineage, LineageEvidence, ParentLineageInput, bitcoin_lineage};
use crate::nbits_table::NbitsTable;

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

/// Classify a Core-attested-absent header that meets its own target: the
/// strength of the evidence that it is a Bitcoin orphan, read from the
/// lineage rule ([`bitcoin_lineage`]) with no placed prev. A coinbase height
/// consistent with the header's time is strict evidence, bits matching the
/// time's epoch are weak evidence, another chain's header is excluded, and a
/// header the cache cannot decide yet is pending.
pub fn classify_btc_orphan_with(
    nbits: &NbitsTable,
    header_time: i64,
    header_bits: CompactTarget,
    strict_height: Option<i32>,
) -> (BtcOrphanVerdict, &'static str) {
    let input = ParentLineageInput {
        bits: header_bits,
        time: header_time,
        meets_own_target: true,
        prev_height: None,
        strict_height: strict_height.filter(|&height| height >= BIP34_HEIGHT),
    };
    match bitcoin_lineage(&input, nbits) {
        Lineage::Bitcoin(LineageEvidence::StrictHeight | LineageEvidence::PlacedPrev) => {
            (BtcOrphanVerdict::Strict, "strict_height_nbits_match")
        }
        Lineage::Bitcoin(LineageEvidence::EpochTime) => {
            (BtcOrphanVerdict::Weak, "timestamp_epoch_nbits_match")
        }
        // Newer than the cache: its class is decided once the cache reaches
        // its time.
        Lineage::Bitcoin(LineageEvidence::CurrentEpoch) => {
            (BtcOrphanVerdict::Pending, "above_nbits_time_horizon")
        }
        Lineage::NotBitcoin(reason) => (BtcOrphanVerdict::Excluded, reason),
        Lineage::Pending(reason) => (BtcOrphanVerdict::Pending, reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nbits_table::{BitcoinEpochHeader, DAA_EPOCH_INTERVAL};
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
        // Newer than the cache: the latest epoch's bits wait for the cache to
        // reach the header's time, and an old epoch's bits are not Bitcoin's.
        assert_eq!(
            classify_btc_orphan_with(
                &table,
                2_001,
                CompactTarget::from_consensus(0x1d00_aaaa),
                None,
            )
            .0,
            BtcOrphanVerdict::Pending
        );
        assert_eq!(
            classify_btc_orphan_with(
                &table,
                2_001,
                CompactTarget::from_consensus(0x1d00_bbbb),
                None,
            )
            .0,
            BtcOrphanVerdict::Excluded
        );
    }

    #[test]
    fn a_coinbase_height_that_runs_ahead_of_the_time_falls_to_the_time_rule() {
        // Another chain's coinbase heights run ahead of Bitcoin's: a height
        // outside the epoch the time selects is not strict evidence, and the
        // header is judged by its time instead of waiting for that height.
        let (verdict, reason) = classify_btc_orphan_with(
            &table(),
            1_475,
            CompactTarget::from_consensus(0x1d00_aaaa),
            Some(500_000),
        );
        assert_eq!(verdict, BtcOrphanVerdict::Weak);
        assert_eq!(reason, "timestamp_epoch_nbits_match");
    }

    #[test]
    fn strict_height_in_the_current_epoch_is_decided_by_that_epochs_bits() {
        // The current epoch's bits are known from its boundary, so a height
        // above the cached tip but inside that epoch is decided now.
        let (verdict, reason) = classify_btc_orphan_with(
            &table(),
            1_495,
            CompactTarget::from_consensus(0x1d00_aaaa),
            Some(301_000),
        );
        assert_eq!(verdict, BtcOrphanVerdict::Strict);
        assert_eq!(reason, "strict_height_nbits_match");
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
