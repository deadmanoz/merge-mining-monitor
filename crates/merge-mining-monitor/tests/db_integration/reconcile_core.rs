use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::capture::ParentKind;
use mmm_read_model::{
    ReconcileReadModelConfig, reconcile_from_merge_mining_event, restore_merge_mining_event,
    revoke_merge_mining_event, run_reconcile_read_model,
};
use serde_json::json;
use tokio_postgres::Client;

use crate::support::scenario::{stale_verdict, stale_verdict_with_competitor_header};
use crate::support::seed::block_kind;
use crate::support::{NamecoinEventFixture, canonical_parent_classification, classified_proof};

async fn assert_first_reconcile_read_model(
    client: &Client,
    parent_hash: &[u8],
    source_id: i64,
    event_id: i64,
    height: i32,
) -> Result<()> {
    let block = client
        .query_one(
            "SELECT kind, btc_height, btc_height_source, total_attestations, \
                    distinct_sources, auxpow_chain_count, core_attested, live_observed, \
                    first_attested_at, last_attested_at \
             FROM block WHERE btc_header_hash = $1",
            &[&parent_hash],
        )
        .await?;
    assert_eq!(block.get::<_, String>(0), "canonical");
    assert_eq!(block.get::<_, Option<i32>>(1), Some(height));
    assert_eq!(
        block.get::<_, Option<String>>(2).as_deref(),
        Some("bitcoin-core")
    );
    assert_eq!(block.get::<_, i32>(3), 1);
    assert_eq!(block.get::<_, i32>(4), 2);
    assert_eq!(block.get::<_, i32>(5), 1);
    assert!(block.get::<_, bool>(6));
    assert!(block.get::<_, bool>(7));
    assert_eq!(block.get::<_, Option<i64>>(8), Some(123));
    assert_eq!(block.get::<_, Option<i64>>(9), Some(123));

    let proof = client
        .query_one(
            "SELECT evidence, pow_validated, confirmed_at, revoked_at \
             FROM attestation_proof \
             WHERE btc_header_hash = $1 AND source_id = $2",
            &[&parent_hash, &source_id],
        )
        .await?;
    let evidence: serde_json::Value = proof.get(0);
    assert_eq!(evidence, json!({ "contributing_event_ids": [event_id] }));
    assert!(proof.get::<_, bool>(1));
    assert_eq!(proof.get::<_, i64>(2), 123);
    assert_eq!(proof.get::<_, Option<i64>>(3), None);

    Ok(())
}

async fn assert_replay_preserves_then_revoke_demotes(
    client: &mut Client,
    event_id: i64,
    parent_hash: &[u8],
    height: i32,
) -> Result<()> {
    reconcile_from_merge_mining_event(
        client,
        event_id,
        &ConfiguredParentClassifier::Disabled,
        None,
    )
    .await?;
    let preserved = client
        .query_one(
            "SELECT kind, btc_height, core_attested, live_observed \
             FROM block WHERE btc_header_hash = $1",
            &[&parent_hash],
        )
        .await?;
    assert_eq!(preserved.get::<_, String>(0), "canonical");
    assert_eq!(preserved.get::<_, Option<i32>>(1), Some(height));
    assert!(preserved.get::<_, bool>(2));
    assert!(preserved.get::<_, bool>(3));

    revoke_merge_mining_event(
        client,
        event_id,
        "bad source evidence",
        &ConfiguredParentClassifier::Disabled,
    )
    .await?;
    let demoted = client
        .query_one(
            "SELECT kind, btc_height, total_attestations, distinct_sources, \
                    auxpow_chain_count, bitcoin_miner_pool_id, first_attested_at, last_attested_at \
             FROM block WHERE btc_header_hash = $1",
            &[&parent_hash],
        )
        .await?;
    assert_eq!(demoted.get::<_, String>(0), "canonical");
    assert_eq!(demoted.get::<_, Option<i32>>(1), Some(height));
    assert_eq!(demoted.get::<_, i32>(2), 0);
    assert_eq!(demoted.get::<_, i32>(3), 1);
    assert_eq!(demoted.get::<_, i32>(4), 0);
    assert_eq!(demoted.get::<_, Option<i64>>(5), None);
    assert_eq!(demoted.get::<_, Option<i64>>(6), None);
    assert_eq!(demoted.get::<_, Option<i64>>(7), None);
    Ok(())
}

