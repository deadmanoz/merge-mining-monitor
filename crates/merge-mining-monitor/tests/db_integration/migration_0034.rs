//! Migration 0034 on rows written before it: 0033 and 0034 run together, as in the
//! production window, over events seeded in the 0032 shape.

use anyhow::Result;
use tokio_postgres::Client;

use crate::support::db::{apply_migration_paths, new_test_db_before, teardown_test_db};
use crate::support::seed::header_bytes;

const NAMECOIN: &str = "auxpow:namecoin";
const RSK: &str = "auxpow:rsk";
const HATHOR: &str = "auxpow:hathor";

struct Seed {
    id: i64,
    source: &'static str,
    kind: &'static str,
    revoked: bool,
    merkle_root: u8,
}

fn hash(tag: u8, id: i64) -> Vec<u8> {
    let mut bytes = vec![tag; 32];
    bytes[31] = u8::try_from(id).unwrap();
    bytes
}

async fn seed_event(client: &Client, seed: &Seed) -> Result<()> {
    let near = seed.kind == "near";
    client
        .execute(
            "INSERT INTO merge_mining_event ( \
                id, source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_header_bytes, btc_parent_header_time, \
                btc_parent_height, btc_parent_kind, pow_validates_btc_target, \
                discovered_at, confirmed_at, revoked_at, revocation_reason, \
                btc_parent_prev_header_hash \
             ) OVERRIDING SYSTEM VALUE \
             SELECT $1::int8, s.id, ($1::int8 + 100)::int4, $2, 1000 + $1::int8, $3, $4, 1, \
                    $5, $6, $7, 1, 1, $8, $9, $10 \
               FROM source s WHERE s.code = $11",
            &[
                &seed.id,
                &hash(0xc0, seed.id),
                &hash(0xb0, seed.id),
                &header_bytes(&[seed.merkle_root; 32], &hash(0xa0, seed.id)),
                &(!near).then_some(900_000_i32 + i32::try_from(seed.id)?),
                &seed.kind,
                &!near,
                &seed.revoked.then_some(5_i64),
                &seed.revoked.then_some("seeded"),
                &hash(0xa0, seed.id),
                &seed.source,
            ],
        )
        .await?;
    Ok(())
}

async fn seed_rsk_sidecar(client: &Client, event_id: i64, merkle_proof: &[u8]) -> Result<()> {
    client
        .execute(
            "INSERT INTO rsk_merge_mining_evidence ( \
                event_id, rsk_block_hash, rsk_height, is_uncle, rsk_miner, \
                merge_mining_hash, merkle_proof, proof_format \
             ) VALUES ($1, $2, $3, FALSE, $4, $5, $6, 'rskj_rpc_opaque')",
            &[
                &event_id,
                &hash(0xc0, event_id),
                &(i32::try_from(event_id)? + 100),
                &vec![0x12_u8; 20],
                &hash(0xd0, event_id),
                &merkle_proof,
            ],
        )
        .await?;
    Ok(())
}

async fn seed_child_payout(client: &Client, event_id: i64) -> Result<()> {
    client
        .execute(
            "INSERT INTO event_pool_attribution ( \
                event_id, side, namespace, match_kind, matched_value, source, confidence, \
                first_seen_at, last_seen_at \
             ) VALUES ($1, 'child_block', 'namecoin_payout_address', 'payout_address', $2, \
                       'child_payout_registry', 'high', 1, 1)",
            &[&event_id, &format!("N1payout{event_id}")],
        )
        .await?;
    Ok(())
}

fn seed(id: i64, source: &'static str, kind: &'static str, revoked: bool, merkle_root: u8) -> Seed {
    Seed {
        id,
        source,
        kind,
        revoked,
        merkle_root,
    }
}

fn one_transaction_tree(root: u8) -> Vec<u8> {
    [&[1, 0, 0, 0, 1][..], &[root; 32], &[1, 1]].concat()
}

/// Evidence, active and revoked nears, RSK nears with each proof encoding, and a Hathor near.
async fn seed_pre_swap_rows(client: &Client) -> Result<()> {
    let seeds = [
        seed(1, NAMECOIN, "canonical", false, 1),
        seed(2, NAMECOIN, "near", false, 2),
        seed(3, NAMECOIN, "near", true, 3),
        seed(4, RSK, "near", false, 4),
        seed(5, RSK, "near", false, 5),
        seed(6, RSK, "near", false, 6),
        seed(7, RSK, "canonical", false, 7),
        seed(8, HATHOR, "near", false, 8),
    ];
    for seed in &seeds {
        seed_event(client, seed).await?;
    }
    seed_child_payout(client, 1).await?;
    seed_child_payout(client, 2).await?;
    seed_rsk_sidecar(client, 4, &[]).await?;
    seed_rsk_sidecar(client, 5, &one_transaction_tree(5)).await?;
    seed_rsk_sidecar(client, 6, &one_transaction_tree(0x66)).await?;
    seed_rsk_sidecar(client, 7, &[]).await?;
    client
        .execute(
            "INSERT INTO hathor_merge_mining_evidence ( \
                event_id, hathor_block_hash, hathor_height, aux_pow, funds_graph, \
                funds_graph_split, proof_format \
             ) VALUES (8, $1, 108, '\\x00', '\\x00', 2, 'hathor_rfc0006')",
            &[&hash(0xc0, 8)],
        )
        .await?;
    Ok(())
}

