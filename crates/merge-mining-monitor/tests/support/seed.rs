//! Shared table-level row builders for the DB integration binaries.
//!
//! Direct-SQL seeds for the read-model tables, deliberately bypassing the
//! production write paths: tests that assert table-level behavior (projection
//! layout, pagination, filters) seed exactly the rows they mean to observe.
//! Tests that exercise the production mutation/capture flow use the scenario
//! layer instead.
//!
//! Everything here is `pub` (including seed-struct fields, which are
//! constructed with struct literals at call sites); the per-binary
//! `#![allow(dead_code)]` blanket in `tests/support/mod.rs` covers helpers a
//! given binary does not use.

use std::collections::BTreeMap;

use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};
use mmm_api::query;
use mmm_capture::child_payout::NAMECOIN_PAYOUT_ADDRESS_NAMESPACE;
use serde_json::json;
use time::{Date, Month};
use tokio_postgres::Client;
use tokio_postgres::types::Json;

pub async fn insert_pool(client: &Client, slug: &str, name: &str) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO pool (slug, canonical_name, coinbase_tags, payout_addresses) \
             VALUES ($1, $2, '[]'::jsonb, '[]'::jsonb) \
             RETURNING id",
            &[&slug, &name],
        )
        .await?
        .get(0))
}

pub async fn pool_id_for_slug(client: &Client, slug: &str) -> Result<i64> {
    Ok(client
        .query_one("SELECT id FROM pool WHERE slug = $1", &[&slug])
        .await?
        .get(0))
}

/// Insert a Namecoin payout-address pool identity. Shared by the
/// reclassify-pools and event_pool_attribution DB integration binaries.
pub async fn insert_namecoin_payout_identity(
    client: &Client,
    pool_id: i64,
    address: &str,
) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO pool_identity (pool_id, namespace, identifier) \
             VALUES ($1, $2, $3) \
             RETURNING id",
            &[&pool_id, &NAMECOIN_PAYOUT_ADDRESS_NAMESPACE, &address],
        )
        .await?
        .get(0))
}

/// Read the child-block attribution rows for `event_id` in `namespace`, ordered
/// by matched value. Shared by the Fractal and Hathor reward-replay binaries.
pub async fn child_reward_rows(
    client: &Client,
    event_id: i64,
    namespace: &str,
) -> Result<Vec<(String, String, Option<i64>, Option<i64>)>> {
    let rows = client
        .query(
            "SELECT source, matched_value, pool_id, pool_identity_id \
             FROM event_pool_attribution \
             WHERE event_id = $1 \
               AND side = 'child_block' \
               AND namespace = $2 \
             ORDER BY matched_value",
            &[&event_id, &namespace],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect())
}

pub struct EventSeed {
    pub source_id: i64,
    pub child_height: i32,
    pub child_hash: Vec<u8>,
    pub parent_hash: Vec<u8>,
    pub prev_hash: Vec<u8>,
    pub parent_time: i64,
    pub kind: &'static str,
    pub pow_validates_btc_target: bool,
    pub btc_height: Option<i32>,
    pub pool_id: Option<i64>,
}

pub async fn insert_event(client: &Client, seed: EventSeed) -> Result<i64> {
    let event_id = client
        .query_one(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, child_miner_pool_id, \
                btc_parent_header_hash, btc_parent_prev_header_hash, btc_parent_header_bytes, \
                btc_parent_header_time, btc_parent_height, btc_parent_kind, \
                pow_validates_btc_target, pow_validates_child_target, \
                discovered_at, confirmed_at \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, TRUE, $13, $13 \
             ) \
             RETURNING id",
            &[
                &seed.source_id,
                &seed.child_height,
                &seed.child_hash,
                &(seed.parent_time + seed.child_height as i64),
                &seed.pool_id,
                &seed.parent_hash,
                &seed.prev_hash,
                &header_bytes(&seed.parent_hash, &seed.prev_hash),
                &seed.parent_time,
                &seed.btc_height,
                &seed.kind,
                &seed.pow_validates_btc_target,
                &seed.parent_time,
            ],
        )
        .await?
        .get(0);
    if let Some(pool_id) = seed.pool_id {
        client
            .execute(
                "INSERT INTO event_pool_attribution ( \
                    event_id, side, namespace, match_kind, matched_value, pool_id, \
                    source, confidence, details, first_seen_at, last_seen_at \
                 ) VALUES ( \
                    $1, 'btc_parent', 'btc_coinbase_tag', 'test_seed', $2, $3, \
                    'test_seed', 'high', '{}'::jsonb, $4, $4 \
                 )",
                &[
                    &event_id,
                    &format!("test-pool-{pool_id}"),
                    &pool_id,
                    &seed.parent_time,
                ],
            )
            .await?;
    }
    Ok(event_id)
}

pub async fn insert_block(
    client: &Client,
    hash: &[u8],
    prev_hash: &[u8],
    height: Option<i32>,
    kind: &str,
    header_time: i64,
    canonical_competitor_hash: Option<&[u8]>,
) -> Result<()> {
    let height_source: Option<&str> = match (kind, height) {
        ("canonical", Some(_)) | ("stale", Some(_)) => Some("bitcoin-core"),
        _ => None,
    };
    client
        .execute(
            "INSERT INTO block ( \
                btc_header_hash, btc_prev_header_hash, btc_height, btc_height_source, \
                kind, btc_header_bytes, btc_header_time, bitcoin_miner_pool_id, btc_coinbase_script, \
                btc_coinbase_status, canonical_competitor_hash, \
                total_attestations, distinct_sources, auxpow_chain_count, live_observed, \
                core_attested, pow_validated, created_at, updated_at \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, NULL, \
                CASE WHEN $5 = 'canonical' THEN decode('51', 'hex') ELSE NULL END, \
                CASE WHEN $5 = 'canonical' THEN 'complete' ELSE 'not_attempted' END, \
                $8, \
                0, 0, 0, FALSE, FALSE, TRUE, $7, $7 \
             )",
            &[
                &hash,
                &prev_hash,
                &height,
                &height_source,
                &kind,
                &header_bytes(hash, prev_hash),
                &header_time,
                &canonical_competitor_hash,
            ],
        )
        .await?;
    Ok(())
}