async fn assert_revocation_sticky_then_restore(
    client: &mut Client,
    event_id: i64,
    parent_hash: &[u8],
) -> Result<()> {
    let revoked_proof = client
        .query_one(
            "SELECT revoked_at, revocation_reason FROM attestation_proof WHERE btc_header_hash = $1",
            &[&parent_hash],
        )
        .await?;
    assert!(revoked_proof.get::<_, Option<i64>>(0).is_some());
    assert_eq!(
        revoked_proof.get::<_, Option<String>>(1).as_deref(),
        Some("bad source evidence")
    );
    revoke_merge_mining_event(
        client,
        event_id,
        "second revoke should not overwrite",
        &ConfiguredParentClassifier::Disabled,
    )
    .await?;
    let preserved_revocation = client
        .query_one(
            "SELECT revocation_reason FROM attestation_proof WHERE btc_header_hash = $1",
            &[&parent_hash],
        )
        .await?;
    assert_eq!(
        preserved_revocation.get::<_, Option<String>>(0).as_deref(),
        Some("bad source evidence")
    );
    let repaired_after_revoke = run_reconcile_read_model(
        client,
        &ConfiguredParentClassifier::Disabled,
        ReconcileReadModelConfig::default(),
    )
    .await?;
    assert_eq!(repaired_after_revoke, 0);

    restore_merge_mining_event(client, event_id, &ConfiguredParentClassifier::Disabled).await?;
    restore_merge_mining_event(client, event_id, &ConfiguredParentClassifier::Disabled).await?;
    let restored = client
        .query_one(
            "SELECT total_attestations, distinct_sources, auxpow_chain_count, \
                    first_attested_at, last_attested_at \
             FROM block WHERE btc_header_hash = $1",
            &[&parent_hash],
        )
        .await?;
    assert_eq!(restored.get::<_, i32>(0), 1);
    assert_eq!(restored.get::<_, i32>(1), 2);
    assert_eq!(restored.get::<_, i32>(2), 1);
    assert_eq!(restored.get::<_, Option<i64>>(3), Some(123));
    assert_eq!(restored.get::<_, Option<i64>>(4), Some(123));
    Ok(())
}

async fn derivable_competition_exists(client: &Client, stale_hash: &[u8]) -> Result<bool> {
    Ok(client
        .query_one(
            "SELECT EXISTS ( \
                SELECT 1 \
                FROM block stale \
                JOIN block canonical \
                  ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                WHERE stale.btc_header_hash = $1 \
                  AND stale.kind = 'stale' \
                  AND canonical.kind = 'canonical' \
            )",
            &[&stale_hash],
        )
        .await?
        .get(0))
}

#[tokio::test]
async fn reconciles_classified_namecoin_parent_read_model() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let height = 700_000;
        let event = fixture
            .insert_event(
                &client,
                500_000,
                classified_proof(ParentKind::Canonical, height),
                123,
            )
            .await?;
        let classification = canonical_parent_classification(&event.header, height, true);

        let stats = reconcile_from_merge_mining_event(
            &mut client,
            event.id,
            &ConfiguredParentClassifier::Disabled,
            Some(classification),
        )
        .await?;
        assert_eq!(stats.parents_reconciled, 1);

        assert_first_reconcile_read_model(
            &client,
            &event.parent_hash,
            fixture.source_id,
            event.id,
            height,
        )
        .await?;
        assert_replay_preserves_then_revoke_demotes(
            &mut client,
            event.id,
            &event.parent_hash,
            height,
        )
        .await?;
        assert_revocation_sticky_then_restore(&mut client, event.id, &event.parent_hash).await?;

        Ok::<_, anyhow::Error>(())
    })
}

