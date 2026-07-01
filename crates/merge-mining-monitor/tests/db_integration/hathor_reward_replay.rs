use anyhow::Result;
use bitcoin::base58;
use mmm_capture::capture::{
    CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE, HATHOR_PROOF_FORMAT_RFC0006,
};
use mmm_capture::source_registry::HATHOR_SOURCE_CODE;
use mmm_producers::chains::hathor::{HATHOR_REWARD_ADDRESS_NAMESPACE, parse_hathor_reward_outputs};
use mmm_producers::{ReclassifyPoolsConfig, run_reclassify_pools};
use mmm_store::get_source_id;
use serde_json::{Value, json};
use tokio_postgres::Client;

use crate::support::default_pool_snapshot;
use crate::support::seed::{child_reward_rows, pool_id_for_slug};

const ZULUPOOL_REWARD_ADDRESS: &str = "HFhvehg9Uy1YBg9bJ7eTRWwgoc6B4e1vmP";

async fn insert_hathor_reward_identity(
    client: &Client,
    pool_id: i64,
    address: &str,
) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO pool_identity (pool_id, namespace, identifier) \
             VALUES ($1, $2, $3) \
             RETURNING id",
            &[&pool_id, &HATHOR_REWARD_ADDRESS_NAMESPACE, &address],
        )
        .await?
        .get(0))
}

async fn hathor_reward_identity_id(client: &Client, address: &str) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT id \
             FROM pool_identity \
             WHERE namespace = $1 AND identifier = $2",
            &[&HATHOR_REWARD_ADDRESS_NAMESPACE, &address],
        )
        .await?
        .get(0))
}

async fn hathor_reward_audit(
    client: &Client,
    event_id: i64,
) -> Result<(serde_json::Value, serde_json::Value)> {
    let row = client
        .query_one(
            "SELECT reward_output_details, reward_addresses \
             FROM hathor_merge_mining_evidence \
             WHERE event_id = $1",
            &[&event_id],
        )
        .await?;
    Ok((row.get(0), row.get(1)))
}

fn hathor_fixture_funds_graph() -> (Vec<u8>, i32) {
    let fx: serde_json::Value =
        serde_json::from_str(include_str!("../../../../fixtures/hathor/1971823.json")).unwrap();
    let raw = hex::decode(fx["raw_hex"].as_str().unwrap()).unwrap();
    let aux_pow = hex::decode(fx["aux_pow_hex"].as_str().unwrap()).unwrap();
    let split = fx["expected_funds_graph_split"].as_i64().unwrap() as i32;
    let aux_pow_start = raw
        .windows(aux_pow.len())
        .position(|window| window == aux_pow)
        .unwrap();
    (raw[..aux_pow_start].to_vec(), split)
}

fn hathor_p2pkh_script(hash: [u8; 20], timelock: Option<u32>) -> Vec<u8> {
    let mut script = Vec::new();
    if let Some(timelock) = timelock {
        script.push(0x04);
        script.extend_from_slice(&timelock.to_be_bytes());
        script.push(0x6f);
    }
    script.extend_from_slice(&[0x76, 0xa9, 0x14]);
    script.extend_from_slice(&hash);
    script.extend_from_slice(&[0x88, 0xac]);
    script
}

fn hathor_p2pkh_script_for_address(address: &str) -> Vec<u8> {
    let decoded = base58::decode_check(address).unwrap();
    assert_eq!(decoded.len(), 21);
    assert_eq!(decoded[0], 0x28);
    let payload: [u8; 20] = decoded[1..].try_into().unwrap();
    hathor_p2pkh_script(payload, None)
}

fn append_hathor_output(buf: &mut Vec<u8>, value: u32, token_data: u8, script: &[u8]) {
    buf.extend_from_slice(&value.to_be_bytes());
    buf.push(token_data);
    buf.extend_from_slice(&(script.len() as u16).to_be_bytes());
    buf.extend_from_slice(script);
}

fn single_reward_hathor_funds_graph(address: &str) -> (Vec<u8>, i32) {
    let mut funds = vec![0x00, 0x03, 0x01];
    append_hathor_output(&mut funds, 4, 0, &hathor_p2pkh_script_for_address(address));
    let split = funds.len() as i32;
    funds.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
    (funds, split)
}

