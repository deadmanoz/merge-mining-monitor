use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use mmm_capture::capture::{
    ClassificationProof, NormalizedEventEvidence, ParentKind, ResolvedPoolAttributions,
    build_event_payload_from_evidence,
};

#[test]
fn catalogued_error_block_header_is_pow_valid_and_classified() {
    let header: Header = deserialize(
        &hex::decode(
            "00a0032bb223f1aad55892df75d0ff4712f0543959c5065ab89d000000000000000000005eba715327fc82c765fa651bd6226c4b4a6a846cd60197bcd76d47ada0611cfce335df696913021725806e70",
        )
        .unwrap(),
    )
    .unwrap();
    let payload = build_event_payload_from_evidence(
        NormalizedEventEvidence {
            child_height: Some(1),
            child_block_hash: None,
            child_header_bytes: None,
            child_block_time: None,
            child_nbits: None,
            btc_parent_header: header,
            pow_validates_child_target: None,
            btc_parent_coinbase_txid: None,
            btc_parent_coinbase_script: None,
            btc_parent_coinbase_outputs: None,
            btc_parent_coinbase_outputs_text: None,
            btc_parent_coinbase_tx_bytes: None,
            child_coinbase_txid: None,
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            aux_merkle_proof: None,
        },
        ResolvedPoolAttributions::default(),
        ClassificationProof {
            parent_kind: Some(ParentKind::Canonical),
            parent_height: Some(946_213),
            difficulty_epoch_ok: Some(true),
        },
        10,
    )
    .unwrap();

    assert!(payload.pow_validates_btc_target);
    assert_eq!(payload.btc_parent_kind, ParentKind::ErrorBlock);
    assert_eq!(payload.btc_parent_height, Some(946_213));
}
