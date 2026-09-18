use super::*;

#[test]
fn terracoin_activation_and_archived_candidates() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/terracoin/historical.json"
    )))
    .unwrap();
    for control in fixture["controls"].as_array().unwrap() {
        let raw = hex::decode(control["rawblock"].as_str().unwrap()).unwrap();
        let hash = control["hash"].as_str().unwrap().parse().unwrap();
        let height = control["height"].as_i64().unwrap() as i32;
        let parsed = parse_verified_classic_block(&raw, hash, height, 50).unwrap();
        if height < 833_000 {
            assert!(matches!(parsed, ParsedNamecoinBlock::NonAuxpow(_)));
            assert!(matches!(
                parse_verified_classic_block(&raw[..80], hash, height, 50).unwrap(),
                ParsedNamecoinBlock::NonAuxpow(_)
            ));
        } else {
            let ParsedNamecoinBlock::Auxpow(proof) = parsed else {
                panic!("expected AuxPoW")
            };
            assert_eq!(proof.child_height, Some(height));
            assert_eq!(
                proof.parent_header.hash().to_string(),
                control["parent_hash"].as_str().unwrap()
            );
        }
    }
}

#[test]
fn terracoin_august_canonical_and_mutations() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/terracoin/3288246.json"
    )))
    .unwrap();
    let raw = hex::decode(fixture["rawblock"].as_str().unwrap()).unwrap();
    let hash: BlockHash = fixture["child_hash"].as_str().unwrap().parse().unwrap();
    let ParsedNamecoinBlock::Auxpow(proof) =
        parse_verified_classic_block(&raw, hash, 3_288_246, 50).unwrap()
    else {
        panic!("expected AuxPoW")
    };
    assert_eq!(
        proof.parent_header.hash().to_string(),
        fixture["parent_hash"].as_str().unwrap()
    );
    assert!(validates_target(
        proof.parent_header.hash(),
        proof.parent_header.bits()
    ));
    assert!(parse_verified_classic_block(&raw, hash, 3_288_245, 50).is_err());
    assert!(parse_verified_classic_block(&raw, hash, 3_288_246, 51).is_err());
    assert!(parse_verified_classic_block(&raw, proof.parent_header.hash(), 3_288_246, 50).is_err());
    let mut damaged = raw.clone();
    damaged[120] ^= 1;
    assert!(parse_verified_classic_block(&damaged, hash, 3_288_246, 50).is_err());
    assert!(parse_verified_classic_block(&raw[..100], hash, 3_288_246, 50).is_err());
    let mut wrong_body = raw.clone();
    *wrong_body.last_mut().unwrap() ^= 1;
    assert!(
        parse_verified_classic_block(&wrong_body, hash, 3_288_246, 50)
            .unwrap_err()
            .to_string()
            .contains("merkle root")
    );
    let mut wrong_work = raw.clone();
    let nonce = 80 + proof.auxpow_bytes.len() - 4;
    wrong_work[nonce] ^= 1;
    assert!(
        parse_verified_classic_block(&wrong_work, hash, 3_288_246, 50)
            .unwrap_err()
            .to_string()
            .contains("child target")
    );
    let mut bad_branch = proof.as_ref().clone();
    bad_branch.proof.chain_branch.index ^= 1;
    assert!(verify_classic_auxpow_commitment(&bad_branch, hash, 50).is_err());
    bad_branch = proof.as_ref().clone();
    bad_branch.proof.coinbase_branch.index = 1;
    assert!(verify_classic_auxpow_commitment(&bad_branch, hash, 50).is_err());
}
