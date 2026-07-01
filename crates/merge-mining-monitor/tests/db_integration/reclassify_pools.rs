use anyhow::Result;
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use bitcoin::{Address, Amount, PubkeyHash, ScriptBuf, TxOut};
use mmm_capture::capture::{
    BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE, BTC_POOL_SNAPSHOT_SOURCE,
    CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE, ClassificationProof,
    EventPoolAttribution, PoolAttributionConfidence, PoolAttributionSide, ResolvedPoolAttributions,
    build_event_payload,
};
use mmm_capture::child_payout::{
    NAMECOIN_PAYOUT_ADDRESS_NAMESPACE, SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
};
use mmm_capture::source_registry::{NAMECOIN_SOURCE_CODE, SYSCOIN_SOURCE_CODE};
use mmm_producers::{ReclassifyPoolsConfig, ReclassifyPoolsStats, run_reclassify_pools};
use mmm_store::{
    get_source_id, upsert_event_pool_attributions, upsert_merge_mining_event_with_attributions,
};
use serde_json::json;
use tokio_postgres::Client;

use crate::support::seed::{child_reward_rows, insert_namecoin_payout_identity, pool_id_for_slug};
use crate::support::{default_pool_snapshot, parse_auxpow_fixture};

fn parent_pool_attributions(pool_id: i64) -> ResolvedPoolAttributions {
    ResolvedPoolAttributions {
        attributions: vec![EventPoolAttribution {
            side: PoolAttributionSide::BtcParent,
            namespace: "btc_coinbase_tag",
            match_kind: "test_seed",
            matched_value: format!("test-parent-{pool_id}"),
            pool_id: Some(pool_id),
            pool_identity_id: None,
            source: BTC_POOL_SNAPSHOT_SOURCE,
            confidence: PoolAttributionConfidence::High,
            details: json!({}),
        }],
    }
}

async fn attribution_pool_id(
    client: &Client,
    event_id: i64,
    side: &str,
    source: Option<&str>,
) -> Result<Option<i64>> {
    let row = client
        .query_opt(
            "SELECT pool_id \
             FROM event_pool_attribution \
             WHERE event_id = $1 AND side = $2 \
               AND ($3::text IS NULL OR source = $3) \
             ORDER BY id \
             LIMIT 1",
            &[&event_id, &side, &source],
        )
        .await?;
    Ok(row.map(|row| row.get(0)).unwrap_or(None))
}

async fn parent_snapshot_attribution(
    client: &Client,
    event_id: i64,
) -> Result<(String, String, Option<i64>)> {
    let row = client
        .query_one(
            "SELECT match_kind, matched_value, pool_id \
             FROM event_pool_attribution \
             WHERE event_id = $1 \
               AND side = 'btc_parent' \
               AND source = $2",
            &[&event_id, &BTC_POOL_SNAPSHOT_SOURCE],
        )
        .await?;
    Ok((row.get(0), row.get(1), row.get(2)))
}

async fn run_reclassify_pools_overwrite(client: &mut Client) -> Result<ReclassifyPoolsStats> {
    run_reclassify_pools(
        client,
        ReclassifyPoolsConfig {
            overwrite: true,
            ..ReclassifyPoolsConfig::default()
        },
    )
    .await
}

async fn set_child_outputs(client: &Client, event_id: i64, outputs: Vec<u8>) -> Result<()> {
    client
        .execute(
            "UPDATE merge_mining_event SET child_coinbase_outputs = $2 WHERE id = $1",
            &[&event_id, &Some(outputs)],
        )
        .await?;
    Ok(())
}

fn namecoin_p2pkh_outputs(hashes: &[[u8; 20]]) -> Vec<u8> {
    let outputs = hashes
        .iter()
        .map(|hash| TxOut {
            value: Amount::from_sat(1),
            script_pubkey: ScriptBuf::new_p2pkh(&PubkeyHash::from_slice(hash).unwrap()),
        })
        .collect::<Vec<_>>();
    serialize(&outputs)
}

struct NamecoinEventSeed {
    source_id: i64,
    child_height: i32,
    child_hash_fill: u8,
    nonce_bump: u32,
    resolved_pools: ResolvedPoolAttributions,
    parent_coinbase_script: Option<Vec<u8>>,
    parent_coinbase_outputs: Option<Vec<u8>>,
    child_coinbase_script: Option<Vec<u8>>,
}

