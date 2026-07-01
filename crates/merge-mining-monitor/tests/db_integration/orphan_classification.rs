use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_capture::capture::ClassificationProof;
use mmm_read_model::{
    ReclassifyUnknownParentsConfig, reconcile_from_merge_mining_event,
    run_reclassify_unknown_parents,
};
use mmm_store::upsert_merge_mining_event;
use tokio_postgres::Client;

use crate::support::scenario::{canonical_verdict, orphan_candidate_verdict};
use crate::support::{namecoin_event_payload, namecoin_fixture};

struct UnknownParentFixture {
    event_id: i64,
    parent_hash: Vec<u8>,
    header: Header,
}

#[tokio::test]
async fn core_absent_unknown_keeps_wrong_epoch_exclusion_across_transient_recheck() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = insert_unknown_parent_event(&client).await?;

        // First pass: Core-absent and wrong-epoch (difficulty_epoch_ok = false) ->
        // excluded.
        let mut wrong_epoch = orphan_candidate_verdict(&fixture.header);
        wrong_epoch.difficulty_epoch_ok = Some(false);
        reconcile_from_merge_mining_event(
            &mut client,
            fixture.event_id,
            &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(wrong_epoch)),
            None,
        )
        .await?;

        // A later --recheck-orphans pass where the inferred-stale competitor lookup
        // is transiently missing: Core-absent but difficulty_epoch_ok = None. The
        // proven wrong-epoch evidence must be preserved (both the block column and
        // the orphan class) so the parent does NOT flip to a strict/weak orphan.
        let mut transient = orphan_candidate_verdict(&fixture.header);
        transient.difficulty_epoch_ok = None;
        reconcile_from_merge_mining_event(
            &mut client,
            fixture.event_id,
            &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(transient)),
            None,
        )
        .await?;

        let row = client
            .query_one(
                "SELECT btc_orphan_class, difficulty_epoch_ok FROM block WHERE btc_header_hash = $1",
                &[&fixture.parent_hash],
            )
            .await?;
        assert_eq!(
            row.get::<_, Option<String>>(0).as_deref(),
            Some("btc_stale_excluded"),
            "a transient recheck must not flip a wrong-epoch exclusion to an orphan"
        );
        assert_eq!(
            row.get::<_, Option<bool>>(1),
            Some(false),
            "the proven wrong-epoch difficulty must survive a transient recheck"
        );
        // The event keeps difficulty_epoch_ok = false too (COALESCE, not clobber),
        // so the block column and the event rollup agree and the missing-only
        // repair scanner sees no drift to churn on.
        let event_difficulty: Option<bool> = client
            .query_one(
                "SELECT difficulty_epoch_ok FROM merge_mining_event WHERE id = $1",
                &[&fixture.event_id],
            )
            .await?
            .get(0);
        assert_eq!(
            event_difficulty,
            Some(false),
            "a transient recheck must not clobber the event's proven wrong-epoch difficulty"
        );
        Ok::<_, anyhow::Error>(())
    })
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

        // Core-absent classification sets the orphan class while kind remains
        // unknown.
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
                Some("strict_btc_orphan" | "weak_btc_orphan" | "btc_stale_excluded")
            ),
            "Core-attested-absent unknown must be classified, got {orphan_class:?}"
        );

        // Promote to canonical: kind and btc_orphan_class must change in the same
        // statement so the CHECK is never violated, leaving the class NULL.
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
    // Simulate a prior run that recorded a DIFFERENT class, then recheck: the
    // parent is re-included AND the corrected verdict is a real change, so it is
    // counted (proving --recheck-orphans does re-evaluate, not just skip).
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

        // First pass classifies the orphan (NULL -> non-NULL counts as progress).
        let first = run_reclassify_unknown_parents(
            &mut client,
            &absent_classifier,
            ReclassifyUnknownParentsConfig {
                batch_size: 10,
                recheck_orphans: false,
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

        // Default rerun skips the already-classified parent (no rescan).
        let second = run_reclassify_unknown_parents(
            &mut client,
            &absent_classifier,
            ReclassifyUnknownParentsConfig {
                batch_size: 10,
                recheck_orphans: false,
            },
        )
        .await?;
        assert_eq!(
            second, 0,
            "default pass skips already orphan-classified parents"
        );

        // --recheck-orphans RE-INCLUDES the parent, but with the same classifier the
        // verdict is unchanged, so it must NOT count as progress: count=0 keeps
        // meaning "no scanned parent changed", and repeated rechecks do not churn a
        // nonzero count forever.
        let recheck_unchanged = run_reclassify_unknown_parents(
            &mut client,
            &absent_classifier,
            ReclassifyUnknownParentsConfig {
                batch_size: 10,
                recheck_orphans: true,
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
    let payload = namecoin_event_payload(
        &parsed,
        &resolver,
        &pool_ids_by_slug,
        500_000,
        ClassificationProof::default(),
        1_000,
    )?;
    let event_id = upsert_merge_mining_event(client, source_id, &payload).await?;
    Ok(UnknownParentFixture {
        event_id,
        parent_hash: parsed.parent_header.hash().to_byte_array().to_vec(),
        header: parsed.parent_header.header,
    })
}
