use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::BitcoinCoreBlockCoinbase;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_capture::capture::{
    ClassificationProof, EventPoolAttribution, MergeMiningEventPayload, NormalizedEventEvidence,
    ParentKind, PoolAttributionConfidence, PoolAttributionSide, ResolvedPoolAttributions,
    build_event_payload_from_evidence,
};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_capture::test_support::header_meeting_bits_with_prev;
use mmm_read_model::{
    CoreCanonicalWrite, rebuild_source_health, update_parent_events, write_core_canonical,
};
use mmm_read_model::{restore_merge_mining_event, revoke_merge_mining_event};
use mmm_store::{get_source_id, upsert_event_pool_attributions};
use serde_json::json;
use tokio_postgres::Client;

use crate::source_health::{assert_source_health_matches_recompute, read_source_health_semantic};
use crate::support::scenario::{
    canonical_verdict, stale_verdict_with_competitor_header, unknown_verdict,
};
use crate::support::{
    DefaultPoolSnapshot, NamecoinEventFixture, capture_test_payload, classified_proof,
    default_pool_snapshot,
};

async fn read_block_mutation_state(
    client: &Client,
    hash: &[u8],
) -> Result<Option<(String, Option<i32>, i32, i32, bool, Option<i64>)>> {
    let row = client
        .query_opt(
            "SELECT kind, btc_height, total_attestations, distinct_sources, \
                    core_attested, bitcoin_miner_pool_id \
             FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await?;
    Ok(row.map(|row| {
        (
            row.get(0),
            row.get(1),
            row.get(2),
            row.get(3),
            row.get(4),
            row.get(5),
        )
    }))
}

async fn canonical_parents_for(client: &Client, source_id: i64) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT canonical_parents FROM source_health WHERE source_id = $1",
            &[&source_id],
        )
        .await?
        .get(0))
}

async fn proof_is_active(client: &Client, parent_hash: &[u8], source_id: i64) -> Result<bool> {
    Ok(client
        .query_one(
            "SELECT revoked_at IS NULL FROM attestation_proof \
             WHERE btc_header_hash = $1 AND source_id = $2 AND proof_kind = 'auxpow'",
            &[&parent_hash, &source_id],
        )
        .await?
        .get(0))
}

async fn mutation_pool_snapshot(client: &mut Client) -> Result<(DefaultPoolSnapshot, i64)> {
    rebuild_source_health(client).await?;
    let snapshot = default_pool_snapshot(client).await?;
    let namecoin = get_source_id(client, NAMECOIN_SOURCE_CODE).await?;
    Ok((snapshot, namecoin))
}

async fn mutation_namecoin_fixture(client: &mut Client) -> Result<NamecoinEventFixture> {
    rebuild_source_health(client).await?;
    NamecoinEventFixture::new(client).await
}

fn apply_canonical_proof(payload: &mut MergeMiningEventPayload, height: i32) -> Result<()> {
    mmm_capture::capture::apply_classification_proof(
        payload,
        classified_proof(ParentKind::Canonical, height),
    )
}

// Mutation tests pass easy-bits parent headers here so they can synthesize
// chained Core/cascade scenarios without real chain fixtures.
fn crafted_unknown_payload(
    parent_header: Header,
    child_height: i32,
    child_seed: u8,
    observed_at: i64,
) -> Result<MergeMiningEventPayload> {
    let evidence = NormalizedEventEvidence {
        child_height,
        child_block_hash: vec![child_seed; 32],
        child_block_time: observed_at,
        btc_parent_header: parent_header,
        pow_validates_child_target: Some(true),
        btc_parent_coinbase_txid: None,
        btc_parent_coinbase_script: None,
        btc_parent_coinbase_outputs: None,
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: None,
    };
    build_event_payload_from_evidence(
        evidence,
        ResolvedPoolAttributions::default(),
        ClassificationProof::default(),
        observed_at,
    )
}

// These tests exercise read_model::mutation transaction entry points: capture,
// revoke/restore, Core writes, and event-pool cascade updates. Each scenario
// pairs table assertions with the source_health recompute oracle.

