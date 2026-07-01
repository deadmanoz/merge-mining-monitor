use anyhow::Result;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, FakeParentClassifier, HeightSource, ParentClassification,
};
use mmm_capture::capture::{ClassificationProof, ParentKind};
use mmm_capture::source_registry::RSK_SOURCE_CODE;
use mmm_read_model::{
    compute_source_health_from_base, rebuild_source_health, reconcile_from_merge_mining_event,
    restore_merge_mining_event, revoke_merge_mining_event,
};
use mmm_store::get_source_id;
use tokio_postgres::Client;

use crate::support::scenario::{orphan_candidate_verdict, stale_verdict_with_competitor_header};
use crate::support::{
    NamecoinEventFixture, capture_test_payload, classified_proof, namecoin_event_payload,
    parse_auxpow_fixture,
};
/// The comparable per-source counters: (events, last_event_seen, near, unknown,
/// canonical, stale, strict_orphan, weak_orphan). Excludes the `updated_at` audit
/// column.
pub(crate) type SourceHealthCounts = (i64, Option<i64>, i64, i64, i64, i64, i64, i64);

/// The semantic (non-`updated_at`) `source_health` projection, sorted by source.
pub(crate) async fn read_source_health_semantic(
    client: &Client,
) -> Result<Vec<(i64, SourceHealthCounts)>> {
    let rows = client
        .query(
            "SELECT source_id, events, last_event_seen, near_parents, unknown_parents, \
                    canonical_parents, stale_parents, strict_orphan_parents, weak_orphan_parents \
             FROM source_health ORDER BY source_id",
            &[],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<_, i64>(0),
                (
                    r.get::<_, i64>(1),
                    r.get::<_, Option<i64>>(2),
                    r.get::<_, i64>(3),
                    r.get::<_, i64>(4),
                    r.get::<_, i64>(5),
                    r.get::<_, i64>(6),
                    r.get::<_, i64>(7),
                    r.get::<_, i64>(8),
                ),
            )
        })
        .collect())
}