fn synthetic_hathor_funds_graph() -> (Vec<u8>, i32) {
    let mut funds = vec![0x00, 0x03, 0x03];
    append_hathor_output(&mut funds, 1, 0, &hathor_p2pkh_script([0; 20], None));
    append_hathor_output(
        &mut funds,
        2,
        1,
        &hathor_p2pkh_script([1; 20], Some(1_700_000_000)),
    );
    append_hathor_output(&mut funds, 3, 0, &[0x6a, 0x01, 0x01]);
    let split = funds.len() as i32;
    funds.extend_from_slice(&[0xaa, 0xbb, 0xcc]);
    (funds, split)
}

fn assert_synthetic_reward_audit_details(details: &Value) {
    let details = details.as_array().expect("array");
    assert_eq!(details.len(), 3);
    assert!(details[0]["skipped_reason"].is_null());
    for (index, key, expected) in [
        (1, "token_data", json!(1)),
        (1, "token_index", json!(1)),
        (1, "timelock", json!(1_700_000_000u64)),
        (1, "skipped_reason", json!("non_htr_token")),
        (2, "script_type", json!("nonstandard")),
        (2, "skipped_reason", json!("nonstandard_script")),
    ] {
        assert_eq!(details[index][key], expected);
    }
}

async fn pool_row_for_slug(client: &Client, slug: &str) -> Result<Option<(i64, String)>> {
    Ok(client
        .query_opt(
            "SELECT id, canonical_name FROM pool WHERE slug = $1",
            &[&slug],
        )
        .await?
        .map(|row| (row.get(0), row.get(1))))
}

async fn insert_hathor_reward_sidecar_event(
    client: &Client,
    child_height: i32,
    child_hash_fill: u8,
    funds_graph: Vec<u8>,
    funds_graph_split: i32,
) -> Result<i64> {
    let source_id = get_source_id(client, HATHOR_SOURCE_CODE).await?;
    let child_block_hash = vec![child_hash_fill; 32];
    let event_id: i64 = client
        .query_one(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_prev_header_hash, btc_parent_header_bytes, \
                btc_parent_header_time, btc_parent_kind, pow_validates_btc_target, \
                pow_validates_child_target, discovered_at, confirmed_at \
             ) VALUES ( \
                $1, $2, $3, 1700000000, $4, $5, $6, 1700000000, \
                'unknown', true, NULL, 1700000001, 1700000001 \
             ) RETURNING id",
            &[
                &source_id,
                &child_height,
                &child_block_hash,
                &vec![0xc0_u8; 32],
                &vec![0xbf_u8; 32],
                &vec![0x00_u8; 80],
            ],
        )
        .await?
        .get(0);
    let aux_pow = vec![0x01_u8];
    client
        .execute(
            "INSERT INTO hathor_merge_mining_evidence ( \
                event_id, hathor_block_hash, hathor_height, aux_pow, funds_graph, \
                funds_graph_split, expected_btc_nbits, proof_format \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &event_id,
                &child_block_hash,
                &child_height,
                &aux_pow,
                &funds_graph,
                &funds_graph_split,
                &0x1d00ffff_i64,
                &HATHOR_PROOF_FORMAT_RFC0006,
            ],
        )
        .await?;
    Ok(event_id)
}