#[tokio::test]
async fn mutation_capture_changed_then_unchanged_is_idempotent() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = mutation_namecoin_fixture(&mut client).await?;
        let classifier = ConfiguredParentClassifier::Disabled;
        let proof = classified_proof(ParentKind::Canonical, 500_000);
        let build = || fixture.payload(710_000, proof, 100);
        let parent_hash = fixture.parsed.parent_header.hash().to_byte_array().to_vec();

        let mut payload = build()?;
        let event_id =
            capture_test_payload(&mut client, fixture.source_id, &classifier, &mut payload).await?;
        let after_first = read_block_mutation_state(&client, &parent_hash)
            .await?
            .context("block row missing after first capture")?;
        assert_eq!(after_first.0, "canonical", "classified parent kind");
        assert_eq!(after_first.1, Some(500_000), "classified parent height");
        assert_eq!(after_first.2, 1, "one attestation");
        assert!(
            proof_is_active(&client, &parent_hash, fixture.source_id).await?,
            "derived auxpow proof is active"
        );
        let sh_first = read_source_health_semantic(&client).await?;
        assert_source_health_matches_recompute(&client, "after changed capture").await?;

        // A byte-identical replay must return the same event and leave derived
        // block/source_health projections unchanged.
        let mut replay = build()?;
        let replay_id =
            capture_test_payload(&mut client, fixture.source_id, &classifier, &mut replay).await?;
        assert_eq!(
            replay_id, event_id,
            "idempotent upsert returns the same event"
        );
        let after_replay = read_block_mutation_state(&client, &parent_hash)
            .await?
            .context("block row missing after replay")?;
        assert_eq!(
            after_first, after_replay,
            "unchanged capture must not move the block"
        );
        assert_eq!(
            sh_first,
            read_source_health_semantic(&client).await?,
            "unchanged capture must not move source_health"
        );
        assert_source_health_matches_recompute(&client, "after unchanged capture").await?;

        Ok::<_, anyhow::Error>(())
    })
}

// Revoke/restore should demote and recover the parent read model, including
// proof activity and source_health, while remaining idempotent at both edges.
#[tokio::test]
async fn mutation_revoke_and_restore_roundtrip_block_proof_source_health() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = mutation_namecoin_fixture(&mut client).await?;
        let classifier = ConfiguredParentClassifier::Disabled;
        let parent_hash = fixture.parsed.parent_header.hash().to_byte_array().to_vec();

        let mut payload = fixture.payload(
            710_001,
            classified_proof(ParentKind::Canonical, 500_000),
            100,
        )?;
        let event_id =
            capture_test_payload(&mut client, fixture.source_id, &classifier, &mut payload).await?;

        assert_eq!(
            canonical_parents_for(&client, fixture.source_id).await?,
            1,
            "captured canonical"
        );

        revoke_merge_mining_event(&mut client, event_id, "mutation_test", &classifier).await?;
        let demoted = read_block_mutation_state(&client, &parent_hash)
            .await?
            .context("block row missing after revoke")?;
        assert_eq!(
            demoted.0, "unknown",
            "zero-active parent demotes to unknown"
        );
        assert_eq!(demoted.1, None, "demoted parent loses its height");
        assert_eq!(demoted.2, 0, "no active attestations");
        assert!(
            !proof_is_active(&client, &parent_hash, fixture.source_id).await?,
            "derived proof revoked with its only event"
        );
        assert_eq!(
            canonical_parents_for(&client, fixture.source_id).await?,
            0,
            "bucket emptied"
        );
        assert_source_health_matches_recompute(&client, "after revoke").await?;

        let sh_revoked = read_source_health_semantic(&client).await?;
        revoke_merge_mining_event(&mut client, event_id, "mutation_test", &classifier).await?;
        assert_eq!(
            sh_revoked,
            read_source_health_semantic(&client).await?,
            "double revoke must not move source_health"
        );

        restore_merge_mining_event(&mut client, event_id, &classifier).await?;
        let restored = read_block_mutation_state(&client, &parent_hash)
            .await?
            .context("block row missing after restore")?;
        assert_eq!(restored.0, "canonical", "restore recovers the proven kind");
        assert_eq!(restored.1, Some(500_000), "restore recovers the height");
        assert_eq!(restored.2, 1, "active attestation back");
        assert!(
            proof_is_active(&client, &parent_hash, fixture.source_id).await?,
            "derived proof active again"
        );
        assert_eq!(
            canonical_parents_for(&client, fixture.source_id).await?,
            1,
            "bucket refilled"
        );
        assert_source_health_matches_recompute(&client, "after restore").await?;

        let sh_restored = read_source_health_semantic(&client).await?;
        restore_merge_mining_event(&mut client, event_id, &classifier).await?;
        assert_eq!(
            sh_restored,
            read_source_health_semantic(&client).await?,
            "double restore must not move source_health"
        );

        Ok::<_, anyhow::Error>(())
    })
}