async fn assert_stale_competitor_read_model(
    client: &Client,
    stale_hash: &[u8],
    competitor_hash: Vec<u8>,
    height: i32,
) -> Result<()> {
    let stale = client
        .query_one(
            "SELECT kind, btc_height, canonical_competitor_hash \
             FROM block WHERE btc_header_hash = $1",
            &[&stale_hash],
        )
        .await?;
    assert_eq!(stale.get::<_, String>(0), "stale");
    assert_eq!(stale.get::<_, Option<i32>>(1), Some(height));
    assert_eq!(
        stale.get::<_, Option<Vec<u8>>>(2),
        Some(competitor_hash.clone())
    );

    let canonical_kind = block_kind(client, &competitor_hash).await?;
    assert_eq!(canonical_kind, "canonical");
    assert!(derivable_competition_exists(client, stale_hash).await?);
    Ok(())
}

async fn assert_revoked_stale_keeps_competitor(
    client: &mut Client,
    event_id: i64,
    stale_hash: &[u8],
    competitor_hash: Vec<u8>,
) -> Result<()> {
    revoke_merge_mining_event(
        client,
        event_id,
        "bad stale evidence",
        &ConfiguredParentClassifier::Disabled,
    )
    .await?;
    let stale_after_revoke = client
        .query_one(
            "SELECT kind, canonical_competitor_hash FROM block WHERE btc_header_hash = $1",
            &[&stale_hash],
        )
        .await?;
    assert_eq!(stale_after_revoke.get::<_, String>(0), "stale");
    assert_eq!(
        stale_after_revoke.get::<_, Option<Vec<u8>>>(1),
        Some(competitor_hash.clone())
    );
    assert!(derivable_competition_exists(client, stale_hash).await?);
    Ok(())
}

#[tokio::test]
async fn reconciles_stale_parent_competitor_read_model() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let height = 700_001;
        let event = fixture
            .insert_event(
                &client,
                500_000,
                classified_proof(ParentKind::Stale, height),
                456,
            )
            .await?;
        let stale_hash = event.parent_hash.clone();
        // This synthetic competitor is injected as already classified Core
        // evidence; the test exercises read-model SQL plumbing, not PoW.
        let mut competitor_header = event.header;
        competitor_header.nonce = competitor_header.nonce.wrapping_add(1);
        let competitor_hash = competitor_header.block_hash().to_byte_array().to_vec();
        assert_ne!(stale_hash, competitor_hash);

        let classification = stale_verdict_with_competitor_header(
            &event.header,
            height,
            competitor_header,
            competitor_hash.clone(),
        );

        reconcile_from_merge_mining_event(
            &mut client,
            event.id,
            &ConfiguredParentClassifier::Disabled,
            Some(classification),
        )
        .await?;

        assert_stale_competitor_read_model(&client, &stale_hash, competitor_hash.clone(), height)
            .await?;

        assert_revoked_stale_keeps_competitor(&mut client, event.id, &stale_hash, competitor_hash)
            .await?;

        Ok::<_, anyhow::Error>(())
    })
}

async fn reconcile_first_stale_with_synthesized_competitor(
    client: &mut Client,
    first_event_id: i64,
    header: Header,
    height: i32,
) -> Result<()> {
    let mut first_competitor_header = header;
    first_competitor_header.nonce = first_competitor_header.nonce.wrapping_add(1);
    let first_competitor_hash = first_competitor_header
        .block_hash()
        .to_byte_array()
        .to_vec();
    reconcile_from_merge_mining_event(
        client,
        first_event_id,
        &ConfiguredParentClassifier::Disabled,
        Some(stale_verdict_with_competitor_header(
            &header,
            height,
            first_competitor_header,
            first_competitor_hash,
        )),
    )
    .await?;
    Ok(())
}

