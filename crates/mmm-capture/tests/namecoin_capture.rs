use std::collections::HashMap;
use std::fs;

use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use bitcoin::{Amount, PubkeyHash, ScriptBuf, TxOut};
use mmm_capture::auxpow::{ParsedNamecoinBlock, parse_namecoin_block};
use mmm_capture::capture::{
    BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE, CHILD_COINBASE_OUTPUT_SOURCE,
    CHILD_PAYOUT_REGISTRY_SOURCE, ClassificationProof, EventPoolAttribution,
    ResolvedPoolAttributions, build_event_payload, resolve_event_pools,
    resolve_event_pools_with_child_payout,
};
use mmm_capture::child_payout::{
    NAMECOIN_CHILD_PAYOUT_PARAMS, NAMECOIN_PAYOUT_ADDRESS_NAMESPACE, PoolIdentityLookup,
    PoolIdentityRef, pool_identity_lookup_key,
};
use mmm_capture::pool_resolver::PoolResolver;
use serde::Deserialize;

use mmm_capture::test_support::{fixture_path, load_raw_namecoin_fixture};

#[derive(Debug, Deserialize)]
struct ExpectedAuxpow {
    fixture: String,
    height_hint: Option<i32>,
    expected_parent_kind: Option<String>,
    expected_pow_validates_btc_target: Option<bool>,
    expected_pow_validates_child_target: Option<bool>,
    expected_difficulty_epoch_ok: Option<bool>,
    expected_parent_pool_slug: Option<String>,
    expected_child_height: Option<i32>,
    expected_child_hash: Option<String>,
    expected_parent_hash: Option<String>,
    expected_non_auxpow: Option<bool>,
    expected_parse_error: Option<bool>,
}

#[test]
fn parses_raw_namecoin_auxpow_fixtures() {
    let resolver = PoolResolver::from_default_snapshot().unwrap();
    let ids_by_slug = fake_pool_ids(&resolver);

    for name in [
        "500000-valid-parent",
        "500001-near-parent",
        "500002-wrong-chain-parent",
    ] {
        let expected = load_expected(name);
        let expected_parent_pool_slug = expected.expected_parent_pool_slug.clone();
        let raw = load_raw_namecoin_fixture(name);
        let parsed = parse_namecoin_block(&raw).unwrap();
        let ParsedNamecoinBlock::Auxpow(parsed) = parsed else {
            panic!("{name} should parse as AuxPoW");
        };

        assert_eq!(expected.fixture, name);
        assert_eq!(
            expected.expected_child_hash.unwrap(),
            parsed.child_header.hash().to_string()
        );
        assert_eq!(
            expected.expected_parent_hash.unwrap(),
            parsed.parent_header.hash().to_string()
        );
        assert_eq!(
            parsed.child_header.consensus_bytes(),
            &raw[..Header::SIZE],
            "rust-bitcoin child Header consensus serialization must round-trip fixture bytes"
        );
        let parent_header_wire = &parsed.auxpow_bytes[parsed.auxpow_bytes.len() - Header::SIZE..];
        assert_eq!(
            parsed.parent_header.consensus_bytes(),
            parent_header_wire,
            "rust-bitcoin Header consensus serialization must round-trip fixture bytes"
        );

        // Namecoin protocol quirks shared by every fixture in this set:
        // CAuxPow::hashBlock is zeroed (the parent header is already
        // present later in the payload), and both merkle branches are
        // empty (single-tx parent, single merge-mined chain).
        assert_eq!(parsed.proof.hash_block, BlockHash::all_zeros());
        assert!(parsed.proof.coinbase_branch.hashes.is_empty());
        assert!(parsed.proof.chain_branch.hashes.is_empty());
        assert_eq!(parsed.proof.coinbase_branch.index, 0);
        assert_eq!(parsed.proof.chain_branch.index, 0);

        let pool_ids = resolve_event_pools(&parsed, &resolver, &ids_by_slug);
        let event = build_event_payload(
            &parsed,
            expected.height_hint,
            pool_ids,
            ClassificationProof {
                parent_kind: None,
                parent_height: None,
                difficulty_epoch_ok: expected.expected_difficulty_epoch_ok,
            },
            1_700_000_000,
        )
        .unwrap();

        assert_eq!(event.child_height, expected.expected_child_height.unwrap());
        assert_eq!(
            event.btc_parent_kind.as_db_str(),
            expected.expected_parent_kind.unwrap()
        );
        assert_eq!(
            event.pow_validates_btc_target,
            expected.expected_pow_validates_btc_target.unwrap()
        );
        assert_eq!(
            event.pow_validates_child_target, expected.expected_pow_validates_child_target,
            "{name} child-target expectation"
        );
        assert_eq!(
            event.difficulty_epoch_ok,
            expected.expected_difficulty_epoch_ok
        );
        assert_eq!(event.discovered_at, 1_700_000_000);
        assert_eq!(event.confirmed_at, 1_700_000_000);
        assert!(event.revoked_at.is_none());

        // Payload-boundary byte-order regression guard: bytea fields hold
        // wire-order bytes derived straight from the newtype's
        // to_byte_array(), never reversed.
        assert_eq!(
            event.child_block_hash,
            parsed.child_header.hash().to_byte_array().to_vec()
        );
        assert_eq!(
            event.btc_parent_header_hash,
            parsed.parent_header.hash().to_byte_array().to_vec()
        );
        assert_eq!(event.btc_parent_header_bytes, parent_header_wire);
        assert_eq!(
            event.btc_parent_coinbase_txid,
            Some(parsed.parent_coinbase_txid.to_byte_array().to_vec())
        );
        if let Some(child_txid) = parsed.child_coinbase_txid {
            assert_eq!(
                event.child_coinbase_txid,
                Some(child_txid.to_byte_array().to_vec())
            );
        }

        assert_parent_attribution(&event, expected_parent_pool_slug.as_deref(), &ids_by_slug);
    }
}