// A transient classifier unknown is weaker than an already proven canonical
// parent; replay must preserve the existing block and event classification.
#[tokio::test]
async fn mutation_classifier_transient_unknown_preserves_canonical() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = mutation_namecoin_fixture(&mut client).await?;
        let parent_header = fixture.parsed.parent_header.header;
        let parent_hash = fixture.parsed.parent_header.hash().to_byte_array().to_vec();
        let height = 500_000;

        // First response proves canonical; the replay response is unknown.
        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
            canonical_verdict(&parent_header, height),
            unknown_verdict(&parent_header),
        ]));

        let build = || fixture.payload(710_002, ClassificationProof::default(), 100);
        let mut payload = build()?;
        capture_test_payload(&mut client, fixture.source_id, &classifier, &mut payload).await?;
        let proven = read_block_mutation_state(&client, &parent_hash)
            .await?
            .context("block row missing after proven capture")?;
        assert_eq!(proven.0, "canonical");
        assert_eq!(proven.1, Some(height));
        assert!(proven.4, "core attested by the enabled classifier");

        let mut replay = build()?;
        let replayed_event =
            capture_test_payload(&mut client, fixture.source_id, &classifier, &mut replay).await?;
        let preserved = read_block_mutation_state(&client, &parent_hash)
            .await?
            .context("block row missing after transient replay")?;
        assert_eq!(
            preserved.0, "canonical",
            "transient unknown must not demote a proven canonical block"
        );
        assert_eq!(preserved.1, Some(height), "height preserved");
        let event_kind: String = client
            .query_one(
                "SELECT btc_parent_kind FROM merge_mining_event WHERE id = $1",
                &[&replayed_event],
            )
            .await?
            .get(0);
        assert_eq!(
            event_kind, "canonical",
            "event classification preserved across the transient unknown"
        );
        assert_source_health_matches_recompute(&client, "after transient replay").await?;

        Ok::<_, anyhow::Error>(())
    })
}

async fn assert_core_write_bracket_parity(
    client: &mut Client,
    namecoin: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    // Part 1 (bracket parity): an event-backed unknown parent flips to
    // canonical when Core attests it; the bracket must move the buckets.
    let core_header = header_meeting_bits_with_prev(
        0x207f_ffff,
        1_700_000_100,
        0xAA,
        bitcoin::BlockHash::all_zeros(),
    );
    let core_hash = core_header.block_hash().to_byte_array().to_vec();
    let mut payload = crafted_unknown_payload(core_header, 710_010, 0x51, 200)?;
    capture_test_payload(client, namecoin, classifier, &mut payload).await?;
    let unknown_parents: i64 = client
        .query_one(
            "SELECT unknown_parents FROM source_health WHERE source_id = $1",
            &[&namecoin],
        )
        .await?
        .get(0);
    assert_eq!(unknown_parents, 1, "captured as unknown");

    write_core_canonical(
        client,
        CoreCanonicalWrite {
            header: &core_header,
            height: 850_000,
            coinbase: None,
        },
        async |_txn| Ok(()),
        "test core write",
    )
    .await?
    .cascade(client, classifier)
    .await?;

    let promoted = read_block_mutation_state(client, &core_hash)
        .await?
        .context("core block row missing")?;
    assert_eq!(promoted.0, "canonical", "Core write promotes the unknown");
    assert_eq!(promoted.1, Some(850_000));
    assert!(promoted.4, "core attested");
    let row = client
        .query_one(
            "SELECT unknown_parents, canonical_parents FROM source_health WHERE source_id = $1",
            &[&namecoin],
        )
        .await?;
    assert_eq!(
        row.get::<_, i64>(0),
        0,
        "bracket drained the unknown bucket"
    );
    assert_eq!(
        row.get::<_, i64>(1),
        1,
        "bracket filled the canonical bucket"
    );
    assert_source_health_matches_recompute(client, "after core write over active events").await?;
    Ok(())
}

