use super::*;
use mmm_capture::capture::{
    BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE,
};
use mmm_producers::{ReclassifyPoolsConfig, run_reclassify_pools};
use serde_json::Value;

const F2POOL_ADDRESS: &str = "qb1zpyh3ygrlnrdzk5nrz0wh9usmeex2tfanq89p5yyvqj3pj4tuzlgs4hc5uh";

fn replay_config() -> Result<ReclassifyPoolsConfig> {
    ReclassifyPoolsConfig::from_args([
        "--only",
        "main",
        "--source",
        QBIT_SOURCE_CODE,
        "--batch-size",
        "2",
    ])
}

#[tokio::test]
async fn native_qbit_payouts_capture_and_replay_without_changing_base_evidence() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let context = AuxpowCaptureContext::new_with_classifier(
            &client,
            by_id(ChainId::Qbit),
            ConfiguredParentClassifier::Disabled,
        )
        .await?;
        let source_id = get_source_id(&client, QBIT_SOURCE_CODE).await?;
        let cases = [
            (
                78058,
                include_str!("../../../../../fixtures/qbit/78058.hex"),
                Some("f2pool"),
                "high",
            ),
            (
                8197,
                include_str!("../../../../../fixtures/qbit/8197.hex"),
                Some("secpool"),
                "medium",
            ),
            (
                2450,
                include_str!("../../../../../fixtures/qbit/2450.hex"),
                Some("luxor"),
                "medium",
            ),
            (
                7887,
                include_str!("../../../../../fixtures/qbit/7887.hex"),
                None,
                "medium",
            ),
        ];
        for (height, raw, slug, confidence) in cases {
            let rpc = FixtureBitcoindRpc::carrying(height, hex::decode(raw.trim())?);
            assert_eq!(
                process_auxpow_height(&mut client, &rpc, &context, height).await?,
                AuxpowHeightOutcome::AuxpowWritten
            );
            assert_payout(&client, source_id, height, slug, confidence).await?;
        }
        let preserved = preserved_evidence(&client, source_id).await?;
        // The pre-feature state: authenticated outputs and tags, no payout rows.
        client
            .execute(
                "DELETE FROM event_pool_attribution WHERE namespace='qbit_payout_address'",
                &[],
            )
            .await?;
        let stats = run_reclassify_pools(&mut client, replay_config()?).await?;
        assert_eq!(stats.child_pool_updates, 4);
        assert_eq!(stats.parent_pool_updates, 0);
        for (height, _, slug, confidence) in cases {
            assert_payout(&client, source_id, height, slug, confidence).await?;
        }
        assert_eq!(preserved_evidence(&client, source_id).await?, preserved);
        let attribution_rows: Value = client
            .query_one(
                "SELECT jsonb_agg(to_jsonb(a) ORDER BY a.id) FROM event_pool_attribution a",
                &[],
            )
            .await?
            .get(0);
        let repeat = run_reclassify_pools(&mut client, replay_config()?).await?;
        assert_eq!(repeat.child_pool_updates, 0);
        assert_eq!(repeat.parent_pool_updates, 0);
        let after: Value = client
            .query_one(
                "SELECT jsonb_agg(to_jsonb(a) ORDER BY a.id) FROM event_pool_attribution a",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(after, attribution_rows);
        Ok::<_, anyhow::Error>(())
    })
}