/// Assert the incrementally-maintained `source_health` + the global invalid
/// counter equal the non-mutating recompute. A maintained row whose
/// source has no recompute entry must be all-zeros (a source that lost its last
/// active event keeps a zeroed row).
pub(crate) async fn assert_source_health_matches_recompute(
    client: &Client,
    context: &str,
) -> Result<()> {
    let computed = compute_source_health_from_base(client).await?;
    let mut expected: std::collections::HashMap<i64, SourceHealthCounts> = computed
        .rows
        .iter()
        .map(|r| {
            (
                r.source_id,
                (
                    r.events,
                    r.last_event_seen,
                    r.near_parents,
                    r.unknown_parents,
                    r.canonical_parents,
                    r.stale_parents,
                    r.strict_orphan_parents,
                    r.weak_orphan_parents,
                ),
            )
        })
        .collect();
    for (sid, maintained) in read_source_health_semantic(client).await? {
        match expected.remove(&sid) {
            Some(exp) => assert_eq!(
                maintained, exp,
                "{context}: source {sid} maintained != recompute"
            ),
            None => assert_eq!(
                maintained,
                (0, None, 0, 0, 0, 0, 0, 0),
                "{context}: maintained source {sid} absent from recompute but not all-zero"
            ),
        }
    }
    assert!(
        expected.is_empty(),
        "{context}: recompute has sources missing from source_health: {:?}",
        expected.keys().collect::<Vec<_>>()
    );
    let invalid: i64 = client
        .query_one(
            "SELECT invalid_unknown_parents FROM read_model_invariant WHERE id = TRUE",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(
        invalid, computed.invalid_unknown_parents,
        "{context}: invalid_unknown_parents maintained != recompute"
    );
    Ok(())
}

/// The Core-attested canonical `ParentClassification` used to promote an unknown
/// parent in these reconcile tests (known height, fresh Core observation).
fn canonical_at(prev_hash: Vec<u8>, height: i32) -> ParentClassification {
    ParentClassification {
        kind: ParentKind::Canonical,
        height: Some(height),
        height_source: Some(HeightSource::BitcoinCore),
        prev_hash,
        canonical_predecessor_header: None,
        canonical_competitor_header: None,
        canonical_competitor_hash: None,
        coinbase: None,
        difficulty_epoch_ok: Some(true),
        live_observed: true,
        core_attested: true,
        core_absence_attested: false,
    }
}

/// `(strict_orphan, weak_orphan, unknown, canonical)` distinct-parent counts for a
/// source, read straight from the maintained `source_health` row.
async fn read_sh_counts(client: &Client, source_id: i64) -> Result<(i64, i64, i64, i64)> {
    let row = client
        .query_one(
            "SELECT strict_orphan_parents, weak_orphan_parents, unknown_parents, \
                    canonical_parents \
             FROM source_health WHERE source_id = $1",
            &[&source_id],
        )
        .await?;
    Ok((row.get(0), row.get(1), row.get(2), row.get(3)))
}

// Drive real capture/revoke/restore paths while comparing maintained
// source_health rows against a full SQL recompute after each transition.
// capture_in_txn snapshots before upsert, so this catches incremental drift.
#[tokio::test]
async fn source_health_incremental_matches_recompute() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // Rebuild establishes the empty ready baseline before deltas.
        rebuild_source_health(&mut client).await?;
        let ready: bool = client
            .query_one(
                "SELECT source_health_ready FROM read_model_invariant WHERE id = TRUE",
                &[],
            )
            .await?
            .get(0);
        assert!(ready, "rebuild must set source_health_ready");
        let baseline: i64 = client
            .query_one("SELECT count(*)::bigint FROM source_health", &[])
            .await?
            .get(0);
        assert_eq!(baseline, 0, "fresh source_health is empty");

        let fixture = NamecoinEventFixture::new(&client).await?;
        let namecoin = fixture.source_id;
        let rsk = get_source_id(&client, RSK_SOURCE_CODE).await?;
        let classifier = ConfiguredParentClassifier::Disabled;
        let canonical_proof = || classified_proof(ParentKind::Canonical, 500_000);

        let mut p1 = fixture.payload(700_000, canonical_proof(), 100)?;
        let nc_event = capture_test_payload(&mut client, namecoin, &classifier, &mut p1).await?;
        assert_source_health_matches_recompute(&client, "after namecoin capture").await?;

        // The same parent through another source exercises source-scoped
        // counters over a shared parent hash.
        let mut p2 = fixture.payload(700_001, canonical_proof(), 200)?;
        capture_test_payload(&mut client, rsk, &classifier, &mut p2).await?;
        assert_source_health_matches_recompute(&client, "after rsk capture (two sources)").await?;

        // The near-parent fixture adds a distinct Namecoin parent through the
        // same mutation path, which catches distinct-parent maintenance drift.
        let near = parse_auxpow_fixture("500001-near-parent")?;
        let mut near_payload = namecoin_event_payload(
            &near,
            &fixture.resolver,
            &fixture.pool_ids_by_slug,
            500_001,
            ClassificationProof::default(),
            300,
        )?;
        capture_test_payload(&mut client, namecoin, &classifier, &mut near_payload).await?;
        assert_source_health_matches_recompute(&client, "after near capture").await?;

        revoke_merge_mining_event(&mut client, nc_event, "test_revoke", &classifier).await?;
        assert_source_health_matches_recompute(&client, "after revoke").await?;

        restore_merge_mining_event(&mut client, nc_event, &classifier).await?;
        assert_source_health_matches_recompute(&client, "after restore").await?;

        let before = read_source_health_semantic(&client).await?;
        rebuild_source_health(&mut client).await?;
        let after = read_source_health_semantic(&client).await?;
        assert_eq!(
            before, after,
            "rebuild must be idempotent on a maintained table"
        );

        Ok::<_, anyhow::Error>(())
    })
}

