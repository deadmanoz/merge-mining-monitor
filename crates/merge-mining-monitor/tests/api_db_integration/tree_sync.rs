use anyhow::Result;
use mmm_api::projection::{self, ProjectionError};
use mmm_api::query::{self};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_producers::{BitcoinCoreSyncConfig, run_sync_bitcoin_core};
use mmm_read_model::{compute_source_health_from_base, rebuild_source_health};
use mmm_store::get_source_id;
use serde_json::json;
use time::Month;
use tokio_postgres::Client;

use crate::support::seed::{
    EventSeed, day_epoch, hash_bytes, header_hash_and_prev, insert_block, insert_event,
    test_header_chain,
};

use crate::helpers::{
    FakeBitcoinCoreBackboneSource, expect_tree_api_error, format_api_error, project_tree,
    seed_canonical_chain,
};

/// `sync-bitcoin-core` can flip an event-parent's `block.kind` from unknown to
/// canonical. This asserts the sync wrapper maintains `source_health` inline by
/// comparing it with a fresh recompute.
#[tokio::test]
async fn sync_bitcoin_core_maintains_source_health_on_kind_flip() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 12);
        let headers = test_header_chain(3, ts as u32);
        let source = FakeBitcoinCoreBackboneSource::new(3, headers.clone());
        let (h1, prev1) = header_hash_and_prev(&headers[&1]);
        let (h2, prev2) = header_hash_and_prev(&headers[&2]);
        let (h3, prev3) = header_hash_and_prev(&headers[&3]);

        // Canonical anchors at heights 1 and 3 frame the window; h2 is an UNKNOWN
        // block that is also a namecoin event-parent.
        insert_block(&client, &h1, &prev1, Some(1), "canonical", ts + 1, None).await?;
        insert_block(&client, &h2, &prev2, None, "unknown", ts + 2, None).await?;
        insert_block(&client, &h3, &prev3, Some(3), "canonical", ts + 3, None).await?;
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 42,
                child_hash: hash_bytes(0x4200),
                parent_hash: h2.clone(),
                prev_hash: prev2,
                parent_time: ts + 2,
                kind: "unknown",
                pow_validates_btc_target: true,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;

        // Populate source_health from base: namecoin's h2 is unknown.
        rebuild_source_health(&mut client).await?;
        let unknown_before: i64 = client
            .query_one(
                "SELECT unknown_parents FROM source_health WHERE source_id = $1",
                &[&namecoin],
            )
            .await?
            .get(0);
        assert_eq!(
            unknown_before, 1,
            "h2 is an unknown event-parent before Core backbone sync"
        );

        // Drive the Core backbone sync: the fake Core source canonicalizes h2's
        // height, flipping block.kind unknown -> canonical.
        run_sync_bitcoin_core(
            &mut client,
            &source,
            BitcoinCoreSyncConfig {
                from_height: Some(2),
                to_height: Some(2),
                limit: 1,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        // The sync wrapper must have maintained source_health to equal a fresh
        // recompute (h2 moved unknown -> canonical for namecoin).
        let computed = compute_source_health_from_base(&client).await?;
        let nc = computed
            .rows
            .iter()
            .find(|r| r.source_id == namecoin)
            .expect("namecoin present in recompute");
        let row = client
            .query_one(
                "SELECT unknown_parents, canonical_parents \
                 FROM source_health WHERE source_id = $1",
                &[&namecoin],
            )
            .await?;
        assert_eq!(
            row.get::<_, i64>(0),
            nc.unknown_parents,
            "maintained unknown == recompute"
        );
        assert_eq!(
            row.get::<_, i64>(1),
            nc.canonical_parents,
            "maintained canonical == recompute"
        );
        assert_eq!(
            nc.unknown_parents, 0,
            "h2 flipped out of the unknown bucket"
        );
        assert_eq!(
            nc.canonical_parents, 1,
            "h2 flipped into the canonical bucket"
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_keeps_canonical_attach_parent_for_reduced_stale_branch() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);
        let canonical = seed_canonical_chain(&client, 1..=600, 1, 0, ts + 1, None).await?;
        let c499 = canonical[&499].clone();
        let c500 = canonical[&500].clone();

        let stale = hash_bytes(0x5500);
        insert_block(
            &client,
            &stale,
            &c499,
            Some(500),
            "stale",
            ts + 700,
            Some(&c500),
        )
        .await?;
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 90,
                child_hash: hash_bytes(0x5600),
                parent_hash: stale,
                prev_hash: c499,
                parent_time: ts + 700,
                kind: "stale",
                pow_validates_btc_target: true,
                btc_height: Some(500),
                pool_id: None,
            },
        )
        .await?;

        let payload = project_tree(
            &client,
            Some("from_height=1&to_height=600&source=auxpow:namecoin&kinds=stale&min_sources=1"),
        )
        .await?;
        let stale_node = payload
            .nodes
            .iter()
            .find(|node| node.kind == "stale")
            .expect("stale node is retained");
        assert!(
            payload
                .nodes
                .iter()
                .any(|node| { node.kind == "canonical" && node.hash == stale_node.prev_hash })
        );
        let edge = payload
            .edges
            .iter()
            .find(|edge| edge.to_hash == stale_node.hash)
            .expect("stale node remains attached");
        assert_eq!(edge.edge_kind, "stale_entry");
        assert_eq!(edge.from_hash, stale_node.prev_hash);

        Ok::<_, anyhow::Error>(())
    })
}