impl NamecoinEventSeed {
    fn new(source_id: i64, child_height: i32, child_hash_fill: u8, nonce_bump: u32) -> Self {
        Self {
            source_id,
            child_height,
            child_hash_fill,
            nonce_bump,
            resolved_pools: ResolvedPoolAttributions::default(),
            parent_coinbase_script: None,
            parent_coinbase_outputs: None,
            child_coinbase_script: None,
        }
    }
}

async fn insert_namecoin_event(client: &Client, seed: NamecoinEventSeed) -> Result<i64> {
    let parsed = parse_auxpow_fixture("500000-valid-parent")?;
    let mut header = parsed.parent_header.header;
    header.nonce = header.nonce.wrapping_add(seed.nonce_bump);
    let parent_hash = header.block_hash().to_byte_array().to_vec();

    let mut payload = build_event_payload(
        &parsed,
        Some(seed.child_height),
        seed.resolved_pools,
        ClassificationProof::default(),
        1_000,
    )?;
    payload.child_height = seed.child_height;
    payload.child_block_hash = vec![seed.child_hash_fill; 32];
    payload.btc_parent_header_hash = parent_hash.clone();
    payload.btc_parent_prev_header_hash = header.prev_blockhash.to_byte_array().to_vec();
    payload.btc_parent_header_bytes = serialize(&header);
    payload.btc_parent_header_time = header.time as i64;
    payload.btc_parent_coinbase_script = seed.parent_coinbase_script;
    payload.btc_parent_coinbase_outputs = seed.parent_coinbase_outputs;
    payload.child_coinbase_script = seed.child_coinbase_script;
    payload.child_coinbase_outputs = None;
    upsert_merge_mining_event_with_attributions(client, seed.source_id, &payload).await
}

