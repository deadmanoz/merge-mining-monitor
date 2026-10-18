use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::capture::ClassificationProof;
use mmm_read_model::{
    ReclassifyUnknownParentsConfig, reconcile_from_merge_mining_event,
    run_reclassify_unknown_parents, run_scheduled_recheck, run_scheduled_recheck_pages_for_test,
};
use mmm_store::{
    RecheckScope, acknowledge_recheck_pass, bind_recheck_pass, load_scheduled_recheck,
    schedule_core_recheck, upsert_merge_mining_event,
};
use tokio_postgres::Client;

use crate::support::scenario::{
    canonical_verdict, orphan_candidate_verdict, stale_verdict_with_competitor_header,
    unknown_verdict,
};
use crate::support::{
    InsertedNamecoinEvent, NamecoinEventFixture, exact_observation, namecoin_event_payload,
    namecoin_fixture,
};

struct UnknownParentFixture {
    event_id: i64,
    parent_hash: Vec<u8>,
    header: Header,
}

#[tokio::test]
async fn core_absence_sets_orphan_class_and_canonical_promotion_clears_it() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = insert_unknown_parent_event(&client).await?;

        reconcile_from_merge_mining_event(
            &mut client,
            fixture.event_id,
            &ConfiguredParentClassifier::Disabled,
            None,
        )
        .await?;

        let pending = client
            .query_one(
                "SELECT kind, btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&fixture.parent_hash],
            )
            .await?;
        assert_eq!(pending.get::<_, String>(0), "unknown");
        assert_eq!(
            pending.get::<_, Option<String>>(1),
            None,
            "Core-disabled unknown must stay pending (NULL), never promoted"
        );

        reconcile_from_merge_mining_event(
            &mut client,
            fixture.event_id,
            &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(orphan_candidate_verdict(
                &fixture.header,
            ))),
            None,
        )
        .await?;
        let pre = client
            .query_one(
                "SELECT kind, btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&fixture.parent_hash],
            )
            .await?;
        assert_eq!(pre.get::<_, String>(0), "unknown");
        let orphan_class: Option<String> = pre.get(1);
        assert!(
            matches!(
                orphan_class.as_deref(),
                Some("strict_btc_orphan" | "weak_btc_orphan" | "excluded")
            ),
            "Core-attested-absent unknown must be classified, got {orphan_class:?}"
        );

        reconcile_from_merge_mining_event(
            &mut client,
            fixture.event_id,
            &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(canonical_verdict(
                &fixture.header,
                700_000,
            ))),
            None,
        )
        .await?;

        let row = client
            .query_one(
                "SELECT kind, btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&fixture.parent_hash],
            )
            .await?;
        assert_eq!(row.get::<_, String>(0), "canonical");
        assert_eq!(
            row.get::<_, Option<String>>(1),
            None,
            "canonical promotion must clear btc_orphan_class"
        );
        Ok::<_, anyhow::Error>(())
    })
}

async fn assert_recheck_corrects_stale_class(
    client: &mut Client,
    absent_classifier: &ConfiguredParentClassifier,
    parent_hash: Vec<u8>,
    class_after_first: String,
) -> Result<()> {
    let stale_class = if class_after_first == "weak_btc_orphan" {
        "strict_btc_orphan"
    } else {
        "weak_btc_orphan"
    };
    client
        .execute(
            "UPDATE block SET btc_orphan_class = $2 WHERE btc_header_hash = $1",
            &[&parent_hash, &stale_class],
        )
        .await?;
    let recheck_changed = run_reclassify_unknown_parents(
        client,
        absent_classifier,
        ReclassifyUnknownParentsConfig {
            batch_size: 10,
            recheck_orphans: true,
            scheduled: false,
        },
    )
    .await?;
    assert_eq!(
        recheck_changed, 1,
        "a recheck that corrects a different prior class counts as progress"
    );
    let class_after_recheck: Option<String> = client
        .query_one(
            "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
            &[&parent_hash],
        )
        .await?
        .get(0);
    assert_eq!(
        class_after_recheck.as_deref(),
        Some(class_after_first.as_str()),
        "recheck restored the classifier's verdict"
    );
    Ok(())
}

