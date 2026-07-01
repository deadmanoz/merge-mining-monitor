use anyhow::Result;
use mmm_capture::source_registry::{BITCOIN_SOURCE_CODE, NAMECOIN_SOURCE_CODE, RSK_SOURCE_CODE};
use mmm_producers::{BitcoinCoreSyncConfig, run_sync_bitcoin_core};
use mmm_store::get_source_id;
use serde_json::json;
use time::Month;

use crate::support::seed::{
    EventSeed, day_epoch, display_hash, hash_bytes, header_hash_and_prev, insert_attestation_proof,
    insert_block, insert_event, insert_pool, test_header_chain,
};

use crate::helpers::{
    FakeBitcoinCoreBackboneSource, expect_tree_api_error, project_tree, seed_canonical_chain,
};

#[tokio::test]
async fn compact_tree_keeps_nearby_stale_event_and_hides_canonical_spans() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);
        let hashes = seed_canonical_chain(&client, 0..=120, 0x8000, 0x7fff, ts, None).await?;
        let c60 = hashes[&60].clone();
        let c61 = hashes[&61].clone();
        let stale = hash_bytes(0x5a1e);
        insert_block(
            &client,
            &stale,
            &c60,
            Some(61),
            "stale",
            ts + 200,
            Some(&c61),
        )
        .await?;
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 61,
                child_hash: hash_bytes(0x6100),
                parent_hash: stale.clone(),
                prev_hash: c60.clone(),
                parent_time: ts + 200,
                kind: "stale",
                pow_validates_btc_target: true,
                btc_height: Some(61),
                pool_id: None,
            },
        )
        .await?;

        let payload = project_tree(
            &client,
            Some("at_height=60&context=compact&source=auxpow:namecoin&kinds=stale&min_sources=1"),
        )
        .await?;
        assert_eq!(payload.window.btc_height_min, Some(0));
        assert_eq!(payload.window.btc_height_max, Some(120));
        assert_eq!(payload.nodes.len(), 5);
        assert!(
            payload
                .nodes
                .iter()
                .any(|node| node.hash == display_hash(&stale))
        );
        assert!(payload.nodes.iter().any(|node| node.height == Some(60)));
        assert!(
            payload
                .nodes
                .iter()
                .any(|node| node.height == Some(61) && node.kind == "canonical")
        );
        assert_eq!(payload.window.hidden_linear_block_count, 117);
        let hidden_counts = payload
            .edges
            .iter()
            .filter(|edge| edge.edge_kind == "hidden")
            .map(|edge| edge.hidden_count.unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(hidden_counts, vec![59, 58]);

        Ok::<_, anyhow::Error>(())
    })
}

#[derive(Debug, Clone, Copy)]
enum InvalidOmittedSpan {
    MissingHeight,
    BrokenPrevLink,
}

async fn assert_compact_tree_shrinks_past_invalid_omitted_span(
    case: InvalidOmittedSpan,
) -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        match case {
            InvalidOmittedSpan::MissingHeight => {
                seed_canonical_chain(&client, 0..=120, 0x8100, 0x7fff, ts, Some(30)).await?;
            }
            InvalidOmittedSpan::BrokenPrevLink => {
                let hashes =
                    seed_canonical_chain(&client, 0..=120, 0x8000, 0x7fff, ts, None).await?;
                client
                    .execute(
                        "UPDATE block SET btc_prev_header_hash = $2 WHERE btc_header_hash = $1",
                        &[&hashes[&31], &hash_bytes(0xabcd)],
                    )
                    .await?;
            }
        }
        let payload = project_tree(&client, Some("at_height=60&context=compact")).await?;
        assert_eq!(payload.window.btc_height_min, Some(44), "{case:?}");
        assert_eq!(payload.window.btc_height_max, Some(76), "{case:?}");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn compact_tree_shrinks_past_invalid_omitted_span() -> Result<()> {
    for case in [
        InvalidOmittedSpan::MissingHeight,
        InvalidOmittedSpan::BrokenPrevLink,
    ] {
        assert_compact_tree_shrinks_past_invalid_omitted_span(case).await?;
    }
    Ok(())
}

