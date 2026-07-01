use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, FakeParentClassifier, FakeParentClassifierGate,
    ParentClassification,
};
use mmm_capture::capture::{ClassificationProof, ParentKind};
use mmm_pg::{PgConfig, connect};
use mmm_read_model::{
    ReclassifyUnknownParentsConfig, ReconcileReadModelConfig, reconcile_from_merge_mining_event,
    revoke_merge_mining_event, run_reclassify_unknown_parents, run_reconcile_read_model,
};
use std::time::Duration;
use tokio_postgres::Client;

use crate::support::scenario::stale_verdict_with_competitor_header;
use crate::support::seed::block_kind;
use crate::support::{NamecoinEventFixture, canonical_parent_classification, classified_proof};

async fn event_parent_kind(client: &Client, event_id: i64) -> Result<String> {
    Ok(client
        .query_one(
            "SELECT btc_parent_kind FROM merge_mining_event WHERE id = $1",
            &[&event_id],
        )
        .await?
        .get(0))
}

#[tokio::test]
async fn enabled_unknown_classifier_preserves_canonical_event_when_block_missing() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let event = fixture
            .insert_event(
                &client,
                500_000,
                classified_proof(ParentKind::Canonical, 700_002),
                790,
            )
            .await?;
        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            ParentClassification::unknown(&event.header),
        ));

        let repaired = run_reconcile_read_model(
            &mut client,
            &classifier,
            ReconcileReadModelConfig {
                batch_size: 1,
                max_iterations: 2,
                ..ReconcileReadModelConfig::default()
            },
        )
        .await?;
        assert_eq!(repaired, 1);

        let event_row = client
            .query_one(
                "SELECT btc_parent_kind, btc_parent_height \
                 FROM merge_mining_event WHERE id = $1",
                &[&event.id],
            )
            .await?;
        assert_eq!(event_row.get::<_, String>(0), "canonical");
        assert_eq!(event_row.get::<_, Option<i32>>(1), Some(700_002));

        let block = client
            .query_one(
                "SELECT kind, btc_height, core_attested \
                 FROM block WHERE btc_header_hash = $1",
                &[&event.parent_hash],
            )
            .await?;
        assert_eq!(block.get::<_, String>(0), "canonical");
        assert_eq!(block.get::<_, Option<i32>>(1), Some(700_002));
        assert!(!block.get::<_, bool>(2));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn revoke_cascades_to_child_event_read_model() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let parent_height = 710_000;
        let parent = fixture
            .insert_event(
                &client,
                500_000,
                classified_proof(ParentKind::Canonical, parent_height),
                1_000,
            )
            .await?;
        let parent_classification =
            canonical_parent_classification(&parent.header, parent_height, false);
        reconcile_from_merge_mining_event(
            &mut client,
            parent.id,
            &ConfiguredParentClassifier::Disabled,
            Some(parent_classification),
        )
        .await?;

        let mut child_parent_header = parent.header;
        child_parent_header.prev_blockhash = parent.header.block_hash();
        child_parent_header.nonce = child_parent_header.nonce.wrapping_add(2);
        let child = fixture
            .insert_event_with_header(
                &client,
                500_001,
                0x42,
                child_parent_header,
                ClassificationProof::default(),
                1_001,
            )
            .await?;

        revoke_merge_mining_event(
            &mut client,
            parent.id,
            "parent revoked",
            &ConfiguredParentClassifier::Disabled,
        )
        .await?;

        let parent_kind = block_kind(&client, &parent.parent_hash).await?;
        assert_eq!(parent_kind, "unknown");

        let child_kind = block_kind(&client, &child.parent_hash).await?;
        assert_eq!(child_kind, "unknown");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_unknown_parents_pages_past_stuck_headers() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let first = fixture
            .insert_event(&client, 500_000, ClassificationProof::default(), 2_100)
            .await?;
        let first_header = first.header;

        let mut second_header = first_header;
        second_header.nonce = second_header.nonce.wrapping_add(88);
        let second = fixture
            .insert_event_with_header(
                &client,
                500_001,
                0x88,
                second_header,
                ClassificationProof::default(),
                2_101,
            )
            .await?;

        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
            ParentClassification::unknown(&first_header),
            canonical_parent_classification(&second.header, 720_100, true),
        ]));

        let changed = run_reclassify_unknown_parents(
            &mut client,
            &classifier,
            ReclassifyUnknownParentsConfig {
                batch_size: 1,
                recheck_orphans: false,
            },
        )
        .await?;
        assert_eq!(changed, 1);

        let first_kind = event_parent_kind(&client, first.id).await?;
        let second_kind = event_parent_kind(&client, second.id).await?;
        assert_eq!(first_kind, "unknown");
        assert_eq!(second_kind, "canonical");

        let row = client
            .query_one(
                "SELECT kind, btc_height FROM block WHERE btc_header_hash = $1",
                &[&second.parent_hash],
            )
            .await?;
        assert_eq!(row.get::<_, String>(0), "canonical");
        assert_eq!(row.get::<_, Option<i32>>(1), Some(720_100));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reconcile_all_pages_beyond_first_batch() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let first = fixture
            .insert_event(&client, 500_000, ClassificationProof::default(), 2_200)
            .await?;
        let first_header = first.header;
        let first_hash = first_header.block_hash().to_byte_array().to_vec();

        let mut second_header = first_header;
        second_header.nonce = second_header.nonce.wrapping_add(99);
        let second = fixture
            .insert_event_with_header(
                &client,
                500_001,
                0x99,
                second_header,
                ClassificationProof::default(),
                2_201,
            )
            .await?;

        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
            canonical_parent_classification(&first_header, 721_000, true),
            canonical_parent_classification(&second.header, 721_001, true),
        ]));
        let repaired = run_reconcile_read_model(
            &mut client,
            &classifier,
            ReconcileReadModelConfig {
                missing_only: false,
                batch_size: 1,
                ..ReconcileReadModelConfig::default()
            },
        )
        .await?;
        assert_eq!(repaired, 2);

        let count: i64 = client
            .query_one(
                "SELECT COUNT(*)::bigint FROM block \
                 WHERE btc_header_hash IN ($1, $2) AND kind = 'canonical'",
                &[&first_hash, &second.parent_hash],
            )
            .await?
            .get(0);
        assert_eq!(count, 2);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn missing_only_repairs_block_classification_drift() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let event = fixture
            .insert_event(&client, 500_000, ClassificationProof::default(), 2_300)
            .await?;
        let classification = canonical_parent_classification(&event.header, 722_000, true);
        let classifier =
            ConfiguredParentClassifier::Fake(FakeParentClassifier::new(classification.clone()));
        reconcile_from_merge_mining_event(&mut client, event.id, &classifier, None).await?;

        client
            .execute(
                "UPDATE block \
                 SET kind = 'unknown', btc_height = NULL, btc_height_source = NULL, \
                     bitcoin_miner_pool_id = NULL, difficulty_epoch_ok = NULL \
                 WHERE btc_header_hash = $1",
                &[&event.parent_hash],
            )
            .await?;

        let repaired = run_reconcile_read_model(
            &mut client,
            &classifier,
            ReconcileReadModelConfig::default(),
        )
        .await?;
        assert_eq!(repaired, 1);

        let row = client
            .query_one(
                "SELECT kind, btc_height, difficulty_epoch_ok \
                 FROM block WHERE btc_header_hash = $1",
                &[&event.parent_hash],
            )
            .await?;
        assert_eq!(row.get::<_, String>(0), "canonical");
        assert_eq!(row.get::<_, Option<i32>>(1), Some(722_000));
        assert_eq!(row.get::<_, Option<bool>>(2), Some(true));

        Ok::<_, anyhow::Error>(())
    })
}

