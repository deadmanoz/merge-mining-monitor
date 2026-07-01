use anyhow::Result;
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use bitcoin::{Amount, BlockHash, ScriptBuf, TxOut, WPubkeyHash};
use mmm_capture::capture::{
    CHILD_PAYOUT_REGISTRY_SOURCE, ClassificationProof, ResolvedPoolAttributions,
    build_event_payload,
};
use mmm_capture::source_registry::FRACTAL_SOURCE_CODE;
use mmm_producers::{ReclassifyPoolsConfig, run_reclassify_pools};
use mmm_store::{get_source_id, upsert_merge_mining_event};
use tokio_postgres::Client;

use crate::support::seed::{child_reward_rows, pool_id_for_slug};
use crate::support::{default_pool_snapshot, parse_auxpow_fixture};

const FRACTAL_REWARD_CHILD_HASH: &str =
    "59801838b43b2d38a87b1d295c7c535b1d6cced64fe5cb89ddeb7ff8c92e7e0e";

fn fractal_reward_child_hash() -> BlockHash {
    FRACTAL_REWARD_CHILD_HASH.parse().unwrap()
}

#[tokio::test]
async fn reclassify_pools_fills_fractal_reward_from_stored_outputs() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let source_id = get_source_id(&client, FRACTAL_SOURCE_CODE).await?;
        let event_id = insert_fractal_event_without_child_coinbase(&client, source_id).await?;
        set_child_coinbase_outputs(&client, event_id, fractal_reward_outputs()).await?;

        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;
        let identity_id = insert_fractal_reward_identity(
            &client,
            f2pool_id,
            "bc1qg4l3fvmsrnzuspuntv9yswwh7s58n08a59y3l7",
        )
        .await?;
        let stats = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(stats.child_pool_updates, 1);
        assert_eq!(
            child_reward_rows(&client, event_id, "fractal_reward_address").await?,
            [(
                CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                "bc1qg4l3fvmsrnzuspuntv9yswwh7s58n08a59y3l7".to_owned(),
                Some(f2pool_id),
                Some(identity_id),
            )]
        );

        let again = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(again.child_pool_updates, 0);

        Ok::<_, anyhow::Error>(())
    })
}

/// The new embedded-registry path: `reclassify-pools` self-seeds the embedded
/// `data/pools/child-identities/fractal_reward_address_registry.json` (no manual `pool_identity`
/// insert) and resolves a stored child reward output to the seeded pool. Parity
/// with the Elastos `capture_resolves_embedded_minerinfo_registry_end_to_end` /
/// `reclassify_pools_reresolves_existing_elastos_minerinfo_without_rpc` tests.
#[tokio::test]
async fn reclassify_pools_seeds_embedded_fractal_registry_and_resolves() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // The real F2Pool Fractal reward address shipped in the registry.
        const F2POOL_REGISTRY_ADDRESS: &str = "bc1qptpxl288ng4c7mg6klzu9t0are7nhqlfmtmk9k";
        default_pool_snapshot(&client).await?;
        let source_id = get_source_id(&client, FRACTAL_SOURCE_CODE).await?;
        let event_id = insert_fractal_event_without_child_coinbase(&client, source_id).await?;
        set_child_coinbase_outputs(&client, event_id, f2pool_registry_reward_outputs()).await?;

        // No pool_identity is inserted here: run_reclassify_pools seeds the embedded
        // registry itself, which is exactly the path under test.
        let stats = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(stats.child_pool_updates, 1);

        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;
        let identity_id: i64 = client
            .query_one(
                "SELECT id FROM pool_identity \
                 WHERE namespace = 'fractal_reward_address' AND identifier = $1",
                &[&F2POOL_REGISTRY_ADDRESS],
            )
            .await?
            .get(0);
        assert_eq!(
            child_reward_rows(&client, event_id, "fractal_reward_address").await?,
            [(
                CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                F2POOL_REGISTRY_ADDRESS.to_owned(),
                Some(f2pool_id),
                Some(identity_id),
            )]
        );

        Ok::<_, anyhow::Error>(())
    })
}

async fn insert_fractal_event_without_child_coinbase(
    client: &Client,
    source_id: i64,
) -> Result<i64> {
    let parsed = parse_auxpow_fixture("500000-valid-parent")?;
    let mut payload = build_event_payload(
        &parsed,
        Some(1_342_257),
        ResolvedPoolAttributions::default(),
        ClassificationProof::default(),
        1_800_000_000,
    )?;
    payload.child_block_hash = fractal_reward_child_hash().to_byte_array().to_vec();
    payload.child_coinbase_txid = None;
    payload.child_coinbase_script = None;
    payload.child_coinbase_outputs = None;
    upsert_merge_mining_event(client, source_id, &payload).await
}

async fn set_child_coinbase_outputs(
    client: &Client,
    event_id: i64,
    child_coinbase_outputs: Vec<u8>,
) -> Result<()> {
    client
        .execute(
            "UPDATE merge_mining_event SET child_coinbase_outputs = $2 WHERE id = $1",
            &[&event_id, &child_coinbase_outputs],
        )
        .await?;
    Ok(())
}

fn fractal_reward_outputs() -> Vec<u8> {
    let reward_hash: [u8; 20] = hex::decode("457f14b3701cc5c807935b0a4839d7f42879bcfd")
        .unwrap()
        .try_into()
        .unwrap();
    let outputs = vec![TxOut {
        value: Amount::from_sat(1),
        script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_slice(&reward_hash).unwrap()),
    }];
    serialize(&outputs)
}

/// A child coinbase output paying the witness-program hash that formats to the
/// registry's F2Pool Fractal reward address `bc1qptpxl288ng4c7mg6klzu9t0are7nhqlfmtmk9k`
/// (the same vector pinned in `mmm_capture::child_payout`'s formatter test).
fn f2pool_registry_reward_outputs() -> Vec<u8> {
    let reward_hash: [u8; 20] = hex::decode("0ac26fa8e79a2b8f6d1ab7c5c2adfd1e7d3b83e9")
        .unwrap()
        .try_into()
        .unwrap();
    let outputs = vec![TxOut {
        value: Amount::from_sat(1),
        script_pubkey: ScriptBuf::new_p2wpkh(&WPubkeyHash::from_slice(&reward_hash).unwrap()),
    }];
    serialize(&outputs)
}

async fn insert_fractal_reward_identity(
    client: &Client,
    pool_id: i64,
    address: &str,
) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO pool_identity (pool_id, namespace, identifier) \
             VALUES ($1, 'fractal_reward_address', $2) \
             RETURNING id",
            &[&pool_id, &address],
        )
        .await?
        .get(0))
}
