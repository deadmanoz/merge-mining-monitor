//! The body-invalid stale annotation is display-only: an annotated block keeps
//! `kind='stale'` and its ordinary competition semantics, while the block
//! detail gains a `body_invalid` object and the tree node an optional
//! `body_invalid_rule`. These tests pin annotate-not-promote end to end.

use anyhow::Result;
use mmm_api::projection::{self};
use mmm_store::upsert_body_invalid_stale;
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
    upsert_body_invalid_stale(
        client,
        &annotated,
        Some(300),
        RULE,
        Some(EVIDENCE_URL),
        "test-mirror@aaaa",
        ts,
    )
    .await?;
    Ok((annotated, plain))
}

#[tokio::test]
async fn block_projects_body_invalid_annotation_without_promoting_kind() -> Result<()> {
    crate::run_db_test!(client, {
        let (annotated, plain) = seed_competing_stales(&client).await?;

        let payload = projection::block(&client, &display_hash(&annotated))
            .await
            .map_err(format_projection_error)?;
        assert_eq!(payload.block.kind, "stale");
        assert!(payload.block.error_block_reason.is_none());
        let body_invalid = payload
            .block
            .body_invalid
            .as_ref()
            .expect("annotated stale carries body_invalid");
        assert_eq!(body_invalid.rule, RULE);
        assert_eq!(body_invalid.evidence_url.as_deref(), Some(EVIDENCE_URL));
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
async fn tree_nodes_carry_the_annotation_rule_only_when_present() -> Result<()> {
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
        upsert_body_invalid_stale(
            &client,
            &annotated,
            Some(61),
            RULE,
            Some(EVIDENCE_URL),
            "test-mirror@aaaa",
            ts,
        )
        .await?;
        let annotated_hash = display_hash(&annotated);
        let plain_hash = display_hash(&plain);

        let tree = project_tree(&client, Some("from_height=55&to_height=65")).await?;
        let annotated_node = tree
            .nodes
            .iter()
            .find(|node| node.hash == annotated_hash)
            .expect("annotated stale in tree window");
        assert_eq!(annotated_node.kind, "stale");
        assert_eq!(annotated_node.body_invalid_rule.as_deref(), Some(RULE));
        let plain_node = tree
            .nodes
            .iter()
            .find(|node| node.hash == plain_hash)
            .expect("plain stale in tree window");
        assert!(plain_node.body_invalid_rule.is_none());
        Ok(())
    })
}

#[tokio::test]
async fn annotation_on_non_stale_block_never_surfaces() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 12);
        let canonical = hash_bytes(0x7c01);
        insert_block(
            &client,
            &canonical,
            &hash_bytes(0x7c00),
            Some(310),
            "canonical",
            ts,
            None,
        )
        .await?;
        // A mirror row for a non-stale hash must stay invisible: the join is
        // gated on kind = 'stale', so display never contradicts the kind.
        upsert_body_invalid_stale(
            &client,
            &canonical,
            Some(310),
            RULE,
            Some(EVIDENCE_URL),
            "test-mirror@aaaa",
            ts,
        )
        .await?;
        let payload = projection::block(&client, &display_hash(&canonical))
            .await
            .map_err(format_projection_error)?;
        assert_eq!(payload.block.kind, "canonical");
        assert!(payload.block.body_invalid.is_none());
        Ok(())
    })
}

#[tokio::test]
async fn import_prunes_annotations_withdrawn_by_the_new_mirror() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (annotated, plain) = seed_competing_stales(&client).await?;
        // Second annotation, soon to be withdrawn by the next pin.
        upsert_body_invalid_stale(
            &client,
            &plain,
            Some(300),
            RULE,
            Some(EVIDENCE_URL),
            "test-mirror@aaaa",
            day_epoch(2026, Month::May, 12),
        )
        .await?;

        let mirror =
            std::env::temp_dir().join(format!("body-invalid-mirror-{}.csv", std::process::id()));
        std::fs::write(
            &mirror,
            format!(
                "# Source commit: {}\nheight,hash,rule,evidence_url\n300,{},{},{}\n",
                "b".repeat(40),
                display_hash(&annotated),
                RULE,
                EVIDENCE_URL
            ),
        )?;
        let config = mmm_producers::BodyInvalidImportConfig {
            csv_path: mirror.clone(),
            source_label: "test-mirror@bbbb".to_string(),
        };
        let summary = mmm_producers::run_import_body_invalid_stales(&mut client, &config).await?;
        std::fs::remove_file(&mirror).ok();
        assert_eq!(summary.rows_seen, 1);
        assert_eq!(summary.updated, 1);
        assert_eq!(summary.removed, 1, "withdrawn annotation must be pruned");

        let withdrawn = projection::block(&client, &display_hash(&plain))
            .await
            .map_err(format_projection_error)?;
        assert!(withdrawn.block.body_invalid.is_none());
        let kept = projection::block(&client, &display_hash(&annotated))
            .await
            .map_err(format_projection_error)?;
        assert!(kept.block.body_invalid.is_some());
        Ok(())
    })
}

#[tokio::test]
async fn reimport_replaces_the_annotation_in_place() -> Result<()> {
    crate::run_db_test!(client, {
        let (annotated, _plain) = seed_competing_stales(&client).await?;

        let inserted = upsert_body_invalid_stale(
            &client,
            &annotated,
            Some(300),
            RULE,
            Some("https://example.com/corrected"),
            "test-mirror@bbbb",
            day_epoch(2026, Month::May, 13),
        )
        .await?;
        assert!(!inserted, "conflicting hash must update, not insert");

        let payload = projection::block(&client, &display_hash(&annotated))
            .await
            .map_err(format_projection_error)?;
        let body_invalid = payload.block.body_invalid.expect("annotation retained");
        assert_eq!(
            body_invalid.evidence_url.as_deref(),
            Some("https://example.com/corrected"),
            "newest pin wins on re-import"
        );
        Ok(())
    })
}
