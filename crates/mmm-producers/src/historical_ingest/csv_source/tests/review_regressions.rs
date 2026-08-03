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

#[test]
fn stale_descendant_requires_its_exact_validation_status() {
    for invalid in ["VALID", "VALID_STALE_DESCENDANT_EXTRA"] {
        let input = row(TestRow {
            chain: "namecoin",
            child_height: "42",
            classification: "stale_descendant",
            relevance_reason: "valid_stale_descendant",
            ..TestRow::default()
        })
        .replacen(",VALID,1d00ffff", &format!(",{invalid},1d00ffff"), 1);
        assert_eq!(
            candidate("namecoin", &input).unwrap_err(),
            SkipReason::TaxonomyMismatch,
            "status {invalid:?}"
        );
    }

    let input = row(TestRow {
        chain: "namecoin",
        child_height: "42",
        classification: "stale_descendant",
        relevance_reason: "valid_stale_descendant",
        ..TestRow::default()
    })
    .replacen(",VALID,1d00ffff", ",VALID_STALE_DESCENDANT,1d00ffff", 1);
    assert!(candidate("namecoin", &input).is_ok());
}

#[test]
fn unknown_stale_representation_uses_the_resolved_category_status_contract() {
    for (relevance_reason, valid_status, invalid_status) in [
        ("valid_direct_stale", "VALID", "VALID_STALE_DESCENDANT"),
        ("valid_stale_descendant", "VALID_STALE_DESCENDANT", "VALID"),
    ] {
        let base = row(TestRow {
            chain: "namecoin",
            child_height: "42",
            classification: "unknown",
            relevance_reason,
            ..TestRow::default()
        });
        let valid = base.replacen(",VALID,1d00ffff", &format!(",{valid_status},1d00ffff"), 1);
        assert!(
            candidate("namecoin", &valid).is_ok(),
            "reason {relevance_reason:?} with status {valid_status:?}"
        );

        for invalid in [invalid_status, ""] {
            let input = base.replacen(",VALID,1d00ffff", &format!(",{invalid},1d00ffff"), 1);
            assert_eq!(
                candidate("namecoin", &input).unwrap_err(),
                SkipReason::TaxonomyMismatch,
                "reason {relevance_reason:?} with status {invalid:?}"
            );
        }
    }
}

#[test]
fn header_only_child_evidence_derives_an_exact_identity() {
    let (expected_hash, header) = child_identity();
    let parsed = candidate(
        "devcoin",
        &row(TestRow {
            chain: "devcoin",
            child_header: &header,
            classification: "canonical",
            relevance_reason: "canonical_parent",
            ..TestRow::default()
        }),
    )
    .expect("header-only child evidence");
    assert_eq!(
        parsed.evidence.child_block_hash,
        Some(hex::decode(expected_hash).expect("expected child hash"))
    );
    assert_eq!(parsed.evidence.child_height, None);
}