fn build_retry_stale_classification(changed_header: Header) -> (ParentClassification, Vec<u8>) {
    let mut competitor_header = changed_header;
    competitor_header.nonce = competitor_header.nonce.wrapping_add(1);
    let competitor_hash = competitor_header.block_hash().to_byte_array().to_vec();
    let stale_classification = stale_verdict_with_competitor_header(
        &changed_header,
        720_000,
        competitor_header,
        competitor_hash.clone(),
    );
    (stale_classification, competitor_hash)
}

async fn assert_retry_reconciled_stale_read_model(
    client: &Client,
    event_id: i64,
    changed_hash: &[u8],
    competitor_hash: Vec<u8>,
) -> Result<()> {
    let event_kind = event_parent_kind(client, event_id).await?;
    assert_eq!(event_kind, "stale");

    let stale_row = client
        .query_one(
            "SELECT kind, canonical_competitor_hash \
             FROM block \
             WHERE btc_header_hash = $1",
            &[&changed_hash],
        )
        .await?;
    assert_eq!(stale_row.get::<_, String>(0), "stale");
    assert_eq!(
        stale_row.get::<_, Option<Vec<u8>>>(1),
        Some(competitor_hash.clone())
    );

    let competitor_kind = block_kind(client, &competitor_hash).await?;
    assert_eq!(competitor_kind, "canonical");
    Ok(())
}

#[tokio::test]
async fn retries_reconcile_when_event_change_expands_lock_set() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let event = fixture
            .insert_event(&client, 500_000, ClassificationProof::default(), 3_000)
            .await?;
        let event_id = event.id;
        let initial_header = event.header;
        let mut changed_header = initial_header;
        changed_header.nonce = changed_header.nonce.wrapping_add(101);
        let changed_hash = changed_header.block_hash().to_byte_array().to_vec();
        let changed_prev = changed_header.prev_blockhash.to_byte_array().to_vec();

        let (stale_classification, competitor_hash) =
            build_retry_stale_classification(changed_header);
        let gate = FakeParentClassifierGate::new();
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new_sequence([
                ParentClassification::unknown(&initial_header),
                stale_classification.clone(),
                stale_classification,
            ])
            .with_first_call_gate(gate.clone()),
        );

        let mut task_client = connect(&PgConfig::from_env()?).await?;
        task_client
            .batch_execute(&format!("SET search_path TO {schema}, public;"))
            .await?;
        let task_classifier = classifier.clone();
        let reconcile_task = tokio::spawn(async move {
            reconcile_from_merge_mining_event(&mut task_client, event_id, &task_classifier, None)
                .await
        });

        tokio::time::timeout(Duration::from_secs(5), gate.wait_started())
            .await
            .context("fake classifier did not enter first call")?;
        let changed_header_bytes = serialize(&changed_header);
        client
            .execute(
                "UPDATE merge_mining_event \
                 SET btc_parent_header_hash = $2, \
                     btc_parent_prev_header_hash = $3, \
                     btc_parent_header_bytes = $4, \
                     btc_parent_header_time = $5 \
                 WHERE id = $1",
                &[
                    &event_id,
                    &changed_hash,
                    &changed_prev,
                    &changed_header_bytes,
                    &(changed_header.time as i64),
                ],
            )
            .await?;
        gate.proceed();

        let stats = reconcile_task
            .await
            .context("join reconcile retry task")??;
        assert_eq!(stats.parents_reconciled, 2);
        assert_eq!(stats.descendants_reconciled, 1);

        assert_retry_reconciled_stale_read_model(&client, event_id, &changed_hash, competitor_hash)
            .await?;

        Ok::<_, anyhow::Error>(())
    })
}