async fn assert_core_write_cascade_repairs_dependent(
    client: &mut Client,
    namecoin: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    // Part 2 (cascade repair): a dependent event whose parent block row is
    // missing is rebuilt by the token's cascade after the Core write.
    let core_two = header_meeting_bits_with_prev(
        0x207f_ffff,
        1_700_000_200,
        0xBB,
        bitcoin::BlockHash::all_zeros(),
    );
    let dependent_parent =
        header_meeting_bits_with_prev(0x207f_ffff, 1_700_000_300, 0xCC, core_two.block_hash());
    let dependent_hash = dependent_parent.block_hash().to_byte_array().to_vec();
    let mut dependent = crafted_unknown_payload(dependent_parent, 710_011, 0x52, 300)?;
    capture_test_payload(client, namecoin, classifier, &mut dependent).await?;
    // Simulate the unreconciled-dependent state: strip the derived rows
    // (the parent contribution is unchanged: an active unknown event keeps
    // current_kind = 'unknown' with or without the block row).
    client
        .execute(
            "DELETE FROM attestation_proof WHERE btc_header_hash = $1",
            &[&dependent_hash],
        )
        .await?;
    client
        .execute(
            "DELETE FROM block WHERE btc_header_hash = $1",
            &[&dependent_hash],
        )
        .await?;
    assert!(
        read_block_mutation_state(client, &dependent_hash)
            .await?
            .is_none(),
        "dependent block row stripped"
    );

    write_core_canonical(
        client,
        CoreCanonicalWrite {
            header: &core_two,
            height: 850_001,
            coinbase: None,
        },
        async |_txn| Ok(()),
        "test core write two",
    )
    .await?
    .cascade(client, classifier)
    .await?;

    let repaired = read_block_mutation_state(client, &dependent_hash)
        .await?
        .context("cascade must rebuild the dependent block row")?;
    assert_eq!(
        repaired.0, "unknown",
        "dependent rebuilt with its event kind"
    );
    assert_eq!(repaired.2, 1, "dependent attestation restored");
    assert_source_health_matches_recompute(client, "after dependent cascade repair").await?;
    Ok(())
}

// Covers both source_health bracket parity and the historical cascade format
// where a Core write must rebuild dependent event-backed rows.
#[tokio::test]
async fn mutation_write_core_canonical_bracket_and_cascade() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (_, namecoin) = mutation_pool_snapshot(&mut client).await?;
        let classifier = ConfiguredParentClassifier::Disabled;

        assert_core_write_bracket_parity(&mut client, namecoin, &classifier).await?;
        assert_core_write_cascade_repairs_dependent(&mut client, namecoin, &classifier).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn mutation_core_coinbase_without_pool_preserves_existing_bitcoin_miner() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let ((_, pool_ids_by_slug), namecoin) = mutation_pool_snapshot(&mut client).await?;
        let kncminer = *pool_ids_by_slug
            .get("kncminer")
            .context("default snapshot missing kncminer")?;
        let height = 850_020;
        let (header, hash) =
            seed_kncminer_event_backed_canonical(&mut client, namecoin, height).await?;

        let committed = write_core_canonical(
            &mut client,
            CoreCanonicalWrite {
                header: &header,
                height,
                coinbase: Some(BitcoinCoreBlockCoinbase {
                    txid: vec![0x44; 32],
                    script: b"/DefinitelyUnknownCorePool/".to_vec(),
                    outputs: Vec::new(),
                }),
            },
            async |_txn| Ok(()),
            "test unmatched core coinbase",
        )
        .await?;
        let after_core_write = read_block_mutation_state(&client, &hash)
            .await?
            .context("canonical block row missing after Core enrichment")?;
        assert_eq!(
            after_core_write.5,
            Some(kncminer),
            "an unmatched Core coinbase script must not erase known Bitcoin miner attribution"
        );
        committed
            .cascade(&mut client, &ConfiguredParentClassifier::Disabled)
            .await?;

        Ok::<_, anyhow::Error>(())
    })
}

async fn seed_kncminer_event_backed_canonical(
    client: &mut Client,
    namecoin: i64,
    height: i32,
) -> Result<(Header, Vec<u8>)> {
    let header = header_meeting_bits_with_prev(
        0x207f_ffff,
        1_700_001_500,
        0xE1,
        bitcoin::BlockHash::all_zeros(),
    );
    let mut payload = crafted_unknown_payload(header, 710_030, 0x71, 600)?;
    payload.btc_parent_coinbase_script = Some(b"/KnCMiner/".to_vec());
    apply_canonical_proof(&mut payload, height)?;
    capture_test_payload(
        client,
        namecoin,
        &ConfiguredParentClassifier::Disabled,
        &mut payload,
    )
    .await?;
    let hash = header.block_hash().to_byte_array().to_vec();
    Ok((header, hash))
}