#[tokio::test]
async fn reclassify_pools_fills_parent_pool_from_tag_and_outputs() -> Result<()> {
    use std::str::FromStr;

    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        // SpiderPool is intentionally outside older seed sets; reclassify
        // should self-seed the expanded snapshot before resolving this tag.
        let script = b"\x03\x01\x02\x03/SpiderPool/837/\x00\x00".to_vec();
        let tag_event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                parent_coinbase_script: Some(script),
                ..NamecoinEventSeed::new(source_id, 500_100, 0xa1, 7)
            },
        )
        .await?;

        // `btc-nuggets` is an address-only (tag-less) upstream pool; it resolves
        // only via a stored payout address in btc_parent_coinbase_outputs.
        let payout = "1BwZeHJo7b7M2op7VDfYnsmcpXsUYEcVHm";
        let script_pubkey = Address::from_str(payout)?
            .require_network(bitcoin::Network::Bitcoin)?
            .script_pubkey();
        let outputs = vec![TxOut {
            value: Amount::from_sat(625_000_000),
            script_pubkey,
        }];
        let outputs_bytes = serialize(&outputs);
        // A coinbase script that matches no pool tag, so resolution must fall
        // through to the payout-address path.
        let script = b"\x03\xde\xad\xbe/no-such-pool-tag/".to_vec();
        let outputs_event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                parent_coinbase_script: Some(script),
                parent_coinbase_outputs: Some(outputs_bytes),
                ..NamecoinEventSeed::new(source_id, 500_101, 0xa2, 11)
            },
        )
        .await?;

        let stats = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(stats.parent_pool_updates, 2);
        assert_eq!(stats.child_pool_updates, 0);

        let spiderpool_id = pool_id_for_slug(&client, "spiderpool").await?;
        let tag_after = attribution_pool_id(&client, tag_event_id, "btc_parent", None).await?;
        assert_eq!(tag_after, Some(spiderpool_id));
        let btc_nuggets_id = pool_id_for_slug(&client, "btc-nuggets").await?;
        let outputs_after =
            attribution_pool_id(&client, outputs_event_id, "btc_parent", None).await?;
        assert_eq!(outputs_after, Some(btc_nuggets_id));

        let again = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(again.parent_pool_updates, 0);
        assert_eq!(again.child_pool_updates, 0);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_pools_overwrite_reattributes_existing_btc_coinbase_pool() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let antpool_id = pool_id_for_slug(&client, "antpool").await?;
        let spiderpool_id = pool_id_for_slug(&client, "spiderpool").await?;

        // A row whose coinbase resolves to SpiderPool but is currently attributed
        // (a BTC-coinbase-derived pool) to antpool, e.g. after a prior tag fix.
        let event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                resolved_pools: parent_pool_attributions(antpool_id),
                parent_coinbase_script: Some(b"\x03\x01\x02\x03/SpiderPool/702/\x00".to_vec()),
                ..NamecoinEventSeed::new(source_id, 500_300, 0xe1, 21)
            },
        )
        .await?;

        // A second upgraded row already points at the resolved pool through the
        // current source, but its provenance tuple is stale. Since the API
        // exposes the tuple, `--overwrite` must refresh it even when `pool_id`
        // itself is unchanged.
        let stale_same_pool_event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                resolved_pools: parent_pool_attributions(spiderpool_id),
                parent_coinbase_script: Some(b"\x03\x01\x02\x03/SpiderPool/702/\x00".to_vec()),
                ..NamecoinEventSeed::new(source_id, 500_303, 0xe4, 24)
            },
        )
        .await?;

        // Fill-missing mode must not move an existing attribution, even when
        // the script now resolves elsewhere.
        let fill = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(fill.parent_pool_updates, 0);
        let after_fill = attribution_pool_id(&client, event_id, "btc_parent", None).await?;
        assert_eq!(after_fill, Some(antpool_id));
        let stale_before = parent_snapshot_attribution(&client, stale_same_pool_event_id).await?;
        assert_eq!(stale_before.0, "test_seed");

        // --overwrite re-attributes the existing source-scoped row to the
        // freshly resolved SpiderPool and refreshes stale same-pool provenance.
        let overwrite = run_reclassify_pools_overwrite(&mut client).await?;
        assert_eq!(overwrite.parent_pool_updates, 2);
        let after_overwrite = attribution_pool_id(&client, event_id, "btc_parent", None).await?;
        assert_eq!(after_overwrite, Some(spiderpool_id));
        let stale_after = parent_snapshot_attribution(&client, stale_same_pool_event_id).await?;
        assert_eq!(stale_after.0, "coinbase_tag");
        assert!(stale_after.1.contains("SpiderPool"));
        assert_eq!(stale_after.2, Some(spiderpool_id));

        let again = run_reclassify_pools_overwrite(&mut client).await?;
        assert_eq!(again.parent_pool_updates, 0);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_pools_fills_child_tag_and_payout_from_outputs() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let syscoin = get_source_id(&client, SYSCOIN_SOURCE_CODE).await?;
        let spiderpool_id = pool_id_for_slug(&client, "spiderpool").await?;
        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;
        let child_outputs = namecoin_p2pkh_outputs(&[[0; 20]]);
        let identity_id = insert_namecoin_payout_identity(
            &client,
            f2pool_id,
            "MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm",
        )
        .await?;
        let namecoin_event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                child_coinbase_script: Some(b"\x03\x01\x02\x03/SpiderPool/513/\x00".to_vec()),
                ..NamecoinEventSeed::new(namecoin, 500_410, 0xf2, 38)
            },
        )
        .await?;
        set_child_outputs(&client, namecoin_event_id, child_outputs.clone()).await?;

        let syscoin_event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                child_coinbase_script: Some(b"\x03\x01\x02\x03/no-child-tag/".to_vec()),
                ..NamecoinEventSeed::new(syscoin, 2_248_410, 0xf7, 43)
            },
        )
        .await?;
        set_child_outputs(&client, syscoin_event_id, child_outputs).await?;

        let stats = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(stats.child_pool_updates, 2);
        assert_eq!(stats.parent_pool_updates, 0);
        assert_eq!(
            attribution_pool_id(
                &client,
                namecoin_event_id,
                "child_block",
                Some(BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE)
            )
            .await?,
            Some(spiderpool_id)
        );

        let namecoin_rows = child_reward_rows(
            &client,
            namecoin_event_id,
            NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
        )
        .await?;
        assert_eq!(
            namecoin_rows,
            [(
                CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                "MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm".to_owned(),
                Some(f2pool_id),
                Some(identity_id),
            )]
        );

        let syscoin_rows =
            child_reward_rows(&client, syscoin_event_id, SYSCOIN_PAYOUT_ADDRESS_NAMESPACE).await?;
        assert_eq!(
            syscoin_rows,
            [(
                CHILD_COINBASE_OUTPUT_SOURCE.to_owned(),
                "SMJ12qn9jNCCXJnTYRz5Yu9ZenERqvYwfg".to_owned(),
                None,
                None,
            )]
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_pools_never_alters_rsk_miner_address_attribution() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // Seed the pool table so we have a real pool_id to attach. RSK
        // attribution comes from the child miner address, not coinbase.
        default_pool_snapshot(&client).await?;
        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;

        let rsk_source_id = get_source_id(&client, "auxpow:rsk").await?;
        // An RSK event: NULL coinbase scripts, child pool from miner-address
        // attribution. Insert directly so we control the NULL coinbase columns.
        let parent_hash: Vec<u8> = vec![0xb1; 32];
        let prev_hash: Vec<u8> = vec![0xb0; 32];
        let header_bytes: Vec<u8> = vec![0x00; 80];
        let child_block_hash: Vec<u8> = vec![0xc1; 32];
        let event_id: i64 = client
            .query_one(
                "INSERT INTO merge_mining_event ( \
                    source_id, child_height, child_block_hash, child_block_time, \
                    btc_parent_header_hash, btc_parent_prev_header_hash, \
                    btc_parent_header_bytes, btc_parent_header_time, \
                    btc_parent_kind, pow_validates_btc_target, pow_validates_child_target, \
                    btc_parent_coinbase_script, child_coinbase_script, \
                    discovered_at, confirmed_at \
                 ) VALUES ( \
                    $1, $2, $3, $4, $5, $6, $7, $8, 'unknown', true, false, \
                    NULL, NULL, 10, 20 \
                 ) RETURNING id",
                &[
                    &rsk_source_id,
                    &600_000_i32,
                    &child_block_hash,
                    &1_700_000_000_i64,
                    &parent_hash,
                    &prev_hash,
                    &header_bytes,
                    &1_700_000_000_i64,
                ],
            )
            .await?
            .get(0);
        upsert_event_pool_attributions(
            &client,
            event_id,
            &[EventPoolAttribution {
                side: PoolAttributionSide::ChildBlock,
                namespace: "rsk_miner_address",
                match_kind: "miner_address",
                matched_value: "0x0000000000000000000000000000000000000001".to_owned(),
                pool_id: Some(f2pool_id),
                pool_identity_id: None,
                source: "rsk_miner_registry",
                confidence: PoolAttributionConfidence::High,
                details: json!({}),
            }],
            20,
        )
        .await?;

        // Even with overwrite, coinbase reclassification must skip RSK
        // miner-address rows.
        let stats = run_reclassify_pools(
            &mut client,
            ReclassifyPoolsConfig {
                overwrite: true,
                ..ReclassifyPoolsConfig::default()
            },
        )
        .await?;
        assert_eq!(stats.parent_pool_updates, 0);
        assert_eq!(stats.child_pool_updates, 0);

        assert_eq!(
            attribution_pool_id(&client, event_id, "btc_parent", None).await?,
            None
        );
        assert_eq!(
            attribution_pool_id(&client, event_id, "child_block", None).await?,
            Some(f2pool_id)
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn source_filter_scopes_main_scan_to_one_source() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let syscoin = get_source_id(&client, SYSCOIN_SOURCE_CODE).await?;
        let spiderpool_id = pool_id_for_slug(&client, "spiderpool").await?;

        // Two events under different sources, each with a parent coinbase tag
        // that resolves to SpiderPool.
        let namecoin_event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                parent_coinbase_script: Some(b"\x03\x01\x02\x03/SpiderPool/901/\x00".to_vec()),
                ..NamecoinEventSeed::new(namecoin, 500_900, 0xd1, 61)
            },
        )
        .await?;
        let syscoin_event_id = insert_namecoin_event(
            &client,
            NamecoinEventSeed {
                parent_coinbase_script: Some(b"\x03\x01\x02\x03/SpiderPool/902/\x00".to_vec()),
                ..NamecoinEventSeed::new(syscoin, 2_500_902, 0xd2, 62)
            },
        )
        .await?;

        // `--only main --source <namecoin>` bounds the candidate scan to the
        // Namecoin source: only the Namecoin event is re-attributed.
        let stats = run_reclassify_pools(
            &mut client,
            ReclassifyPoolsConfig {
                run_rsk: false,
                run_hathor: false,
                run_elastos: false,
                source: Some(NAMECOIN_SOURCE_CODE.to_owned()),
                ..ReclassifyPoolsConfig::default()
            },
        )
        .await?;
        assert_eq!(stats.parent_pool_updates, 1);

        assert_eq!(
            attribution_pool_id(&client, namecoin_event_id, "btc_parent", None).await?,
            Some(spiderpool_id)
        );
        assert_eq!(
            attribution_pool_id(&client, syscoin_event_id, "btc_parent", None).await?,
            None
        );

        Ok::<_, anyhow::Error>(())
    })
}
