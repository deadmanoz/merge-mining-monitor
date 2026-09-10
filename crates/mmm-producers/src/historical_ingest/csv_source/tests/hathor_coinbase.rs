use bitcoin::{ScriptBuf, Transaction, consensus::serialize};

use super::*;

const HATHOR_BIP34_SCRIPT: &str = "03e09304";

fn orphan_nbits_table() -> NbitsTable {
    let mut headers = (0..=300_384)
        .step_by(mmm_capture::nbits_table::DAA_EPOCH_INTERVAL as usize)
        .map(|height| BitcoinEpochHeader {
            height,
            block_time: match height {
                298_368 => 1_231_006_504,
                300_384 => 1_231_006_506,
                _ => i64::from(height),
            },
            bits: 0x1d00_ffff,
        })
        .collect::<Vec<_>>();
    headers.push(BitcoinEpochHeader {
        height: 300_500,
        block_time: 1_231_006_507,
        bits: 0x1d00_ffff,
    });
    NbitsTable::from_bitcoin_core_headers(&headers).unwrap()
}

fn full_coinbase_with_script(script_hex: &str, coinbase: bool) -> String {
    let mut tx: Transaction = deserialize(&hex::decode(GENESIS_COINBASE).unwrap()).unwrap();
    tx.input[0].script_sig = ScriptBuf::from_bytes(hex::decode(script_hex).unwrap());
    if !coinbase {
        tx.input[0].previous_output.vout = 0;
    }
    hex::encode(serialize(&tx))
}

#[test]
fn hathor_unknown_needs_validated_full_coinbase_for_strict_height() {
    let table = orphan_nbits_table();
    let script_only = candidate_with_nbits_table(
        "hathor",
        &row(TestRow {
            chain: "hathor",
            child_height: "42",
            coinbase_script: HATHOR_BIP34_SCRIPT,
            classification: "unknown",
            relevance: "weak_btc_orphan",
            relevance_reason: "timestamp_epoch_nbits_match",
            ..TestRow::default()
        }),
        Some(&table),
    )
    .unwrap();
    assert_eq!(script_only.orphan_verdict, Some(BtcOrphanVerdict::Weak));
    assert_eq!(script_only.evidence.btc_parent_coinbase_tx_bytes, None);

    let full_coinbase = full_coinbase_with_script(HATHOR_BIP34_SCRIPT, true);
    let validated = candidate_with_nbits_table(
        "hathor",
        &row(TestRow {
            chain: "hathor",
            child_height: "42",
            coinbase_script: HATHOR_BIP34_SCRIPT,
            full_coinbase: &full_coinbase,
            classification: "unknown",
            relevance: "strict_btc_orphan",
            relevance_reason: "strict_height_nbits_match",
            ..TestRow::default()
        }),
        Some(&table),
    )
    .unwrap();
    assert_eq!(validated.orphan_verdict, Some(BtcOrphanVerdict::Strict));
    assert_eq!(
        validated.evidence.btc_parent_coinbase_tx_bytes,
        Some(hex::decode(full_coinbase).unwrap())
    );
}

#[test]
fn hathor_unknown_rejects_invalid_full_transaction_evidence() {
    let table = orphan_nbits_table();
    let mismatched = full_coinbase_with_script("03e19304", true);
    let ordinary = full_coinbase_with_script(HATHOR_BIP34_SCRIPT, false);

    for (full_coinbase, expected) in [
        (mismatched, SkipReason::EvidenceMismatch),
        (ordinary, SkipReason::Malformed),
        ("ff".to_owned(), SkipReason::Malformed),
    ] {
        assert_eq!(
            candidate_with_nbits_table(
                "hathor",
                &row(TestRow {
                    chain: "hathor",
                    child_height: "42",
                    coinbase_script: HATHOR_BIP34_SCRIPT,
                    full_coinbase: &full_coinbase,
                    classification: "unknown",
                    relevance: "strict_btc_orphan",
                    relevance_reason: "strict_height_nbits_match",
                    ..TestRow::default()
                }),
                Some(&table),
            )
            .unwrap_err(),
            expected
        );
    }
}