fn assert_parent_attribution(
    event: &mmm_capture::capture::MergeMiningEventPayload,
    expected_parent_pool_slug: Option<&str>,
    ids_by_slug: &HashMap<String, i64>,
) {
    let parent_slug = expected_parent_pool_slug.unwrap();
    let expected_pool_id = ids_by_slug.get(parent_slug).copied();
    assert_eq!(
        event
            .pool_attributions
            .iter()
            .find(|attribution| attribution.side.as_db_str() == "btc_parent")
            .and_then(|attribution| attribution.pool_id),
        expected_pool_id
    );
}

#[test]
fn resolves_namecoin_child_attribution_sources_and_keeps_parent_independent() {
    let resolver = PoolResolver::from_default_snapshot().unwrap();
    let ids_by_slug = fake_pool_ids(&resolver);

    let mut legacy = load_auxpow_fixture("500000-valid-parent");
    legacy.child_coinbase_script = Some(b"\x03\x01\x02\x03/SpiderPool/513/\x00".to_vec());
    legacy.child_coinbase_outputs.clear();
    let legacy_pool_ids = resolve_event_pools_with_child_payout(
        &legacy,
        &resolver,
        &ids_by_slug,
        Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
        Some(&PoolIdentityLookup::new()),
    );
    let child = only_child_attribution(&legacy_pool_ids);
    assert_eq!(child.source, BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE);
    assert_eq!(child.namespace, "btc_coinbase_tag");
    assert_eq!(child.match_kind, "coinbase_tag");
    assert_eq!(child.pool_id, ids_by_slug.get("spiderpool").copied());

    let parsed = namecoin_child_payout_fixture();

    let unknown_pool_ids = resolve_event_pools_with_child_payout(
        &parsed,
        &resolver,
        &ids_by_slug,
        Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
        Some(&PoolIdentityLookup::new()),
    );

    let unknown_child = only_child_attribution(&unknown_pool_ids);
    assert_eq!(unknown_child.source, CHILD_COINBASE_OUTPUT_SOURCE);
    assert_eq!(unknown_child.namespace, NAMECOIN_PAYOUT_ADDRESS_NAMESPACE);
    assert_eq!(
        unknown_child.matched_value,
        "MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm"
    );
    assert_eq!(unknown_child.pool_id, None);
    assert_eq!(unknown_child.pool_identity_id, None);

    let mut identities = PoolIdentityLookup::new();
    let f2pool_id = ids_by_slug["f2pool"];
    identities.insert(
        pool_identity_lookup_key(
            NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
            "MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm",
        ),
        PoolIdentityRef {
            pool_id: f2pool_id,
            pool_identity_id: 9001,
        },
    );

    let pool_ids = resolve_event_pools_with_child_payout(
        &parsed,
        &resolver,
        &ids_by_slug,
        Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
        Some(&identities),
    );

    let child = only_child_attribution(&pool_ids);
    assert_eq!(child.source, CHILD_PAYOUT_REGISTRY_SOURCE);
    assert_eq!(child.namespace, NAMECOIN_PAYOUT_ADDRESS_NAMESPACE);
    assert_eq!(child.match_kind, "payout_address");
    assert_eq!(child.matched_value, "MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm");
    assert_eq!(child.pool_id, Some(f2pool_id));
    assert_eq!(child.pool_identity_id, Some(9001));

    let mut parsed = parsed;
    parsed.parent_coinbase_script = b"\x03\x01\x02\x03/SpiderPool/837/\x00".to_vec();
    parsed.parent_coinbase_output_addresses.clear();

    let pool_ids = resolve_event_pools_with_child_payout(
        &parsed,
        &resolver,
        &ids_by_slug,
        Some(NAMECOIN_CHILD_PAYOUT_PARAMS),
        Some(&identities),
    );
    let parent = pool_ids
        .attributions
        .iter()
        .find(|attribution| attribution.side.as_db_str() == "btc_parent")
        .expect("parent attribution");
    let child = only_child_attribution(&pool_ids);
    assert_eq!(parent.pool_id, ids_by_slug.get("spiderpool").copied());
    assert_eq!(child.pool_id, Some(f2pool_id));
    assert_ne!(parent.pool_id, child.pool_id);
}