async fn seed_canonical_parent_p(
    client: &mut Client,
    namecoin: i64,
    height: i32,
) -> Result<(Header, Vec<u8>, i64)> {
    // Canonical parent P (pool unattributed) via producer proof.
    let p_header = header_meeting_bits_with_prev(
        0x207f_ffff,
        1_700_001_000,
        0xD1,
        bitcoin::BlockHash::all_zeros(),
    );
    let p_hash = p_header.block_hash().to_byte_array().to_vec();
    let mut p_payload = crafted_unknown_payload(p_header, 710_020, 0x61, 400)?;
    apply_canonical_proof(&mut p_payload, height)?;
    let p_event = capture_test_payload(
        client,
        namecoin,
        &ConfiguredParentClassifier::Disabled,
        &mut p_payload,
    )
    .await?;
    Ok((p_header, p_hash, p_event))
}

async fn seed_stale_competitor_s(
    client: &mut Client,
    namecoin: i64,
    height: i32,
    p_header: Header,
    p_hash: Vec<u8>,
) -> Result<Vec<u8>> {
    // Stale competitor S whose canonical competitor is P (Fake classifier).
    let s_header = header_meeting_bits_with_prev(
        0x207f_ffff,
        1_700_001_100,
        0xD2,
        bitcoin::BlockHash::all_zeros(),
    );
    let s_hash = s_header.block_hash().to_byte_array().to_vec();
    let fake = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
        stale_verdict_with_competitor_header(&s_header, height, p_header, p_hash.clone()),
    ));
    let mut s_payload = crafted_unknown_payload(s_header, 710_021, 0x62, 500)?;
    capture_test_payload(client, namecoin, &fake, &mut s_payload).await?;
    let canonical_pool_before = derived_competition_canonical_pool(client, &s_hash).await?;
    assert_eq!(
        canonical_pool_before, None,
        "derived competition starts unattributed"
    );
    Ok(s_hash)
}

async fn derived_competition_canonical_pool(
    client: &Client,
    stale_hash: &[u8],
) -> Result<Option<i64>> {
    Ok(client
        .query_one(
            "SELECT canonical.bitcoin_miner_pool_id \
             FROM block stale \
             JOIN block canonical \
               ON canonical.btc_header_hash = stale.canonical_competitor_hash \
             WHERE stale.btc_header_hash = $1 \
               AND stale.kind = 'stale' \
               AND canonical.kind = 'canonical'",
            &[&stale_hash],
        )
        .await?
        .get(0))
}

// Event-level btc_parent attribution is provenance. It must not overwrite the
// Bitcoin block miner fields, which come only from Bitcoin block evidence.
#[tokio::test]
async fn mutation_update_parent_events_btc_parent_attribution_does_not_drive_block_miner()
-> Result<()> {
    crate::run_mut_db_test!(client, {
        let ((_, pool_ids_by_slug), namecoin) = mutation_pool_snapshot(&mut client).await?;
        let pool_id = *pool_ids_by_slug
            .get("kncminer")
            .context("default snapshot missing kncminer")?;
        let height = 850_010;

        let (p_header, p_hash, p_event) =
            seed_canonical_parent_p(&mut client, namecoin, height).await?;
        let s_hash =
            seed_stale_competitor_s(&mut client, namecoin, height, p_header, p_hash.clone())
                .await?;

        // A btc_parent attribution row through the mutation entry point.
        update_parent_events(
            &mut client,
            &ConfiguredParentClassifier::Disabled,
            &p_hash,
            async |txn| {
                let attribution = EventPoolAttribution {
                    side: PoolAttributionSide::BtcParent,
                    namespace: "btc_coinbase_tag",
                    match_kind: "test_seed",
                    matched_value: "kncminer".to_owned(),
                    pool_id: Some(pool_id),
                    pool_identity_id: None,
                    source: "test_seed",
                    confidence: PoolAttributionConfidence::High,
                    details: json!({}),
                };
                upsert_event_pool_attributions(txn, p_event, &[attribution], 1_700_000_000)
                    .await
                    .context("set parent pool attribution")?;
                Ok(())
            },
            Some(p_event),
            "test pool update",
        )
        .await?;

        let p_state = read_block_mutation_state(&client, &p_hash)
            .await?
            .context("canonical block row missing")?;
        assert_eq!(
            p_state.5, None,
            "block.bitcoin_miner_pool_id is resolved only from Bitcoin block evidence"
        );
        let canonical_pool_after = derived_competition_canonical_pool(&client, &s_hash).await?;
        assert_eq!(
            canonical_pool_after, None,
            "derived competition canonical pool follows the Bitcoin block row"
        );
        assert_source_health_matches_recompute(&client, "after pool update cascade").await?;

        Ok::<_, anyhow::Error>(())
    })
}
