use super::*;

/// Valid reconstructed coinbase bytes are retained, while malformed or
/// non-coinbase transactions skip without pinning the live poller.
#[test]
fn reconstructed_coinbase_validation_preserves_valid_and_skips_invalid() {
    let context = HathorCaptureContext {
        resolver: PoolResolver::from_default_snapshot().unwrap(),
        reward_identities: std::collections::HashMap::new(),
        observation: ChainObservation::Live { fork_window: 20 },
        base: crate::producer_runtime::ProducerContext::from_parts(
            std::collections::HashMap::new(),
            1,
            ConfiguredParentClassifier::Disabled,
        ),
    };
    let (tx, height) = fixture_tx(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hathor/1971823.json"
    )));
    let HathorParentReconstruction::BtcValid(reconstructed) =
        reconstruct_or_skip(height, &tx).unwrap()
    else {
        panic!("the fixture must reconstruct as a BTC-valid parent");
    };
    let raw = reconstructed.raw;
    let aux_pow = reconstructed.aux_pow;
    let mut recon = reconstructed.recon;
    let funds_graph = &raw[..recon.funds_graph_len];
    let nbits_table =
        NbitsTable::from_bitcoin_core_headers(&[mmm_capture::nbits_table::BitcoinEpochHeader {
            height: 0,
            block_time: 1,
            bits: 0x1d00_ffff,
        }])
        .expect("the minimal test Core header cache is valid");

    let intact = build_hathor_capture(
        &context,
        &tx,
        height,
        &aux_pow,
        &recon,
        funds_graph,
        &nbits_table,
    )
    .unwrap()
    .expect("fixture coinbase must build");
    assert_eq!(
        intact.evidence.btc_parent_coinbase_tx_bytes.as_deref(),
        Some(recon.full_coinbase.as_slice()),
        "validated reconstructed coinbase bytes must be retained"
    );

    let pristine_coinbase = recon.full_coinbase.clone();
    recon.full_coinbase.push(0x00);
    let corrupted = build_hathor_capture(
        &context,
        &tx,
        height,
        &aux_pow,
        &recon,
        funds_graph,
        &nbits_table,
    )
    .unwrap();
    assert!(
        corrupted.is_none(),
        "trailing-byte coinbase must skip, not error"
    );

    let mut non_coinbase: Transaction = deserialize(&pristine_coinbase).unwrap();
    non_coinbase.input[0].previous_output =
        bitcoin::OutPoint::new(bitcoin::Txid::from_byte_array([1; 32]), 0);
    assert!(!non_coinbase.is_coinbase());
    recon.full_coinbase = serialize(&non_coinbase);

    let built = build_hathor_capture(
        &context,
        &tx,
        height,
        &aux_pow,
        &recon,
        funds_graph,
        &nbits_table,
    )
    .unwrap();
    assert!(
        built.is_none(),
        "well-formed non-coinbase transaction must skip"
    );
}

fn fixture_tx(json: &str) -> (HathorTransaction, i32) {
    let j: serde_json::Value = serde_json::from_str(json).unwrap();
    (
        HathorTransaction {
            raw: j["raw_hex"].as_str().unwrap().to_owned(),
            aux_pow: Some(j["aux_pow_hex"].as_str().unwrap().to_owned()),
            hash: j["tx_id"].as_str().unwrap().to_owned(),
            timestamp: j["timestamp"].as_i64().unwrap_or(0),
        },
        j["hathor_height"].as_i64().unwrap() as i32,
    )
}
