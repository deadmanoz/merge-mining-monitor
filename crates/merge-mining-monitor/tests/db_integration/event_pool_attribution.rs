use anyhow::Result;
use mmm_capture::capture::{
    CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE, EventPoolAttribution,
    PoolAttributionConfidence, PoolAttributionSide,
};
use mmm_capture::child_payout::NAMECOIN_PAYOUT_ADDRESS_NAMESPACE;
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::{get_source_id, upsert_event_pool_attributions};
use serde_json::json;
use tokio_postgres::Client;

use crate::support::default_pool_snapshot;
use crate::support::seed::insert_namecoin_payout_identity;

#[tokio::test]
async fn child_payout_source_upgrade_preserves_row_when_snapshot_is_mixed() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let event_id = insert_minimal_event(&client, source_id).await?;

        let (_, pool_ids) = default_pool_snapshot(&client).await?;
        let f2pool_id = *pool_ids.get("f2pool").expect("f2pool in snapshot");
        let upgraded = "MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm";
        let still_unknown = "Mvfg5bLiyVxhv1sUURShZserivKLNy6Jzv";

        upsert_event_pool_attributions(
            &client,
            event_id,
            &[
                child_payout(upgraded, CHILD_COINBASE_OUTPUT_SOURCE, None, None),
                child_payout(still_unknown, CHILD_COINBASE_OUTPUT_SOURCE, None, None),
            ],
            1_000,
        )
        .await?;

        let identity_id = insert_namecoin_payout_identity(&client, f2pool_id, upgraded).await?;
        upsert_event_pool_attributions(
            &client,
            event_id,
            &[
                child_payout(
                    upgraded,
                    CHILD_PAYOUT_REGISTRY_SOURCE,
                    Some(f2pool_id),
                    Some(identity_id),
                ),
                child_payout(still_unknown, CHILD_COINBASE_OUTPUT_SOURCE, None, None),
            ],
            2_000,
        )
        .await?;

        let rows = client
            .query(
                "SELECT matched_value, source, pool_id, pool_identity_id, first_seen_at, last_seen_at \
                 FROM event_pool_attribution \
                 WHERE event_id = $1 \
                   AND side = 'child_block' \
                   AND namespace = $2 \
                 ORDER BY matched_value",
                &[&event_id, &NAMECOIN_PAYOUT_ADDRESS_NAMESPACE],
            )
            .await?;
        assert_eq!(rows.len(), 2);

        assert_eq!(rows[0].get::<_, String>("matched_value"), upgraded);
        assert_eq!(
            rows[0].get::<_, String>("source"),
            CHILD_PAYOUT_REGISTRY_SOURCE
        );
        assert_eq!(rows[0].get::<_, Option<i64>>("pool_id"), Some(f2pool_id));
        assert_eq!(
            rows[0].get::<_, Option<i64>>("pool_identity_id"),
            Some(identity_id)
        );
        assert_eq!(rows[0].get::<_, i64>("first_seen_at"), 1_000);
        assert_eq!(rows[0].get::<_, i64>("last_seen_at"), 2_000);

        assert_eq!(rows[1].get::<_, String>("matched_value"), still_unknown);
        assert_eq!(
            rows[1].get::<_, String>("source"),
            CHILD_COINBASE_OUTPUT_SOURCE
        );
        assert_eq!(rows[1].get::<_, Option<i64>>("pool_id"), None);
        assert_eq!(rows[1].get::<_, Option<i64>>("pool_identity_id"), None);
        assert_eq!(rows[1].get::<_, i64>("first_seen_at"), 1_000);
        assert_eq!(rows[1].get::<_, i64>("last_seen_at"), 2_000);

        Ok::<_, anyhow::Error>(())
    })
}

fn child_payout(
    address: &str,
    source: &'static str,
    pool_id: Option<i64>,
    pool_identity_id: Option<i64>,
) -> EventPoolAttribution {
    EventPoolAttribution {
        side: PoolAttributionSide::ChildBlock,
        namespace: NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
        match_kind: "payout_address",
        matched_value: address.to_owned(),
        pool_id,
        pool_identity_id,
        source,
        confidence: PoolAttributionConfidence::Medium,
        details: json!({ "address_source": "child_coinbase_outputs" }),
    }
}

async fn insert_minimal_event(client: &Client, source_id: i64) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_prev_header_hash, \
                btc_parent_header_bytes, btc_parent_header_time, \
                btc_parent_kind, pow_validates_btc_target, pow_validates_child_target, \
                btc_parent_coinbase_script, child_coinbase_script, \
                discovered_at, confirmed_at \
             ) VALUES ( \
                $1, 501199, $2, 1700000000, $3, $4, $5, 1700000000, \
                'unknown', true, true, $6, $7, 100, 200 \
             ) RETURNING id",
            &[
                &source_id,
                &vec![0xb0_u8; 32],
                &vec![0xb1_u8; 32],
                &vec![0xb2_u8; 32],
                &vec![0_u8; 80],
                &Some(b"\x03\x01\x02\x03/no-pool/".to_vec()),
                &Some(b"\x03\x01\x02\x03/no-child-tag/".to_vec()),
            ],
        )
        .await?
        .get(0))
}