async fn seed_and_reconcile_modified_stale<F>(
    client: &mut Client,
    fixture: &NamecoinEventFixture,
    height: i32,
    seed: (u32, u8, i64),
    classify: F,
) -> Result<Vec<u8>>
where
    F: FnOnce(&Header, Vec<u8>) -> ParentClassification,
{
    let (nonce_bump, child_hash_fill, observed_at) = seed;
    let mut header = fixture.parsed.parent_header.header;
    header.nonce = header.nonce.wrapping_add(nonce_bump);
    let event = fixture
        .insert_event_with_header(
            client,
            500_001,
            child_hash_fill,
            header,
            classified_proof(ParentKind::Stale, height),
            observed_at,
        )
        .await?;

    reconcile_from_merge_mining_event(
        client,
        event.id,
        &ConfiguredParentClassifier::Disabled,
        Some(classify(&event.header, event.parent_hash.clone())),
    )
    .await?;
    Ok(event.parent_hash)
}

#[tokio::test]
async fn synthesized_canonical_promotion_clears_stale_competitor() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let height = 700_010;

        let first = fixture
            .insert_event(
                &client,
                500_000,
                classified_proof(ParentKind::Stale, height),
                810,
            )
            .await?;
        let stale_hash = first.parent_hash.clone();

        reconcile_first_stale_with_synthesized_competitor(
            &mut client,
            first.id,
            first.header,
            height,
        )
        .await?;
        assert!(derivable_competition_exists(&client, &stale_hash).await?);

        let competitor_header = first.header;
        seed_and_reconcile_modified_stale(
            &mut client,
            &fixture,
            height,
            (7, 0x81, 811),
            |second_header, _| {
                stale_verdict_with_competitor_header(
                    second_header,
                    height,
                    competitor_header,
                    stale_hash.clone(),
                )
            },
        )
        .await?;

        let promoted_kind = block_kind(&client, &stale_hash).await?;
        assert_eq!(promoted_kind, "canonical");
        assert!(!derivable_competition_exists(&client, &stale_hash).await?);

        Ok::<_, anyhow::Error>(())
    })
}

async fn assert_revoked_competitor_demoted_both(
    client: &Client,
    canonical_hash: &[u8],
    stale_hash: &[u8],
) -> Result<()> {
    let canonical_kind = block_kind(client, canonical_hash).await?;
    assert_eq!(canonical_kind, "unknown");

    let stale_kind = block_kind(client, stale_hash).await?;
    assert_eq!(stale_kind, "unknown");

    assert!(!derivable_competition_exists(client, stale_hash).await?);
    Ok(())
}

#[tokio::test]
async fn revoking_canonical_competitor_demotes_stale_dependent() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let height = 700_003;
        let canonical = fixture
            .insert_event(
                &client,
                500_000,
                classified_proof(ParentKind::Canonical, height),
                900,
            )
            .await?;
        let canonical_hash = canonical.parent_hash.clone();
        let canonical_classification =
            canonical_parent_classification(&canonical.header, height, false);
        reconcile_from_merge_mining_event(
            &mut client,
            canonical.id,
            &ConfiguredParentClassifier::Disabled,
            Some(canonical_classification),
        )
        .await?;

        let stale_hash = seed_and_reconcile_modified_stale(
            &mut client,
            &fixture,
            height,
            (11, 0x44, 901),
            |stale_header, _| ParentClassification {
                live_observed: false,
                core_attested: false,
                ..stale_verdict(stale_header, height, canonical_hash.clone())
            },
        )
        .await?;

        revoke_merge_mining_event(
            &mut client,
            canonical.id,
            "canonical competitor revoked",
            &ConfiguredParentClassifier::Disabled,
        )
        .await?;

        assert_revoked_competitor_demoted_both(&client, &canonical_hash, &stale_hash).await?;

        Ok::<_, anyhow::Error>(())
    })
}
