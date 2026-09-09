use super::*;

#[test]
fn xaya_powdata_target_requires_zero_pure_header_nbits() {
    let (hash, header) = child_identity_with_nbits(0);
    for (child_nbits, expected_pow) in [("1d00ffff", true), ("184c238c", false)] {
        let parsed = candidate(
            "xaya",
            &row(TestRow {
                chain: "xaya",
                child_height: "42",
                child_hash: &hash,
                child_header: &header,
                child_time: "1231006505",
                child_nbits,
                classification: "stale",
                relevance_reason: "valid_direct_stale",
                ..TestRow::default()
            }),
        )
        .unwrap();
        assert_eq!(
            parsed.evidence.child_nbits,
            Some(u32::from_str_radix(child_nbits, 16).unwrap())
        );
        assert_eq!(
            parsed.evidence.pow_validates_child_target,
            Some(expected_pow)
        );
    }
}

#[test]
fn accepts_real_xaya_publication_with_zero_header_and_external_target() {
    let input = include_str!("../../../../../../fixtures/xaya/xaya_monitor_evidence.csv");
    let row = input.lines().nth(1).expect("Xaya fixture data row");
    let parsed = candidate("xaya", row).expect("authenticated Xaya canonical witness");

    assert_eq!(parsed.evidence.child_height, Some(901));
    assert_eq!(
        parsed.evidence.child_block_hash,
        Some(
            hex::decode("532e410b32f1c9e6ed8ce17afe58e9b0aa408d958481e8ffcb6eefc9aa40679d")
                .unwrap()
        )
    );
    let header = parsed.evidence.child_header_bytes.as_ref().unwrap();
    assert_eq!(&header[72..76], &[0, 0, 0, 0]);
    assert_eq!(parsed.evidence.child_block_time, Some(1_531_504_538));
    assert_eq!(parsed.evidence.child_nbits, Some(0x1830_fe39));
    assert_eq!(parsed.evidence.pow_validates_child_target, Some(true));
    assert_eq!(
        parsed.btc_parent_display_hash,
        "00000000000000000033ee726f0e7d55a5c2cc7e4aead173e925130c68977595"
    );
    assert_eq!(parsed.historical_provenance.btc_height, Some(531_784));
    assert_eq!(parsed.historical_provenance.classification, "canonical");
}

#[test]
fn rod_powdata_target_uses_external_nbits_for_parent_work() {
    let (hash, header) = child_identity_with_nbits(0);
    for (child_nbits, expected_pow) in [("1d00ffff", true), ("184c238c", false)] {
        let parsed = candidate(
            "rod",
            &row(TestRow {
                chain: "rod",
                child_height: "2697753",
                child_hash: &hash,
                child_header: &header,
                child_time: "1231006505",
                child_nbits,
                classification: "canonical",
                relevance_reason: "canonical_parent",
                ..TestRow::default()
            }),
        )
        .unwrap();
        assert_eq!(
            parsed.evidence.child_nbits,
            Some(u32::from_str_radix(child_nbits, 16).unwrap())
        );
        assert_eq!(
            parsed.evidence.pow_validates_child_target,
            Some(expected_pow)
        );
    }
}