async fn assert_payout(
    client: &Client,
    source_id: i64,
    height: i32,
    slug: Option<&str>,
    confidence: &str,
) -> Result<()> {
    let row = client
        .query_one(
            "SELECT e.btc_parent_header_hash, o.child_pool_id, a.pool_id, a.pool_identity_id, \
         a.source, a.confidence, p.slug, a.matched_value, a.details \
         FROM merge_mining_event e JOIN event_pool_attribution a ON a.event_id=e.id \
         JOIN child_block_observation o ON o.id = e.observation_id \
         LEFT JOIN pool p ON p.id=a.pool_id \
         WHERE e.source_id=$1 AND e.child_height=$2 AND a.namespace='qbit_payout_address'",
            &[&source_id, &height],
        )
        .await?;
    assert_eq!(row.get::<_, Option<String>>(6).as_deref(), slug);
    assert_eq!(row.get::<_, String>(5), confidence);
    assert_eq!(row.get::<_, Option<i64>>(1), row.get::<_, Option<i64>>(2));
    assert_eq!(row.get::<_, Option<i64>>(3).is_some(), slug.is_some());
    assert_eq!(
        row.get::<_, String>(4),
        if slug.is_some() {
            CHILD_PAYOUT_REGISTRY_SOURCE
        } else {
            "child_coinbase_output"
        }
    );
    assert_eq!(
        row.get::<_, Value>(8)["address_source"],
        "child_coinbase_outputs"
    );
    let parent = BlockHash::from_slice(&row.get::<_, Vec<u8>>(0))?;
    let projected = mmm_api::projection::block(client, &parent.to_string())
        .await
        .map_err(|err| anyhow::anyhow!("block projection: {err:?}"))?;
    let payload = serde_json::to_value(projected)?;
    let event = payload["event_details"]
        .as_array()
        .unwrap()
        .iter()
        .find(|event| event["source"] == QBIT_SOURCE_CODE && event["child_height"] == height)
        .unwrap();
    assert_eq!(event["child_miner_pool"]["slug"].as_str(), slug);
    let payout = event["pool_attributions"]["child_block"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["namespace"] == "qbit_payout_address")
        .unwrap();
    assert_eq!(payout["confidence"], confidence);
    assert_eq!(payout["pool_identity"].is_object(), slug.is_some());
    if slug == Some("f2pool") {
        assert_eq!(row.get::<_, String>(7), F2POOL_ADDRESS);
        assert_eq!(
            event["pool_attributions"]["child_block"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }
    Ok(())
}

async fn preserved_evidence(client: &Client, source_id: i64) -> Result<Value> {
    Ok(client
        .query_one(
            "SELECT jsonb_build_object( \
         'events', (SELECT jsonb_agg(to_jsonb(e) ORDER BY e.id) \
                    FROM merge_mining_event e WHERE e.source_id=$1), \
         'parent_attributions', (SELECT jsonb_agg(to_jsonb(a) ORDER BY a.id) \
                    FROM event_pool_attribution a JOIN merge_mining_event e ON e.id=a.event_id \
                    WHERE e.source_id=$1 AND a.side='btc_parent'))",
            &[&source_id],
        )
        .await?
        .get(0))
}

#[tokio::test]
async fn qbit_payout_only_and_conflicting_child_tags_keep_existing_resolution_policy() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        let context = AuxpowCaptureContext::new_with_classifier(
            &client,
            by_id(ChainId::Qbit),
            ConfiguredParentClassifier::Disabled,
        )
        .await?;
        let (height, raw, _) = qbit_positive_control();
        let rpc = FixtureBitcoindRpc::carrying(height, raw);
        process_auxpow_height(&mut client, &rpc, &context, height).await?;
        let event_id: i64 = client
            .query_one("SELECT id FROM merge_mining_event WHERE source_id=36", &[])
            .await?
            .get(0);
        // Legacy retained outputs can lack a child script. Replay must still map
        // the curated F2Pool payout at high confidence without a matching tag.
        client
            .execute(
                "UPDATE child_block_observation SET child_coinbase_script=NULL \
                 WHERE id = (SELECT observation_id FROM merge_mining_event WHERE id=$1)",
                &[&event_id],
            )
            .await?;
        client
            .execute(
                "DELETE FROM event_pool_attribution WHERE event_id=$1 AND side='child_block'",
                &[&event_id],
            )
            .await?;
        run_reclassify_pools(&mut client, replay_config()?).await?;
        let row = client.query_one(
            "SELECT p.slug, a.confidence FROM merge_mining_event e \
             JOIN child_block_observation o ON o.id = e.observation_id \
             JOIN pool p ON p.id=o.child_pool_id \
             JOIN event_pool_attribution a ON a.event_id=e.id AND a.side='child_block' WHERE e.id=$1",
            &[&event_id],
        ).await?;
        assert_eq!(row.get::<_, String>(0), "f2pool");
        assert_eq!(row.get::<_, String>(1), "high");
        let antpool: i64 = client
            .query_one("SELECT id FROM pool WHERE slug='antpool'", &[])
            .await?
            .get(0);
        let mut conflict = mmm_capture::capture::EventPoolAttribution::from_legacy_child_pool_match(
            &mmm_capture::pool_resolver::PoolResolver::from_default_snapshot()?
                .resolve_coinbase_script(b"/AntPool/")
                .expect("known child tag"),
            antpool,
        );
        conflict.source = BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE;
        mmm_store::upsert_event_pool_attributions(&client, event_id, &[conflict], 1_000).await?;
        mmm_store::sync_event_observation_attribution(&client, event_id).await?;
        assert!(
            client
                .query_one(
                    "SELECT o.child_pool_id FROM merge_mining_event e \
                     JOIN child_block_observation o ON o.id = e.observation_id WHERE e.id=$1",
                    &[&event_id]
                )
                .await?
                .get::<_, Option<i64>>(0)
                .is_none()
        );
        let repeat = run_reclassify_pools(&mut client, replay_config()?).await?;
        assert_eq!(repeat.child_pool_updates, 0);
        assert_eq!(client.query_one("SELECT count(*) FROM event_pool_attribution WHERE event_id=$1 AND side='child_block'", &[&event_id]).await?.get::<_, i64>(0), 2);
        Ok::<_, anyhow::Error>(())
    })
}