// Synthesizes the canonical competitor branch so stale counting runs under the
// recompute oracle instead of only checking hand-maintained counters.
#[tokio::test]
async fn source_health_matches_recompute_for_stale_with_competitor() -> Result<()> {
    crate::run_mut_db_test!(client, {
        rebuild_source_health(&mut client).await?;
        let fixture = NamecoinEventFixture::new(&client).await?;
        let namecoin = fixture.source_id;
        let height = 700_005;

        let mut competitor_header = fixture.parsed.parent_header.header;
        competitor_header.nonce = competitor_header.nonce.wrapping_add(1);
        let competitor_hash = competitor_header.block_hash().to_byte_array().to_vec();
        let classification = stale_verdict_with_competitor_header(
            &fixture.parsed.parent_header.header,
            height,
            competitor_header,
            competitor_hash,
        );
        let classifier =
            ConfiguredParentClassifier::Fake(FakeParentClassifier::new(classification));

        let mut payload =
            fixture.payload(height, classified_proof(ParentKind::Stale, height), 500)?;
        capture_test_payload(&mut client, namecoin, &classifier, &mut payload).await?;

        assert_source_health_matches_recompute(&client, "after stale-with-competitor capture")
            .await?;
        let stale_count: i64 = client
            .query_one(
                "SELECT stale_parents FROM source_health WHERE source_id = $1",
                &[&namecoin],
            )
            .await?
            .get(0);
        assert_eq!(stale_count, 1, "the stale bucket must be set directly");

        Ok::<_, anyhow::Error>(())
    })
}

// Reconcile uses the no-wrapper PrimaryDiff path, distinct from producer
// capture/revoke, and must move source_health buckets the same way. Orphan
// class is a refinement inside kind=unknown: strict/weak counters move while
// current_kind remains unknown.
#[tokio::test]
async fn source_health_matches_recompute_on_orphan_class_transition() -> Result<()> {
    crate::run_mut_db_test!(client, {
        rebuild_source_health(&mut client).await?;
        let fixture = NamecoinEventFixture::new(&client).await?;
        let namecoin = fixture.source_id;
        let header = fixture.parsed.parent_header.header;
        let parent_hash = fixture.parsed.parent_header.hash().to_byte_array().to_vec();
        let disabled = ConfiguredParentClassifier::Disabled;
        let height = 700_020;

        let mut payload = fixture.payload(height, ClassificationProof::default(), 700)?;
        let event_id = capture_test_payload(&mut client, namecoin, &disabled, &mut payload).await?;
        assert_source_health_matches_recompute(&client, "after unknown capture").await?;
        let (strict, weak, ..) = read_sh_counts(&client, namecoin).await?;
        assert_eq!(
            (strict, weak),
            (0, 0),
            "a pending (NULL) unknown contributes no orphan counts"
        );

        reconcile_from_merge_mining_event(
            &mut client,
            event_id,
            &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(orphan_candidate_verdict(
                &header,
            ))),
            None,
        )
        .await?;
        // The Core-absent verdict writes btc_orphan_class inside the
        // source_health bracket; kind remains unknown but orphan counters move.
        assert_source_health_matches_recompute(&client, "after orphan classification").await?;
        let class: String = client
            .query_one(
                "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await?
            .get::<_, Option<String>>(0)
            .expect("a core-absent unknown must be classified, not pending");
        let (strict, weak, unknown, _) = read_sh_counts(&client, namecoin).await?;
        assert_eq!(
            strict,
            (class == "strict_btc_orphan") as i64,
            "strict counter must track btc_orphan_class (got {class})"
        );
        assert_eq!(
            weak,
            (class == "weak_btc_orphan") as i64,
            "weak counter must track btc_orphan_class (got {class})"
        );
        assert_eq!(
            unknown, 1,
            "still one unknown parent: the orphan class is a refinement of unknown"
        );

        // Promotion to canonical clears the orphan refinement counters as it
        // leaves unknown.
        let canonical = canonical_at(header.prev_blockhash.to_byte_array().to_vec(), height);
        reconcile_from_merge_mining_event(
            &mut client,
            event_id,
            &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(canonical)),
            None,
        )
        .await?;
        assert_source_health_matches_recompute(&client, "after promotion to canonical").await?;
        let (strict, weak, unknown, canonical_count) = read_sh_counts(&client, namecoin).await?;
        assert_eq!(
            (strict, weak),
            (0, 0),
            "promotion out of unknown clears the orphan counts"
        );
        assert_eq!(unknown, 0, "no longer unknown");
        assert_eq!(canonical_count, 1, "now canonical");

        Ok::<_, anyhow::Error>(())
    })
}
