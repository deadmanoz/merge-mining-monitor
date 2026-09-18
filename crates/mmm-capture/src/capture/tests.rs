use super::*;

#[test]
fn unknown_when_pow_valid_without_proof() {
    assert_eq!(
        classify_parent(true, None, None).unwrap(),
        ParentKind::Unknown
    );
}

#[test]
fn near_when_pow_invalid_even_without_height() {
    assert_eq!(
        classify_parent(false, None, None).unwrap(),
        ParentKind::Near
    );
}

#[test]
fn canonical_proof_requires_height() {
    let err = classify_parent(true, Some(ParentKind::Canonical), None).unwrap_err();
    assert!(err.to_string().contains("requires btc_parent_height"));
}

#[test]
fn classification_proof_updates_payload_height_from_proof_only() {
    let mut payload = MergeMiningEventPayload {
        child_height: Some(1),
        child_block_hash: Some(vec![1; 32]),
        child_header_bytes: None,
        child_block_time: Some(1),
        child_nbits: None,
        btc_parent_header_hash: vec![2; 32],
        btc_parent_prev_header_hash: vec![3; 32],
        btc_parent_header_bytes: vec![4; 80],
        btc_parent_header_time: 1,
        btc_parent_height: None,
        btc_parent_kind: ParentKind::Unknown,
        pow_validates_btc_target: true,
        pow_validates_child_target: Some(true),
        difficulty_epoch_ok: None,
        classification_incomplete: false,
        btc_parent_coinbase_txid: None,
        btc_parent_coinbase_script: None,
        btc_parent_coinbase_outputs: None,
        btc_parent_coinbase_outputs_text: None,
        btc_parent_coinbase_tx_bytes: None,
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: None,
        pool_attributions: Vec::new(),
        discovered_at: 10,
        confirmed_at: 10,
        revoked_at: None,
        revocation_reason: None,
        historical_provenance: None,
    };

    apply_classification_proof(
        &mut payload,
        ClassificationProof {
            parent_kind: Some(ParentKind::Canonical),
            parent_height: Some(840_000),
            difficulty_epoch_ok: Some(true),
            incomplete: false,
        },
    )
    .unwrap();

    assert_eq!(payload.btc_parent_kind, ParentKind::Canonical);
    assert_eq!(payload.btc_parent_height, Some(840_000));
    assert_eq!(payload.difficulty_epoch_ok, Some(true));
}

#[test]
fn catalogued_error_block_overrides_external_proof() {
    let mut payload = MergeMiningEventPayload {
        child_height: Some(1),
        child_block_hash: Some(vec![1; 32]),
        child_header_bytes: None,
        child_block_time: Some(1),
        child_nbits: None,
        btc_parent_header_hash: bitcoin::BlockHash::from_str(
            "00000000000000000000c3d95a4bdc068dfe0c6d1e7ad13045c6f570e58d9ed7",
        )
        .unwrap()
        .to_byte_array()
        .to_vec(),
        btc_parent_prev_header_hash: vec![3; 32],
        btc_parent_header_bytes: vec![4; 80],
        btc_parent_header_time: 1,
        btc_parent_height: None,
        btc_parent_kind: ParentKind::Unknown,
        pow_validates_btc_target: true,
        pow_validates_child_target: Some(true),
        difficulty_epoch_ok: None,
        classification_incomplete: false,
        btc_parent_coinbase_txid: None,
        btc_parent_coinbase_script: None,
        btc_parent_coinbase_outputs: None,
        btc_parent_coinbase_outputs_text: None,
        btc_parent_coinbase_tx_bytes: None,
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: None,
        pool_attributions: Vec::new(),
        discovered_at: 10,
        confirmed_at: 10,
        revoked_at: None,
        revocation_reason: None,
        historical_provenance: None,
    };

    apply_classification_proof(
        &mut payload,
        ClassificationProof {
            parent_kind: Some(ParentKind::Canonical),
            parent_height: Some(946_213),
            difficulty_epoch_ok: Some(true),
            incomplete: false,
        },
    )
    .unwrap();

    assert_eq!(payload.btc_parent_kind, ParentKind::ErrorBlock);
    assert_eq!(payload.btc_parent_height, Some(946_213));
    assert_eq!(payload.difficulty_epoch_ok, Some(true));
}

#[test]
fn publication_output_parser_accepts_an_all_digit_witness_script() {
    let script = format!("0014{}", "00".repeat(20));
    assert_eq!(
        published_parent_coinbase_output_addresses(&format!(
            "{script};{script}:5000000000;5000000000:{script};not-an-address"
        )),
        vec!["bc1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq9e75rs"]
    );
}