#[tokio::test]
async fn reclassify_skips_classified_orphans_unless_recheck() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = insert_unknown_parent_event(&client).await?;
        let absent_classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&fixture.header),
        ));

        let first = run_reclassify_unknown_parents(
            &mut client,
            &absent_classifier,
            ReclassifyUnknownParentsConfig {
                batch_size: 10,
                recheck_orphans: false,
                scheduled: false,
            },
        )
        .await?;
        assert_eq!(first, 1, "first pass classifies the orphan");
        let class_after_first: Option<String> = client
            .query_one(
                "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&fixture.parent_hash],
            )
            .await?
            .get(0);
        let class_after_first = class_after_first.expect("first pass set an orphan class");

        let second = run_reclassify_unknown_parents(
            &mut client,
            &absent_classifier,
            ReclassifyUnknownParentsConfig {
                batch_size: 10,
                recheck_orphans: false,
                scheduled: false,
            },
        )
        .await?;
        assert_eq!(
            second, 0,
            "default pass skips already orphan-classified parents"
        );

        let recheck_unchanged = run_reclassify_unknown_parents(
            &mut client,
            &absent_classifier,
            ReclassifyUnknownParentsConfig {
                batch_size: 10,
                recheck_orphans: true,
                scheduled: false,
            },
        )
        .await?;
        assert_eq!(
            recheck_unchanged, 0,
            "a recheck that leaves the class unchanged is not counted as progress"
        );

        assert_recheck_corrects_stale_class(
            &mut client,
            &absent_classifier,
            fixture.parent_hash,
            class_after_first,
        )
        .await?;
        Ok::<_, anyhow::Error>(())
    })
}

async fn insert_unknown_parent_event(client: &Client) -> Result<UnknownParentFixture> {
    let (resolver, pool_ids_by_slug, source_id, parsed) = namecoin_fixture(client).await?;
    let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
        .context("Namecoin fixture must carry a BIP34 parent height")?;
    crate::support::db::seed_synthetic_bitcoin_history(
        client,
        parent_height,
        i64::from(parsed.parent_header.header.time),
        parsed.parent_header.header.bits.to_consensus(),
    )
    .await?;
    let payload = namecoin_event_payload(
        &parsed,
        &resolver,
        &pool_ids_by_slug,
        500_000,
        ClassificationProof::default(),
        1_000,
    )?;
    let event_id = upsert_merge_mining_event(client, source_id, &payload)
        .await?
        .event_id;
    Ok(UnknownParentFixture {
        event_id,
        parent_hash: parsed.parent_header.hash().to_byte_array().to_vec(),
        header: parsed.parent_header.header,
    })
}

/// Seeds two target-validating unknown parents straddling the cache time horizon
/// (the second without a coinbase height, so its orphan class stays pending).
async fn insert_two_unknown_parents(client: &Client) -> Result<()> {
    let (_, _, source_id, parsed) = namecoin_fixture(client).await?;
    let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
        .context("Namecoin fixture must carry a BIP34 parent height")?;
    crate::support::db::seed_synthetic_bitcoin_history(
        client,
        parent_height + 2_016,
        i64::from(parsed.parent_header.header.time) + 1,
        parsed.parent_header.header.bits.to_consensus(),
    )
    .await?;
    for (fixture, height, byte) in [
        ("500000-valid-parent", 500_000, 0xa1),
        ("500002-wrong-chain-parent", 500_002, 0xa2),
    ] {
        let mut payload = exact_observation(fixture, height, [byte; 32], 1_000)?;
        if height == 500_002 {
            payload.btc_parent_coinbase_script = None;
        }
        upsert_merge_mining_event(client, source_id, &payload).await?;
    }
    Ok(())
}

async fn two_unknown_parents_and_a_classifier(
    client: &Client,
) -> Result<(FakeParentClassifier, ConfiguredParentClassifier)> {
    insert_two_unknown_parents(client).await?;
    let parsed = crate::support::parse_auxpow_fixture("500000-valid-parent")?;
    let fake = FakeParentClassifier::new(orphan_candidate_verdict(&parsed.parent_header.header));
    Ok((fake.clone(), ConfiguredParentClassifier::Fake(fake)))
}