async fn seed_filtered_stale_branch(client: &Client) -> Result<()> {
    let namecoin = get_source_id(client, NAMECOIN_SOURCE_CODE).await?;
    let ts = day_epoch(2026, Month::May, 10);
    let s101 = hash_bytes(0x0201);
    let s102 = hash_bytes(0x0202);
    let canonical = seed_canonical_chain(client, 100..=102, 0x0100, 0x0099, ts + 100, None).await?;
    let c100 = canonical[&100].clone();
    let c101 = canonical[&101].clone();
    let c102 = canonical[&102].clone();

    insert_block(
        client,
        &s101,
        &c100,
        Some(101),
        "stale",
        ts + 103,
        Some(&c101),
    )
    .await?;
    insert_block(
        client,
        &s102,
        &s101,
        Some(102),
        "stale",
        ts + 104,
        Some(&c102),
    )
    .await?;
    insert_event(
        client,
        EventSeed {
            source_id: namecoin,
            child_height: 50,
            child_hash: hash_bytes(0x0301),
            parent_hash: s102,
            prev_hash: s101,
            parent_time: ts + 104,
            kind: "stale",
            pow_validates_btc_target: true,
            btc_height: Some(102),
            pool_id: None,
        },
    )
    .await?;

    Ok(())
}

#[tokio::test]
async fn tree_includes_full_stale_branch_when_one_member_matches_filter() -> Result<()> {
    crate::run_db_test!(client, {
        seed_filtered_stale_branch(&client).await?;

        let payload = project_tree(
            &client,
            Some("from_height=100&to_height=102&source=auxpow:namecoin&kinds=stale&min_sources=1"),
        )
        .await?;
        let stale_nodes = payload
            .nodes
            .iter()
            .filter(|node| node.kind == "stale")
            .collect::<Vec<_>>();
        assert_eq!(stale_nodes.len(), 2);
        assert!(
            stale_nodes
                .iter()
                .any(|node| node.source_summary.distinct_sources == 0)
        );
        assert!(
            stale_nodes
                .iter()
                .any(|node| node.source_summary.distinct_sources == 1)
        );
        assert_eq!(payload.branches.len(), 1);
        assert_eq!(payload.branches[0].depth, 2);
        assert_eq!(payload.branches[0].member_hashes.len(), 2);
        assert_eq!(payload.branches[0].canonical_competitor_hashes.len(), 2);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_errors_when_emitted_stale_block_has_no_derivable_competition() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);
        let s101 = hash_bytes(0x0501);
        let noncanonical_competitor = hash_bytes(0x0502);
        let canonical =
            seed_canonical_chain(&client, 100..=101, 0x0400, 0x0399, ts + 100, None).await?;
        let c100 = canonical[&100].clone();
        insert_block(
            &client,
            &noncanonical_competitor,
            &c100,
            None,
            "unknown",
            ts + 101,
            None,
        )
        .await?;

        insert_block(
            &client,
            &s101,
            &c100,
            Some(101),
            "stale",
            ts + 102,
            Some(&noncanonical_competitor),
        )
        .await?;
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 60,
                child_hash: hash_bytes(0x0601),
                parent_hash: s101,
                prev_hash: c100,
                parent_time: ts + 102,
                kind: "stale",
                pow_validates_btc_target: true,
                btc_height: Some(101),
                pool_id: None,
            },
        )
        .await?;

        let query = query::parse_tree_query(Some(
            "from_height=100&to_height=101&source=auxpow:namecoin&kinds=stale&min_sources=1",
        ))
        .map_err(format_api_error)?;
        let err = match projection::tree(&client, &query).await {
            Ok(_) => anyhow::bail!("expected projection error"),
            Err(err) => err,
        };
        assert!(matches!(err, ProjectionError::Internal(_)));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_reports_unsynced_when_canonical_coinbase_is_incomplete() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        let hash = hash_bytes(0x0101);
        insert_block(
            &client,
            &hash,
            &hash_bytes(0x0100),
            Some(1),
            "canonical",
            ts,
            None,
        )
        .await?;
        client
            .execute(
                "UPDATE block \
                 SET btc_coinbase_txid = NULL, \
                     btc_coinbase_script = NULL, \
                     btc_coinbase_outputs = NULL, \
                     btc_coinbase_status = 'not_attempted' \
                 WHERE btc_header_hash = $1",
                &[&hash],
            )
            .await?;

        let api = expect_tree_api_error(&client, "from_height=1&to_height=1").await?;
        assert_eq!(api.code(), "backbone_unsynced");
        let details = api.details();
        assert_eq!(details["from_height"], json!(1));
        assert_eq!(details["to_height"], json!(1));
        assert_eq!(details["first_missing_height"], json!(1));
        assert_eq!(details["missing_count"], json!(0));
        assert_eq!(details["partial_count"], json!(1));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn tree_reports_backbone_conflict_details() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        insert_block(
            &client,
            &hash_bytes(0x0110),
            &hash_bytes(0x010f),
            Some(10),
            "canonical",
            ts,
            None,
        )
        .await?;
        insert_block(
            &client,
            &hash_bytes(0x0111),
            &hash_bytes(0x010f),
            Some(10),
            "canonical",
            ts,
            None,
        )
        .await?;

        let api = expect_tree_api_error(&client, "from_height=10&to_height=10").await?;
        assert_eq!(api.code(), "backbone_conflict");
        let details = api.details();
        assert_eq!(details["conflict_height"], json!(10));
        assert_eq!(
            details["conflict_reason"],
            json!("duplicate_canonical_height")
        );
        assert_eq!(details["conflict_count"], json!(1));
        assert_eq!(details["hashes"].as_array().map(Vec::len), Some(2));

        let h20 = hash_bytes(0x0120);
        insert_block(
            &client,
            &h20,
            &hash_bytes(0x011f),
            Some(20),
            "canonical",
            ts,
            None,
        )
        .await?;
        insert_block(
            &client,
            &hash_bytes(0x0121),
            &hash_bytes(0x9999),
            Some(21),
            "canonical",
            ts + 1,
            None,
        )
        .await?;

        let api = expect_tree_api_error(&client, "from_height=20&to_height=21").await?;
        assert_eq!(api.code(), "backbone_conflict");
        let details = api.details();
        assert_eq!(details["conflict_height"], json!(21));
        assert_eq!(details["conflict_reason"], json!("link_mismatch"));
        assert_eq!(details["hashes"].as_array().map(Vec::len), Some(1));

        // Keep the fixture meaningful: the stored previous hash is not h20.
        let h21_prev: Vec<u8> = client
            .query_one(
                "SELECT btc_prev_header_hash FROM block WHERE btc_height = 21",
                &[],
            )
            .await?
            .get(0);
        assert_ne!(h21_prev, h20);

        Ok::<_, anyhow::Error>(())
    })
}