#[tokio::test]
async fn reclassify_pools_creates_and_fills_hathor_reward_pools() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        assert_eq!(pool_row_for_slug(&client, "zulupool").await?, None);

        let (zulu_funds_graph, zulu_split) =
            single_reward_hathor_funds_graph(ZULUPOOL_REWARD_ADDRESS);
        let zulu_parsed = parse_hathor_reward_outputs(&zulu_funds_graph, zulu_split)?;
        assert_eq!(zulu_parsed.reward_addresses(), [ZULUPOOL_REWARD_ADDRESS]);
        let zulu_event_id = insert_hathor_reward_sidecar_event(
            &client,
            1_143_246,
            0x93,
            zulu_funds_graph,
            zulu_split,
        )
        .await?;

        let (funds_graph, split) = hathor_fixture_funds_graph();
        let parsed = parse_hathor_reward_outputs(&funds_graph, split)?;
        let reward_address = parsed.reward_addresses()[0].clone();
        assert_eq!(reward_address, "HV3iKMJpuZpktXwpoBxKEUetG6NS3zfXje");
        let event_id =
            insert_hathor_reward_sidecar_event(&client, 1_971_823, 0x91, funds_graph, split)
                .await?;

        let stats = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(stats.child_pool_updates, 2);
        assert_eq!(stats.hathor_reward_updates, 2);
        assert_eq!(stats.hathor_reward_audit_updates, 2);
        assert_eq!(stats.corrupt_hathor_funds_graph_skipped, 0);

        let (zulupool_id, canonical_name) = pool_row_for_slug(&client, "zulupool").await?.unwrap();
        assert_eq!(canonical_name, "ZULUPooL");
        let identity_id = hathor_reward_identity_id(&client, ZULUPOOL_REWARD_ADDRESS).await?;
        assert_eq!(
            child_reward_rows(&client, zulu_event_id, HATHOR_REWARD_ADDRESS_NAMESPACE).await?,
            [(
                CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                ZULUPOOL_REWARD_ADDRESS.to_owned(),
                Some(zulupool_id),
                Some(identity_id),
            )]
        );

        let poolin_id = pool_id_for_slug(&client, "poolin").await?;
        let identity_id = hathor_reward_identity_id(&client, &reward_address).await?;
        assert_eq!(
            child_reward_rows(&client, event_id, HATHOR_REWARD_ADDRESS_NAMESPACE).await?,
            [(
                CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                reward_address.clone(),
                Some(poolin_id),
                Some(identity_id),
            )]
        );

        let (details, addresses) = hathor_reward_audit(&client, event_id).await?;
        assert_eq!(addresses, json!([reward_address]));
        assert_eq!(details[0]["output_index"], 0);
        assert_eq!(details[0]["value"], 3200);
        assert_eq!(details[0]["token_data"], 0);
        assert_eq!(details[0]["authority"], false);
        assert_eq!(details[0]["script_type"], "P2PKH");
        assert_eq!(details[0]["skipped_reason"], serde_json::Value::Null);

        let again = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(again.child_pool_updates, 0);
        assert_eq!(again.hathor_reward_updates, 0);
        assert_eq!(again.hathor_reward_audit_updates, 0);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_pools_records_unknown_and_nonstandard_hathor_rewards() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let (funds_graph, split) = synthetic_hathor_funds_graph();
        let parsed = parse_hathor_reward_outputs(&funds_graph, split)?;
        let reward_address = parsed.reward_addresses()[0].clone();
        let event_id =
            insert_hathor_reward_sidecar_event(&client, 1_971_824, 0x92, funds_graph, split)
                .await?;

        let stats = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(stats.child_pool_updates, 1);
        assert_eq!(stats.hathor_reward_updates, 1);
        assert_eq!(stats.hathor_reward_audit_updates, 1);
        assert_eq!(
            child_reward_rows(&client, event_id, HATHOR_REWARD_ADDRESS_NAMESPACE).await?,
            [(
                CHILD_COINBASE_OUTPUT_SOURCE.to_owned(),
                reward_address.clone(),
                None,
                None,
            )]
        );

        let (details, addresses) = hathor_reward_audit(&client, event_id).await?;
        assert_eq!(addresses, json!([reward_address]));
        assert_synthetic_reward_audit_details(&details);

        let spiderpool_id = pool_id_for_slug(&client, "spiderpool").await?;
        let identity_id =
            insert_hathor_reward_identity(&client, spiderpool_id, &reward_address).await?;
        let upgrade = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(upgrade.child_pool_updates, 1);
        assert_eq!(upgrade.hathor_reward_updates, 1);
        assert_eq!(upgrade.hathor_reward_audit_updates, 0);
        assert_eq!(
            child_reward_rows(&client, event_id, HATHOR_REWARD_ADDRESS_NAMESPACE).await?,
            [(
                CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                reward_address,
                Some(spiderpool_id),
                Some(identity_id),
            )]
        );

        Ok::<_, anyhow::Error>(())
    })
}