#[tokio::test]
async fn scheduled_recheck_resumes_from_its_cursor_across_an_additive_trigger() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (fake, classifier) = two_unknown_parents_and_a_classifier(&client).await?;
        let generation = schedule_core_recheck(&client, &RecheckScope::everything(), true).await?;

        let first = run_scheduled_recheck_pages_for_test(&mut client, &classifier, 1, 1).await?;
        assert_eq!(first.candidates, 1);
        assert_eq!(first.acknowledged, None);
        let bound = load_scheduled_recheck(&client).await?;
        let pass = bound.pass.expect("a page leaves the pass bound");
        assert_eq!(pass.generation, generation);
        assert!(pass.cursor.is_some());
        assert_eq!(fake.call_count().await, 1);

        let advanced = schedule_core_recheck(&client, &RecheckScope::pending_rows(), false).await?;
        assert_eq!(advanced, generation + 1);
        let second = run_scheduled_recheck_pages_for_test(&mut client, &classifier, 1, 1).await?;
        assert_eq!(second.candidates, 1);
        assert_eq!(fake.call_count().await, 2);
        let continued = load_scheduled_recheck(&client).await?;
        assert_eq!(continued.pass.map(|pass| pass.generation), Some(generation));
        assert_eq!(continued.pending_scope, RecheckScope::pending_rows());

        let still_pending: i64 = client
            .query_one(
                "SELECT count(*) FROM block WHERE kind = 'unknown' AND btc_orphan_class IS NULL",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(still_pending, 1);
        let rest = run_scheduled_recheck(&mut client, &classifier, 1).await?;
        assert_eq!(rest.candidates, 1);
        assert_eq!(rest.acknowledged, Some(advanced));
        assert_eq!(fake.call_count().await, 3);
        let done = load_scheduled_recheck(&client).await?;
        assert!(!done.is_pending());
        assert_eq!(done.pass, None);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn an_invalidating_trigger_restarts_the_pass_with_the_merged_scope() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (fake, classifier) = two_unknown_parents_and_a_classifier(&client).await?;
        let generation = schedule_core_recheck(&client, &RecheckScope::everything(), true).await?;
        let first = run_scheduled_recheck_pages_for_test(&mut client, &classifier, 1, 1).await?;
        assert_eq!(first.candidates, 1);
        assert_eq!(fake.call_count().await, 1);

        let hathor = RecheckScope {
            orphans: true,
            sources: Some(vec!["auxpow:hathor".to_owned()]),
        };
        let newer = schedule_core_recheck(&client, &hathor, true).await?;
        assert_eq!(newer, generation + 1);
        let pending = load_scheduled_recheck(&client).await?;
        assert_eq!(
            pending.pass, None,
            "an invalidating trigger clears the pass"
        );
        assert_eq!(
            pending.pending_scope,
            RecheckScope::everything(),
            "the cut-short pass's scope is folded back into the pending scope"
        );
        let rest = run_scheduled_recheck(&mut client, &classifier, 1).await?;
        assert_eq!(rest.candidates, 2);
        assert_eq!(rest.acknowledged, Some(newer));
        assert_eq!(fake.call_count().await, 3);
        let done = load_scheduled_recheck(&client).await?;
        assert!(!done.is_pending());
        assert_eq!(done.acknowledged_generation, newer);
        assert_eq!(done.pass, None);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn a_scoped_pass_finishes_and_a_later_trigger_runs_as_a_follow_up() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (fake, classifier) = two_unknown_parents_and_a_classifier(&client).await?;

        let state = load_scheduled_recheck(&client).await?;
        let seeded = bind_recheck_pass(&client, state.pending_scope).await?;
        acknowledge_recheck_pass(&client, seeded.generation).await?;

        let hathor = RecheckScope {
            orphans: true,
            sources: Some(vec!["auxpow:hathor".to_owned()]),
        };
        let scoped = schedule_core_recheck(&client, &hathor, true).await?;
        let scheduled = load_scheduled_recheck(&client).await?;
        let old_pass = bind_recheck_pass(&client, scheduled.pending_scope).await?;
        assert_eq!(old_pass.generation, scoped);
        assert_eq!(old_pass.scope, hathor);
        let consumed = load_scheduled_recheck(&client).await?;
        assert_eq!(consumed.pending_scope.sources, Some(Vec::new()));
        assert!(!consumed.pending_scope.orphans);

        let widened = schedule_core_recheck(&client, &RecheckScope::pending_rows(), false).await?;
        let pending = load_scheduled_recheck(&client).await?;
        assert_eq!(pending.pending_generation, widened);
        assert_eq!(pending.pending_scope, RecheckScope::pending_rows());
        assert_eq!(pending.pass.map(|pass| pass.generation), Some(scoped));

        let report = run_scheduled_recheck(&mut client, &classifier, 10).await?;
        assert_eq!(report.candidates, 2);
        assert_eq!(report.acknowledged, Some(widened));
        assert_eq!(fake.call_count().await, 2);
        let done = load_scheduled_recheck(&client).await?;
        assert_eq!(done.acknowledged_generation, widened);
        assert!(!done.is_pending());
        Ok::<_, anyhow::Error>(())
    })
}

fn synthesized_competitor(header: &Header) -> Header {
    let mut competitor = *header;
    competitor.nonce = competitor.nonce.wrapping_add(1);
    competitor
}

