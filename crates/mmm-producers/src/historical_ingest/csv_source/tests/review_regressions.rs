use super::*;

#[test]
fn parses_and_preserves_a_full_parent_coinbase_transaction() {
    let parsed = candidate(
        "devcoin",
        &row(TestRow {
            chain: "devcoin",
            child_height: "42",
            coinbase_script: GENESIS_COINBASE_SCRIPT,
            full_coinbase: GENESIS_COINBASE,
            classification: "canonical",
            relevance_reason: "canonical_parent",
            ..TestRow::default()
        }),
    )
    .unwrap();
    assert!(parsed.evidence.btc_parent_coinbase_txid.is_some());
    assert!(parsed.evidence.btc_parent_coinbase_outputs.is_some());
    assert_eq!(
        parsed.evidence.btc_parent_coinbase_tx_bytes,
        Some(hex::decode(GENESIS_COINBASE).unwrap())
    );
}

#[test]
fn rejects_published_addresses_absent_from_the_full_coinbase() {
    let input = row(TestRow {
        chain: "devcoin",
        child_height: "42",
        coinbase_outputs: "76a914000000000000000000000000000000000000000088ac;OP_RETURN:0",
        full_coinbase: GENESIS_COINBASE,
        classification: "canonical",
        relevance_reason: "canonical_parent",
        ..TestRow::default()
    });
    assert_eq!(
        candidate("devcoin", &input).unwrap_err(),
        SkipReason::EvidenceMismatch
    );
}

#[test]
fn stale_validation_requires_a_complete_valid_token() {
    for invalid in ["VALIDATED", "VALIDATION_FAILED", "VALID_BOGUS"] {
        let input = row(TestRow {
            chain: "devcoin",
            child_height: "42",
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        })
        .replacen(",VALID,1d00ffff", &format!(",{invalid},1d00ffff"), 1);
        assert_eq!(
            candidate("devcoin", &input).unwrap_err(),
            SkipReason::TaxonomyMismatch,
            "status {invalid:?}"
        );
    }

    for valid in ["VALID", "VALID (available-evidence)", "VALID profile"] {
        let input = row(TestRow {
            chain: "devcoin",
            child_height: "42",
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        })
        .replacen(",VALID,1d00ffff", &format!(",{valid},1d00ffff"), 1);
        assert!(candidate("devcoin", &input).is_ok(), "status {valid:?}");
    }
}