#[tokio::test]
async fn tree_at_time_resolves_empty_and_nearest_complete_canonical_height() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        let h0 = hash_bytes(0x2000);
        let h1 = hash_bytes(0x2001);
        let h2 = hash_bytes(0x2002);
        let h3 = hash_bytes(0x2003);
        let h4 = hash_bytes(0x2004);
        insert_block(
            &client,
            &h0,
            &hash_bytes(0x1fff),
            Some(0),
            "canonical",
            ts + 100,
            None,
        )
        .await?;
        insert_block(&client, &h1, &h0, Some(1), "canonical", ts + 200, None).await?;
        insert_block(&client, &h2, &h1, Some(2), "canonical", ts + 200, None).await?;
        insert_block(&client, &h3, &h2, Some(3), "canonical", ts + 180, None).await?;
        insert_block(&client, &h4, &h3, Some(4), "canonical", ts + 210, None).await?;
        client
            .execute(
                "UPDATE block SET btc_coinbase_status = 'not_attempted' WHERE btc_header_hash = $1",
                &[&h4],
            )
            .await?;

        let empty = project_tree(&client, Some("at_time=2026-05-10T00:01:00Z")).await?;
        assert_eq!(empty.window.btc_height_min, None);
        assert_eq!(empty.window.btc_height_max, None);
        assert_eq!(
            empty.window.empty_reason,
            Some("no_complete_canonical_at_or_before_time")
        );
        assert!(!empty.window.defaulted_to_tip);
        assert!(empty.nodes.is_empty());

        let payload = project_tree(&client, Some("at_time=2026-05-10T00:03:10Z")).await?;
        assert_eq!(payload.window.btc_height_min, Some(3));
        assert_eq!(payload.window.btc_height_max, Some(3));
        assert!(!payload.window.defaulted_to_tip);
        assert_eq!(payload.nodes.len(), 1);
        assert_eq!(payload.nodes[0].height, Some(3));

        let payload = project_tree(&client, Some("at_time=2026-05-10T00:03:40Z")).await?;
        assert_eq!(payload.window.btc_height_min, Some(2));
        assert_eq!(payload.window.btc_height_max, Some(2));
        assert!(!payload.window.defaulted_to_tip);
        assert_eq!(payload.nodes.len(), 1);
        assert_eq!(payload.nodes[0].height, Some(2));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_defaults_to_local_canonical_tip_window() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        seed_canonical_chain(&client, 0..=150, 1, 0, ts, None).await?;
        let payload = project_tree(&client, None).await?;
        assert_eq!(payload.window.btc_height_min, Some(135));
        assert_eq!(payload.window.btc_height_max, Some(150));
        assert_eq!(payload.window.tip_height, Some(150));
        assert!(payload.window.defaulted_to_tip);
        assert_eq!(payload.window.empty_reason, None);
        assert_eq!(payload.nodes.len(), 16);
        assert_eq!(
            payload.nodes.first().and_then(|node| node.height),
            Some(135)
        );
        assert_eq!(payload.nodes.last().and_then(|node| node.height), Some(150));
        assert!(payload.nodes.iter().all(|node| node.kind == "canonical"));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_projects_backbone_synced_canonical_with_child_chain_evidence() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);
        let headers = test_header_chain(1, ts as u32);
        let (parent, prev) = header_hash_and_prev(&headers[&1]);
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 41,
                child_hash: hash_bytes(0x4100),
                parent_hash: parent.clone(),
                prev_hash: prev,
                parent_time: ts + 1,
                kind: "canonical",
                pow_validates_btc_target: true,
                btc_height: Some(1),
                pool_id: None,
            },
        )
        .await?;

        let source = FakeBitcoinCoreBackboneSource::new(1, headers);
        run_sync_bitcoin_core(
            &mut client,
            &source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(1),
                limit: 2,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        let payload = project_tree(&client, Some("from_height=1&to_height=1")).await?;
        assert_eq!(payload.nodes.len(), 1);
        let node = &payload.nodes[0];
        assert_eq!(node.height, Some(1));
        assert_eq!(node.source_summary.distinct_sources, 2);
        assert!(node.source_summary.live_observed);
        assert!(
            node.source_summary
                .sources
                .contains(&BITCOIN_SOURCE_CODE.to_string())
        );
        assert!(
            node.source_summary
                .sources
                .contains(&NAMECOIN_SOURCE_CODE.to_string())
        );
        assert_eq!(node.child_chain_evidence.len(), 1);
        assert_eq!(node.child_chain_evidence[0].source, NAMECOIN_SOURCE_CODE);
        assert_eq!(node.child_chain_evidence[0].event_count, 1);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_reports_unsynced_explicit_window_without_hydrating_core() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);
        let headers = test_header_chain(3, ts as u32);
        let (h1, prev1) = header_hash_and_prev(&headers[&1]);
        let (h2, prev2) = header_hash_and_prev(&headers[&2]);
        let (h3, prev3) = header_hash_and_prev(&headers[&3]);

        insert_block(&client, &h1, &prev1, Some(1), "canonical", ts + 1, None).await?;
        let event_id = insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 41,
                child_hash: hash_bytes(0x4100),
                parent_hash: h1.clone(),
                prev_hash: prev1,
                parent_time: ts + 1,
                kind: "canonical",
                pow_validates_btc_target: true,
                btc_height: Some(1),
                pool_id: None,
            },
        )
        .await?;
        insert_attestation_proof(&client, &h1, namecoin, &[event_id], ts + 2).await?;

        insert_block(&client, &h2, &prev2, None, "unknown", ts + 2, None).await?;
        insert_block(&client, &h3, &prev3, Some(3), "canonical", ts + 3, None).await?;

        let stale = hash_bytes(0x5a1e);
        insert_block(&client, &stale, &h2, Some(3), "stale", ts + 4, Some(&h3)).await?;
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 42,
                child_hash: hash_bytes(0x4200),
                parent_hash: stale.clone(),
                prev_hash: h2.clone(),
                parent_time: ts + 4,
                kind: "stale",
                pow_validates_btc_target: true,
                btc_height: Some(3),
                pool_id: None,
            },
        )
        .await?;

        let api = expect_tree_api_error(&client, "from_height=0&to_height=3").await?;
        assert_eq!(api.code(), "backbone_unsynced");
        let details = api.details();
        assert_eq!(details["from_height"], json!(0));
        assert_eq!(details["to_height"], json!(3));
        assert_eq!(details["first_missing_height"], json!(0));
        assert_eq!(details["missing_count"], json!(2));
        assert_eq!(details["partial_count"], json!(0));

        let not_promoted = client
            .query_one(
                "SELECT kind, btc_height, core_attested, live_observed \
                 FROM block WHERE btc_header_hash = $1",
                &[&h2],
            )
            .await?;
        assert_eq!(not_promoted.get::<_, String>(0), "unknown");
        assert_eq!(not_promoted.get::<_, Option<i32>>(1), None);
        assert!(!not_promoted.get::<_, bool>(2));
        assert!(!not_promoted.get::<_, bool>(3));

        let stale_row = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&stale],
            )
            .await?;
        assert_eq!(stale_row.get::<_, String>(0), "stale");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_node_infers_child_miner_when_coinbase_unknown() -> Result<()> {
    crate::run_db_test!(client, {
        // A stale node whose Bitcoin coinbase miner is unknown but whose RSK
        // child event resolves a pool: the tree label falls back to the
        // child-inferred miner rather than "unknown miner". The tree path needs
        // no RSK sidecar (it reads merge_mining_event.child_miner_pool_id only).
        let rsk = get_source_id(&client, RSK_SOURCE_CODE).await?;
        let f2pool = insert_pool(&client, "f2pool", "F2Pool").await?;
        let ts = day_epoch(2026, Month::May, 10);
        let hashes = seed_canonical_chain(&client, 0..=120, 0x8000, 0x7fff, ts, None).await?;
        let c60 = hashes[&60].clone();
        let c61 = hashes[&61].clone();
        let stale = hash_bytes(0x5a2e);
        insert_block(
            &client,
            &stale,
            &c60,
            Some(61),
            "stale",
            ts + 200,
            Some(&c61),
        )
        .await?;
        // No set_block_pool: the coinbase miner is unknown.
        insert_event(
            &client,
            EventSeed {
                source_id: rsk,
                child_height: 61,
                child_hash: hash_bytes(0x6101),
                parent_hash: stale.clone(),
                prev_hash: c60.clone(),
                parent_time: ts + 200,
                kind: "stale",
                pow_validates_btc_target: true,
                btc_height: Some(61),
                pool_id: Some(f2pool),
            },
        )
        .await?;

        let payload = project_tree(
            &client,
            Some("at_height=60&context=compact&source=auxpow:rsk&kinds=stale&min_sources=1"),
        )
        .await?;
        let node = payload
            .nodes
            .iter()
            .find(|node| node.hash == display_hash(&stale))
            .expect("stale node present in window");
        assert!(
            !node.bitcoin_miner_pool.known,
            "strict Bitcoin coinbase miner stays Unknown"
        );
        assert_eq!(node.display_miner_pool.id, Some(f2pool));
        assert_eq!(node.display_miner_basis, "child_inferred");

        Ok::<_, anyhow::Error>(())
    })
}