fn stale_with_competitor(header: &Header, height: i32) -> mmm_bitcoin_core::ParentClassification {
    let competitor = synthesized_competitor(header);
    let competitor_hash = competitor.block_hash().to_byte_array().to_vec();
    stale_verdict_with_competitor_header(header, height, competitor, competitor_hash)
}

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
async fn an_interrupted_cascade_survives_in_the_durable_queue() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let parent = fixture
            .insert_event(&client, 500_001, ClassificationProof::default(), 2_100)
            .await?;
        let mut descendant_header = parent.header;
        descendant_header.prev_blockhash = parent.header.block_hash();
        descendant_header.nonce = descendant_header.nonce.wrapping_add(7);
        let descendant = fixture
            .insert_event_with_header(
                &client,
                500_000,
                0x7d,
                descendant_header,
                ClassificationProof::default(),
                2_099,
            )
            .await?;
        let generation = schedule_core_recheck(&client, &RecheckScope::everything(), true).await?;

        let interrupted = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(unknown_verdict(&descendant.header))
                .with_verdicts_for(
                    &parent.header,
                    [Some(stale_with_competitor(&parent.header, 720_000))],
                )
                .with_verdicts_for(
                    &descendant.header,
                    [Some(unknown_verdict(&descendant.header)), None],
                ),
        );
        let error = run_scheduled_recheck(&mut client, &interrupted, 10)
            .await
            .expect_err("the injected failure must fail the run");
        assert!(format!("{error:#}").contains("injected classification error for"));
        assert_eq!(event_parent_kind(&client, parent.id).await?, "stale");
        assert_eq!(event_parent_kind(&client, descendant.id).await?, "unknown");
        let cut_short = load_scheduled_recheck(&client).await?;
        assert!(
            cut_short
                .pass
                .as_ref()
                .is_some_and(|pass| pass.cursor.is_some())
        );
        let queued: i64 = client
            .query_one(
                "SELECT count(*) FROM bitcoin_core_reconcile_queue \
                 WHERE btc_parent_header_hash = $1 AND primary_pending",
                &[&descendant.parent_hash],
            )
            .await?
            .get(0);
        assert_eq!(queued, 1, "the descendant waits in the durable queue");

        let recovered = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(stale_with_competitor(&descendant.header, 720_001))
                .with_verdicts_for(
                    &parent.header,
                    [Some(stale_with_competitor(&parent.header, 720_000))],
                ),
        );
        let report = run_scheduled_recheck(&mut client, &recovered, 10).await?;
        assert_eq!(report.candidates, 0);
        assert_eq!(report.acknowledged, Some(generation));
        assert_eq!(event_parent_kind(&client, descendant.id).await?, "stale");
        let remaining: i64 = client
            .query_one("SELECT count(*) FROM bitcoin_core_reconcile_queue", &[])
            .await?
            .get(0);
        assert_eq!(remaining, 0);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn a_synthesized_competitor_reaches_its_dependents_through_the_durable_queue() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let parent = fixture
            .insert_event(&client, 500_001, ClassificationProof::default(), 2_100)
            .await?;
        let competitor = synthesized_competitor(&parent.header);
        let mut dependent_header = parent.header;
        dependent_header.prev_blockhash = competitor.block_hash();
        dependent_header.nonce = dependent_header.nonce.wrapping_add(9);
        let dependent = fixture
            .insert_event_with_header(
                &client,
                500_000,
                0x9d,
                dependent_header,
                ClassificationProof::default(),
                2_099,
            )
            .await?;
        let generation = schedule_core_recheck(&client, &RecheckScope::everything(), true).await?;

        let fake = FakeParentClassifier::new(unknown_verdict(&dependent.header))
            .with_verdicts_for(
                &parent.header,
                [Some(stale_with_competitor(&parent.header, 720_000))],
            )
            .with_verdicts_for(
                &dependent.header,
                [
                    Some(unknown_verdict(&dependent.header)),
                    Some(canonical_verdict(&dependent.header, 720_001)),
                ],
            );
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        let report = run_scheduled_recheck(&mut client, &classifier, 10).await?;
        assert_eq!(report.candidates, 2);
        assert_eq!(report.acknowledged, Some(generation));
        assert_eq!(fake.call_count().await, 4);
        assert_eq!(event_parent_kind(&client, parent.id).await?, "stale");
        assert_eq!(event_parent_kind(&client, dependent.id).await?, "canonical");
        let remaining: i64 = client
            .query_one("SELECT count(*) FROM bitcoin_core_reconcile_queue", &[])
            .await?
            .get(0);
        assert_eq!(remaining, 0);
        Ok::<_, anyhow::Error>(())
    })
}