#[test]
fn accepts_authenticated_rod_canonical_fixture_without_identity_contradictions() {
    let input = include_str!("../../../../../../fixtures/rod/rod_monitor_evidence.csv");
    let row = input.lines().nth(1).expect("ROD fixture data row");
    let mut reader = csv::Reader::from_reader(input.as_bytes());
    let headers = reader.headers().unwrap().clone();
    let source_fields = reader.records().next().unwrap().unwrap();
    let field = |name| {
        source_fields
            .get(headers.iter().position(|header| header == name).unwrap())
            .unwrap()
    };
    let parsed = candidate("rod", row).expect("authenticated ROD canonical witness");

    assert_eq!(parsed.evidence.child_height, Some(2_697_753));
    assert_eq!(
        parsed.evidence.child_block_hash,
        Some(
            hex::decode("1be2ba207535962551b3b34b8cbf8efca577c9a42690cf67a89309b7a9fd0747",)
                .unwrap()
        )
    );
    assert_eq!(
        parsed.evidence.child_header_bytes,
        Some(hex::decode(
            "00000020a05f506fdfa17d773250c20cd8ea755b1f5d7dc310940e960ea7ecd98a02a032b57e246233e37c733da434150628640b75601e60ff72af7c829f756eaf121de8258dca670000000000000000",
        )
        .unwrap())
    );
    let mut child_display_hash = parsed.evidence.child_block_hash.unwrap();
    child_display_hash.reverse();
    assert_eq!(
        hex::encode(child_display_hash),
        "4707fda9b70993a867cf9026a4c977a5fc8ebf8c4bb3b3512596357520bae21b"
    );
    assert_eq!(parsed.evidence.child_block_time, Some(1_741_327_653));
    assert_eq!(parsed.evidence.child_nbits, Some(0x1907_7766));
    assert_eq!(parsed.evidence.pow_validates_child_target, Some(true));
    assert_eq!(
        parsed.btc_parent_display_hash,
        "00000000000000000001822dc3db70b75d281687f8baa10d1818d0703f49fec0"
    );
    assert_eq!(parsed.evidence.btc_parent_header.time, 1_741_327_702);
    assert_eq!(
        parsed.evidence.btc_parent_header.bits.to_consensus(),
        0x1702_8bb1
    );
    assert!(parsed.evidence.btc_parent_coinbase_txid.is_some());
    assert!(parsed.evidence.btc_parent_coinbase_outputs.is_some());
    assert_eq!(
        parsed.evidence.btc_parent_coinbase_outputs_text.as_deref(),
        Some(field("coinbase_outputs"))
    );
    assert_eq!(
        parsed.evidence.btc_parent_coinbase_tx_bytes,
        Some(hex::decode(field("full_coinbase_hex")).unwrap())
    );
    assert_eq!(parsed.historical_provenance.source_kind, "canonical_blocks");
    assert_eq!(
        parsed.historical_provenance.source_path,
        "<chain-archive>/rod/comprehensive-run-v2/extraction/chunks/0002697728-0002697983.jsonl"
    );
    assert_eq!(parsed.historical_provenance.source_row_number, 26);
    assert_eq!(
        parsed.historical_provenance.artifact_scope,
        "canonical_blocks"
    );
    assert_eq!(parsed.historical_provenance.classification, "canonical");
    assert_eq!(parsed.historical_provenance.btc_height, Some(886_688));
    assert_eq!(
        parsed.historical_provenance.provenance,
        "rod-core-rpc:active-chain@248f1af050579a369af527288f5773a021ae8492;terminal:4127689:4a16afd2df5efd2ae7db5e07ba83820bf3174914efe2754da618a150c3df6b0a;powdata-envelope:f85552ad15221899c8a666ac0fd4047435044e2969a6b02a03e5c7361821b50d;audit:a0a6957e53b5d1dee6a0e54cc444467b13278b4f63b14a988b89101c6d605030;candidate-review:14f7c0b1ed9d57271499fb6ecfa34abab04cf884eedfd39a7e1262dafb23ad1c"
    );
}

#[test]
fn rod_powdata_target_rejects_header_nbits_and_zero_external_target() {
    for (hash, header, child_nbits) in [
        {
            let (hash, header) = child_identity();
            (hash, header, "184c238c")
        },
        {
            let (hash, header) = child_identity_with_nbits(0);
            (hash, header, "00000000")
        },
    ] {
        assert_eq!(
            candidate(
                "rod",
                &row(TestRow {
                    chain: "rod",
                    child_height: "2697753",
                    child_hash: &hash,
                    child_header: &header,
                    child_time: "1231006505",
                    child_nbits,
                    classification: "canonical",
                    relevance_reason: "canonical_parent",
                    ..TestRow::default()
                }),
            )
            .unwrap_err(),
            SkipReason::EvidenceMismatch
        );
    }
}

#[test]
fn xaya_powdata_target_does_not_invent_missing_target_or_header() {
    let (hash, header) = child_identity_with_nbits(0);
    let without_target = candidate(
        "xaya",
        &row(TestRow {
            chain: "xaya",
            child_height: "42",
            child_hash: &hash,
            child_header: &header,
            child_time: "1231006505",
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        }),
    )
    .unwrap();
    assert_eq!(without_target.evidence.child_nbits, None);
    assert_eq!(without_target.evidence.pow_validates_child_target, None);

    let without_header = candidate(
        "xaya",
        &row(TestRow {
            chain: "xaya",
            child_height: "42",
            child_hash: &hash,
            child_nbits: "1d00ffff",
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        }),
    )
    .unwrap();
    assert_eq!(without_header.evidence.child_header_bytes, None);
    assert_eq!(without_header.evidence.child_block_time, None);
    assert_eq!(without_header.evidence.child_nbits, Some(0x1d00_ffff));
}