#[test]
fn handles_non_auxpow_and_rejects_malformed_namecoin_fixtures() {
    let expected = load_expected("019199-non-auxpow");
    assert_eq!(expected.expected_non_auxpow, Some(true));

    let raw = load_raw_namecoin_fixture("019199-non-auxpow");
    let parsed = parse_namecoin_block(&raw).unwrap();
    let ParsedNamecoinBlock::NonAuxpow(header) = parsed else {
        panic!("fixture should be a non-AuxPoW block");
    };

    assert_eq!(
        expected.expected_child_hash.unwrap(),
        header.hash().to_string()
    );

    let expected = load_expected("500003-malformed");
    assert_eq!(expected.expected_parse_error, Some(true));

    let raw = load_raw_namecoin_fixture("500003-malformed");
    let err = parse_namecoin_block(&raw).unwrap_err();
    assert!(err.to_string().contains("parse AuxPoW payload"));
}

fn fake_pool_ids(resolver: &PoolResolver) -> HashMap<String, i64> {
    resolver
        .snapshot()
        .pools
        .iter()
        .enumerate()
        .map(|(index, pool)| (pool.slug.clone(), 10_000 + index as i64))
        .collect()
}

fn load_expected(name: &str) -> ExpectedAuxpow {
    let json =
        fs::read_to_string(fixture_path(&format!("namecoin/{name}"), "expected.json")).unwrap();
    serde_json::from_str(&json).unwrap()
}

fn load_auxpow_fixture(name: &str) -> Box<mmm_capture::auxpow::ParsedAuxpowBlock> {
    let raw = load_raw_namecoin_fixture(name);
    let parsed = parse_namecoin_block(&raw).unwrap();
    let ParsedNamecoinBlock::Auxpow(parsed) = parsed else {
        panic!("{name} should parse as AuxPoW");
    };
    parsed
}

fn namecoin_child_payout_fixture() -> Box<mmm_capture::auxpow::ParsedAuxpowBlock> {
    let mut parsed = load_auxpow_fixture("500000-valid-parent");
    parsed.child_coinbase_script = Some(b"\x03\x01\x02\x03/no-child-tag/".to_vec());
    parsed.child_coinbase_outputs = vec![namecoin_p2pkh_output([0; 20])];
    parsed
}

fn only_child_attribution(pool_ids: &ResolvedPoolAttributions) -> &EventPoolAttribution {
    let mut child_rows = pool_ids
        .attributions
        .iter()
        .filter(|attribution| attribution.side.as_db_str() == "child_block");
    let child = child_rows.next().expect("child payout attribution");
    assert!(
        child_rows.next().is_none(),
        "expected exactly one child payout attribution"
    );
    child
}

fn namecoin_p2pkh_output(hash: [u8; 20]) -> TxOut {
    TxOut {
        value: Amount::from_sat(1),
        script_pubkey: ScriptBuf::new_p2pkh(&PubkeyHash::from_slice(&hash).unwrap()),
    }
}