pub async fn block_kind(client: &Client, hash: &[u8]) -> Result<String> {
    Ok(client
        .query_one(
            "SELECT kind FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await?
        .get(0))
}

// Insert a PoW-valid `kind='unknown'` BTC orphan (height NULL) with the given
// btc_orphan_class. insert_block sets pow_validated=TRUE; the migration CHECK does
// not enforce a class on pow_validated, so the class is set in a follow-up UPDATE.
pub async fn insert_orphan(
    client: &Client,
    hash: &[u8],
    prev_hash: &[u8],
    header_time: i64,
    orphan_class: &str,
) -> Result<()> {
    insert_block(client, hash, prev_hash, None, "unknown", header_time, None).await?;
    client
        .execute(
            "UPDATE block SET btc_orphan_class = $2 WHERE btc_header_hash = $1",
            &[&hash, &orphan_class],
        )
        .await?;
    Ok(())
}

pub struct StaleEventSeed {
    pub source_id: i64,
    pub hash: Vec<u8>,
    pub prev_hash: Vec<u8>,
    pub canonical_competitor_hash: Vec<u8>,
    pub height: i32,
    pub child_height: i32,
    pub header_time: i64,
}

pub async fn insert_stale_with_event(client: &Client, seed: StaleEventSeed) -> Result<()> {
    insert_block(
        client,
        &seed.hash,
        &seed.prev_hash,
        Some(seed.height),
        "stale",
        seed.header_time,
        Some(&seed.canonical_competitor_hash),
    )
    .await?;
    let event_id = insert_event(
        client,
        EventSeed {
            source_id: seed.source_id,
            child_height: seed.child_height,
            child_hash: hash_bytes(0xf000 + seed.child_height as u32),
            parent_hash: seed.hash.clone(),
            prev_hash: seed.prev_hash,
            parent_time: seed.header_time,
            kind: "stale",
            pow_validates_btc_target: true,
            btc_height: Some(seed.height),
            pool_id: None,
        },
    )
    .await?;
    insert_attestation_proof(
        client,
        &seed.hash,
        seed.source_id,
        &[event_id],
        seed.header_time,
    )
    .await
}

pub async fn set_block_pool(client: &Client, hash: &[u8], pool_id: i64) -> Result<()> {
    client
        .execute(
            "UPDATE block SET bitcoin_miner_pool_id = $2 WHERE btc_header_hash = $1",
            &[&hash, &pool_id],
        )
        .await?;
    Ok(())
}

pub async fn insert_attestation_proof(
    client: &Client,
    hash: &[u8],
    source_id: i64,
    event_ids: &[i64],
    ts: i64,
) -> Result<()> {
    let evidence = json!({ "contributing_event_ids": event_ids });
    client
        .execute(
            "INSERT INTO attestation_proof ( \
                btc_header_hash, source_id, proof_kind, evidence, pow_validated, \
                discovered_at, confirmed_at \
             ) VALUES ($1, $2, 'auxpow', $3, TRUE, $4, $4)",
            &[&hash, &source_id, &Json(&evidence), &ts],
        )
        .await?;
    Ok(())
}

pub fn test_header_chain(tip_height: i32, base_time: u32) -> BTreeMap<i32, Header> {
    let mut headers = BTreeMap::new();
    let mut prev = BlockHash::all_zeros();
    for height in 0..=tip_height {
        let header = Header {
            version: bitcoin::block::Version::ONE,
            prev_blockhash: prev,
            merkle_root: TxMerkleNode::all_zeros(),
            time: base_time + height as u32,
            bits: CompactTarget::from_consensus(0x207f_ffff),
            nonce: height as u32 + 1,
        };
        prev = header.block_hash();
        headers.insert(height, header);
    }
    headers
}

pub fn header_hash_bytes(header: &Header) -> Vec<u8> {
    header.block_hash().to_byte_array().to_vec()
}

pub fn header_hash_and_prev(header: &Header) -> (Vec<u8>, Vec<u8>) {
    (
        header_hash_bytes(header),
        header.prev_blockhash.to_byte_array().to_vec(),
    )
}

pub fn hash_bytes(n: u32) -> Vec<u8> {
    let mut bytes = vec![0u8; 32];
    bytes[28..].copy_from_slice(&n.to_be_bytes());
    bytes
}

pub fn display_hash(bytes: &[u8]) -> String {
    BlockHash::from_slice(bytes).unwrap().to_string()
}

pub fn header_bytes(hash: &[u8], prev_hash: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 80];
    bytes[4..36].copy_from_slice(prev_hash);
    bytes[36..68].copy_from_slice(hash);
    bytes
}

pub fn day_epoch(year: i32, month: Month, day: u8) -> i64 {
    query::epoch_start_of_day(Date::from_calendar_date(year, month, day).unwrap())
}