async fn seed_canonical_successors(
    client: &mut Client,
    fixture: &NamecoinEventFixture,
    head: &Header,
    first_child: i32,
) -> Result<(InsertedNamecoinEvent, InsertedNamecoinEvent)> {
    let mut first_header = *head;
    first_header.prev_blockhash = head.block_hash();
    first_header.nonce = first_header.nonce.wrapping_add(11);
    let first = fixture
        .insert_event_with_header(
            client,
            first_child,
            0x11,
            first_header,
            ClassificationProof::default(),
            2_101,
        )
        .await?;
    let mut second_header = *head;
    second_header.prev_blockhash = first_header.block_hash();
    second_header.nonce = second_header.nonce.wrapping_add(12);
    let second = fixture
        .insert_event_with_header(
            client,
            first_child + 1,
            0x12,
            second_header,
            ClassificationProof::default(),
            2_102,
        )
        .await?;
    let successors = ConfiguredParentClassifier::Fake(
        FakeParentClassifier::new(canonical_verdict(&first.header, 720_001)).with_verdicts_for(
            &second.header,
            [Some(canonical_verdict(&second.header, 720_002))],
        ),
    );
    reconcile_from_merge_mining_event(client, first.id, &successors, None).await?;
    reconcile_from_merge_mining_event(client, second.id, &successors, None).await?;
    assert_eq!(event_parent_kind(client, first.id).await?, "canonical");
    assert_eq!(event_parent_kind(client, second.id).await?, "canonical");
    Ok((first, second))
}

async fn queue_len(client: &Client) -> Result<i64> {
    Ok(client
        .query_one("SELECT count(*) FROM bitcoin_core_reconcile_queue", &[])
        .await?
        .get(0))
}

#[tokio::test]
async fn the_scheduled_cascade_stops_at_an_unchanged_descendant() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let parent = fixture
            .insert_event(&client, 500_000, ClassificationProof::default(), 2_100)
            .await?;
        let (first, second) =
            seed_canonical_successors(&mut client, &fixture, &parent.header, 500_001).await?;

        let generation = schedule_core_recheck(&client, &RecheckScope::everything(), true).await?;
        let fake = FakeParentClassifier::new(canonical_verdict(&parent.header, 720_000))
            .with_verdicts_for(
                &first.header,
                [Some(canonical_verdict(&first.header, 720_001))],
            )
            .with_verdicts_for(&second.header, [None]);
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        let report = run_scheduled_recheck(&mut client, &classifier, 10).await?;
        assert_eq!(report.candidates, 1);
        assert_eq!(report.changed, 1);
        assert_eq!(report.acknowledged, Some(generation));
        assert_eq!(fake.call_count().await, 2);
        assert_eq!(event_parent_kind(&client, parent.id).await?, "canonical");
        assert_eq!(queue_len(&client).await?, 0);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn a_candidate_changed_by_another_candidate_expands_once_without_walking_on() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let parent = fixture
            .insert_event(&client, 500_000, ClassificationProof::default(), 2_100)
            .await?;
        let competitor_header = synthesized_competitor(&parent.header);
        let competitor = fixture
            .insert_event_with_header(
                &client,
                500_001,
                0x1c,
                competitor_header,
                ClassificationProof::default(),
                2_100,
            )
            .await?;
        let (first, second) =
            seed_canonical_successors(&mut client, &fixture, &competitor_header, 500_002).await?;
        let generation = schedule_core_recheck(&client, &RecheckScope::everything(), true).await?;
        let fake = FakeParentClassifier::new(canonical_verdict(&parent.header, 720_000))
            .with_verdicts_for(
                &parent.header,
                [Some(stale_with_competitor(&parent.header, 720_000))],
            )
            .with_verdicts_for(
                &competitor.header,
                [Some(canonical_verdict(&competitor.header, 720_000))],
            )
            .with_verdicts_for(
                &first.header,
                [Some(canonical_verdict(&first.header, 720_001))],
            )
            .with_verdicts_for(&second.header, [None]);
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        let report = run_scheduled_recheck(&mut client, &classifier, 10).await?;
        assert_eq!(report.candidates, 2);
        assert_eq!(report.acknowledged, Some(generation));
        assert_eq!(event_parent_kind(&client, parent.id).await?, "stale");
        assert_eq!(
            event_parent_kind(&client, competitor.id).await?,
            "canonical"
        );
        assert_eq!(queue_len(&client).await?, 0);
        Ok::<_, anyhow::Error>(())
    })
}