type ObservationRow = (i64, String, Option<Vec<u8>>, Option<Vec<u8>>);

#[tokio::test]
async fn the_swap_keeps_evidence_and_moves_nears_to_observations() -> Result<()> {
    let (client, schema, remaining) = new_test_db_before("0033").await?;
    let result = async {
        seed_pre_swap_rows(&client).await?;
        apply_migration_paths(&client, &schema, remaining).await?;

        let events: Vec<(i64, String, i64)> = client
            .query(
                "SELECT id, btc_parent_kind, observation_id FROM merge_mining_event ORDER BY id",
                &[],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2)))
            .collect();
        assert_eq!(
            events,
            vec![(1, "canonical".to_owned(), 1), (7, "canonical".to_owned(), 7)],
            "evidence events stay, linked to the observation 0033 gave them"
        );

        let observations: Vec<ObservationRow> = client
            .query(
                "SELECT id, observation_kind, btc_parent_header_hash, btc_parent_coinbase_txid \
                 FROM child_block_observation ORDER BY id",
                &[],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
            .collect();
        assert_eq!(
            observations,
            vec![
                (1, "evidence".to_owned(), Some(hash(0xb0, 1)), None),
                (2, "near".to_owned(), Some(hash(0xb0, 2)), None),
                (3, "unverified_parent".to_owned(), None, None),
                (4, "near".to_owned(), Some(hash(0xb0, 4)), Some(vec![4; 32])),
                (5, "near".to_owned(), Some(hash(0xb0, 5)), Some(vec![5; 32])),
                (6, "near".to_owned(), Some(hash(0xb0, 6)), None),
                (7, "evidence".to_owned(), Some(hash(0xb0, 7)), None),
                (8, "near".to_owned(), Some(hash(0xb0, 8)), None),
            ],
            "a revoked near keeps its child only, and a coinbase-only RSK proof names the parent coinbase"
        );

        let kept: (i64, i64, i64, i64, i64) = client
            .query_one(
                "SELECT (SELECT count(*) FROM rsk_merge_mining_evidence WHERE event_id = 7), \
                        (SELECT count(*) FROM rsk_merge_mining_evidence), \
                        (SELECT count(*) FROM event_pool_attribution WHERE event_id = 1), \
                        (SELECT count(*) FROM event_pool_attribution), \
                        (SELECT count(*) FROM hathor_merge_mining_evidence)",
                &[],
            )
            .await
            .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))?;
        assert_eq!(
            kept,
            (1, 1, 1, 1, 0),
            "only evidence events keep sidecar and attribution rows"
        );

        let ready: bool = client
            .query_one("SELECT source_health_ready FROM read_model_invariant", &[])
            .await?
            .get(0);
        assert!(!ready, "source health waits for its rebuild");

        let next_id: i64 = client
            .query_one(
                "INSERT INTO merge_mining_event ( \
                    source_id, child_block_hash, btc_parent_header_hash, btc_parent_header_bytes, \
                    btc_parent_header_time, btc_parent_kind, pow_validates_btc_target, \
                    discovered_at, confirmed_at, btc_parent_prev_header_hash, observation_id \
                 ) SELECT source_id, child_block_hash, btc_parent_header_hash, \
                          btc_parent_header_bytes, 1, 'unknown', TRUE, 1, 1, $1, id \
                     FROM child_block_observation WHERE id = 2 \
                 RETURNING id",
                &[&hash(0xa0, 9)],
            )
            .await?
            .get(0);
        assert_eq!(next_id, 9, "new event ids continue past every removed near id");
        let near_rejected = client
            .execute(
                "UPDATE merge_mining_event SET btc_parent_kind = 'near', \
                        pow_validates_btc_target = FALSE WHERE id = 9",
                &[],
            )
            .await;
        assert!(near_rejected.is_err(), "the event kind check rejects a near");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown_test_db(&client, &schema, result).await
}

#[tokio::test]
async fn the_swap_refuses_an_event_without_its_observation() -> Result<()> {
    let (client, schema, mut remaining) = new_test_db_before("0033").await?;
    let result = async {
        seed_event(&client, &seed(1, NAMECOIN, "canonical", false, 1)).await?;
        let swap = remaining.split_off(1);
        apply_migration_paths(&client, &schema, remaining).await?;
        // A later event that names another block's observation.
        seed_event(&client, &seed(2, NAMECOIN, "canonical", false, 2)).await?;
        client
            .execute(
                "UPDATE merge_mining_event SET observation_id = 1 WHERE id = 2",
                &[],
            )
            .await?;
        let error = apply_migration_paths(&client, &schema, swap)
            .await
            .expect_err("an event without its observation stops the swap");
        assert!(
            format!("{error:#}").contains("have no matching child_block_observation"),
            "{error:#}"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    teardown_test_db(&client, &schema, result).await
}