#[test]
fn xaya_powdata_target_rejects_nonzero_pure_header_or_zero_external_target() {
    let (nonzero_hash, nonzero_header) = child_identity();
    let (zero_hash, zero_header) = child_identity_with_nbits(0);
    for (hash, header, child_nbits) in [
        (nonzero_hash, nonzero_header, "184c238c"),
        (zero_hash, zero_header, "00000000"),
    ] {
        assert_eq!(
            candidate(
                "xaya",
                &row(TestRow {
                    chain: "xaya",
                    child_height: "42",
                    child_hash: &hash,
                    child_header: &header,
                    child_time: "1231006505",
                    child_nbits,
                    classification: "stale",
                    relevance_reason: "valid_direct_stale",
                    ..TestRow::default()
                }),
            )
            .unwrap_err(),
            SkipReason::EvidenceMismatch
        );
    }

    let (hash, _) = child_identity_with_nbits(0);
    assert_eq!(
        candidate(
            "xaya",
            &row(TestRow {
                chain: "xaya",
                child_height: "42",
                child_hash: &hash,
                child_nbits: "00000000",
                classification: "stale",
                relevance_reason: "valid_direct_stale",
                ..TestRow::default()
            }),
        )
        .unwrap_err(),
        SkipReason::EvidenceMismatch
    );
}

#[test]
fn xaya_powdata_target_keeps_child_hash_and_time_checks() {
    let (hash, header) = child_identity_with_nbits(0);
    for (child_hash, child_time) in [
        ("11".repeat(32), "1231006505".to_owned()),
        (hash.clone(), "1231006506".to_owned()),
    ] {
        assert!(matches!(
            candidate(
                "xaya",
                &row(TestRow {
                    chain: "xaya",
                    child_height: "42",
                    child_hash: &child_hash,
                    child_header: &header,
                    child_time: &child_time,
                    child_nbits: "1d00ffff",
                    classification: "stale",
                    relevance_reason: "valid_direct_stale",
                    ..TestRow::default()
                }),
            )
            .unwrap_err(),
            SkipReason::HashMismatch | SkipReason::EvidenceMismatch
        ));
    }
}

#[test]
fn identity_free_rsk_rows_have_consistent_skip_reason() {
    let mut rsk_row = row(TestRow {
        chain: "rsk",
        classification: "canonical",
        relevance_reason: "canonical_parent",
        ..TestRow::default()
    });
    rsk_row.pop();
    rsk_row.push_str(",,,,,,,\n");
    assert_eq!(
        candidate("rsk", &rsk_row).unwrap_err(),
        SkipReason::MissingChildIdentity
    );

    let spec = historical_chain_spec("rsk").unwrap();
    let mut input = NORMALIZED_COLUMNS.join(",");
    input.push_str(
        ",rsk_miner,merge_mining_hash,is_uncle,uncle_index,\
         uncle_parent_height,rsk_merkle_proof,rsk_coinbase_tail\n",
    );
    input.push_str(&rsk_row);
    let mut reader = csv::Reader::from_reader(input.as_bytes());
    let layout = CsvLayout::new(reader.headers().unwrap(), spec).unwrap();
    let record = reader.records().next().unwrap().unwrap();
    assert_eq!(
        publication_state_from_record(spec, &layout, &record, false).unwrap_err(),
        SkipReason::MissingChildIdentity
    );
}

#[test]
fn published_orphan_bucket_accepts_only_stronger_cross_chain_promotion() {
    assert_eq!(
        filter_unknown(
            BtcOrphanVerdict::Strict,
            Some(RelevanceSelection::WeakBtcOrphan)
        ),
        Err(SkipReason::TaxonomyMismatch)
    );
    assert_eq!(
        filter_unknown(
            BtcOrphanVerdict::Weak,
            Some(RelevanceSelection::StrictBtcOrphan)
        ),
        Ok(())
    );
    assert_eq!(
        filter_unknown(
            BtcOrphanVerdict::Strict,
            Some(RelevanceSelection::StrictBtcOrphan)
        ),
        Ok(())
    );
    assert_eq!(
        filter_unknown(
            BtcOrphanVerdict::Weak,
            Some(RelevanceSelection::WeakBtcOrphan)
        ),
        Ok(())
    );
}

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
