//! Retiring the annotation path preserves stored evidence and the v1 nullable
//! fields, but legacy rows no longer affect current API projections.

use anyhow::Result;
use mmm_api::projection::{self};
use time::Month;
use tokio_postgres::Client;

use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::get_source_id;

use crate::helpers::{format_projection_error, project_tree, seed_canonical_chain};
use crate::support::seed::{
    EventSeed, day_epoch, display_hash, hash_bytes, insert_block, insert_event,
};

const RULE: &str = "bad-blk-sigops";
const EVIDENCE_URL: &str = "https://b10c.me/observations/11-invalid-blocks-783426-and-784121/";

async fn seed_competing_stales(client: &Client) -> Result<(Vec<u8>, Vec<u8>)> {
    let ts = day_epoch(2026, Month::May, 12);
    let canonical = hash_bytes(0x7b01);
    let annotated = hash_bytes(0x7b02);
    let plain = hash_bytes(0x7b03);
    insert_block(
        client,
        &canonical,
        &hash_bytes(0x7b00),
        Some(300),
        "canonical",
        ts,
        None,
    )
    .await?;
    insert_block(
        client,
        &annotated,
        &hash_bytes(0x7b00),
        Some(300),
        "stale",
        ts + 1,
        Some(&canonical),
    )
    .await?;
    insert_block(
        client,
        &plain,
        &hash_bytes(0x7b00),
        Some(300),
        "stale",
        ts + 2,
        Some(&canonical),
    )
    .await?;
    seed_legacy_annotation(client, &annotated).await?;
    Ok((annotated, plain))
}

#[tokio::test]
async fn legacy_annotations_are_retained_but_not_projected() -> Result<()> {
    crate::run_db_test!(client, {
        let (annotated, plain) = seed_competing_stales(&client).await?;

        let payload = projection::block(&client, &display_hash(&annotated))
            .await
            .map_err(format_projection_error)?;
        assert_eq!(payload.block.kind, "stale");
        assert!(payload.block.error_block_reason.is_none());
        assert!(payload.block.body_invalid.is_none());
        let retained: i64 = client
            .query_one(
                "SELECT count(*) FROM body_invalid_stale WHERE hash = $1",
                &[&annotated],
            )
            .await?
            .get(0);
        assert_eq!(retained, 1, "retirement must not erase legacy evidence");
        // The annotation must not remove ordinary stale semantics.
        assert!(payload.competition.is_some());

        let plain_payload = projection::block(&client, &display_hash(&plain))
            .await
            .map_err(format_projection_error)?;
        assert_eq!(plain_payload.block.kind, "stale");
        assert!(plain_payload.block.body_invalid.is_none());
        Ok(())
    })
}

#[tokio::test]
async fn tree_ignores_retired_annotation_rows() -> Result<()> {
    crate::run_db_test!(client, {
        // The tree window gate needs a contiguous complete canonical run, so
        // seed a full chain and hang the competing stales off it.
        let ts = day_epoch(2026, Month::May, 12);
        let hashes = seed_canonical_chain(&client, 0..=120, 0x7b00, 0x7aff, ts, None).await?;
        let c60 = hashes[&60].clone();
        let c61 = hashes[&61].clone();
        let annotated = hash_bytes(0x7bf1);
        let plain = hash_bytes(0x7bf2);
        insert_block(
            &client,
            &annotated,
            &c60,
            Some(61),
            "stale",
            ts + 200,
            Some(&c61),
        )
        .await?;
        insert_block(
            &client,
            &plain,
            &c60,
            Some(61),
            "stale",
            ts + 201,
            Some(&c61),
        )
        .await?;
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        for (index, stale_hash) in [&annotated, &plain].into_iter().enumerate() {
            insert_event(
                &client,
                EventSeed {
                    source_id: namecoin,
                    child_height: 61,
                    child_hash: hash_bytes(0x6100 + u32::try_from(index)?),
                    parent_hash: stale_hash.clone(),
                    prev_hash: c60.clone(),
                    parent_time: ts + 200 + i64::try_from(index)?,
                    kind: "stale",
                    pow_validates_btc_target: true,
                    btc_height: Some(61),
                    pool_id: None,
                },
            )
            .await?;
        }
        seed_legacy_annotation(&client, &annotated).await?;
        let annotated_hash = display_hash(&annotated);
        let plain_hash = display_hash(&plain);

        let tree = project_tree(&client, Some("from_height=55&to_height=65")).await?;
        let annotated_node = tree
            .nodes
            .iter()
            .find(|node| node.hash == annotated_hash)
            .expect("annotated stale in tree window");
        assert_eq!(annotated_node.kind, "stale");
        assert!(annotated_node.body_invalid_rule.is_none());
        let plain_node = tree
            .nodes
            .iter()
            .find(|node| node.hash == plain_hash)
            .expect("plain stale in tree window");
        assert!(plain_node.body_invalid_rule.is_none());
        Ok(())
    })
}

async fn seed_legacy_annotation(client: &Client, hash: &[u8]) -> Result<()> {
    client.execute(
        "INSERT INTO body_invalid_stale (hash, btc_height, rule, evidence_url, source_label, imported_at) VALUES ($1, 300, $2, $3, 'retained-old-release', 1)",
        &[&hash, &RULE, &EVIDENCE_URL],
    ).await?;
    Ok(())
}
