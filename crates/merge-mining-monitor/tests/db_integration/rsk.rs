use anyhow::{Context, Result};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_store::get_source_id;
use tokio_postgres::Client;

use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use mmm_capture::capture::{
    HistoricalEventProvenance, PoolAttributionSide, RSK_PROOF_FORMAT_OPAQUE,
};
use mmm_capture::pool_resolver::{
    PoolIdentityRegistry, PoolSnapshotSource, RSK_MINER_ADDRESS_NAMESPACE, RskMinerEntry,
    RskMinerRegistry,
};
use mmm_capture::source_registry::RSK_SOURCE_CODE;
use mmm_producers::chains::rsk::{
    BlockOutcome, CaptureDecision, RskBlock, RskCaptureContext, capture_ready_rsk_inputs_for_test,
    capture_rsk_block_for_test, load_rsk_block_fixture, prepare_rsk_capture,
};
use mmm_store::{
    CaptureWrite, upsert_rsk_only_pools, upsert_rsk_pool_identities, write_rsk_capture,
};

use crate::support::db::observation_prev_hash;
use crate::support::scenario::canonical_verdict;
use crate::support::{default_pool_snapshot, event_id_of};

const KNOWN_MINER_HEX: &str = "12d3178a62ef1f520944534ed04504609f7307a1";
const SECOND_MINER_HEX: &str = "4e5dabc28e4a0f5e5b19fcb56b28c5a1989352c1";

#[tokio::test]
async fn writes_rsk_canonical_and_uncle_idempotently_with_pool_identity() -> Result<()> {
    crate::run_mut_db_test!(client, { run_rsk_canonical_and_uncle(&mut client).await })
}

#[tokio::test]
async fn rsk_pool_identity_progresses_null_to_resolved_preserving_proof_bytes() -> Result<()> {
    crate::run_mut_db_test!(client, {
        run_rsk_pool_identity_progression(&mut client).await
    })
}

#[tokio::test]
async fn the_rsk_sidecar_supplies_the_observation_miner_without_child_attribution() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = load_rsk_block_fixture("canonical-valid");
        let context =
            rsk_context_with_known_miner(&client, ConfiguredParentClassifier::Disabled).await?;
        let mut inputs = ready_inputs(&context, &block, false, None, None);
        // Historical imports carry only Bitcoin-parent attributions.
        inputs
            .payload
            .pool_attributions
            .retain(|row| row.side == PoolAttributionSide::BtcParent);
        let event_id = event_id_of(
            write_rsk_capture(
                &mut client,
                context.source_id(),
                &inputs.payload,
                &inputs.evidence,
            )
            .await?,
        )?;
        let identifiers: Vec<String> = client
            .query(
                "SELECT ci.identifier FROM merge_mining_event e \
                 JOIN child_block_observation o ON o.id = e.observation_id \
                 JOIN child_miner_identity ci ON ci.id = ANY (o.child_identity_ids) \
                 WHERE e.id = $1",
                &[&event_id],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(identifiers, vec![KNOWN_MINER_HEX.to_owned()]);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn a_refused_rsk_uncle_keeps_its_chain_role_on_the_child_side_observation() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let context =
            rsk_context_with_known_miner(&client, ConfiguredParentClassifier::Disabled).await?;
        // The fixture parent is a share whose all-zero prev is the seeded genesis header. Give
        // it a prev the cache does not hold: its bits then match no Bitcoin epoch, so the
        // lineage gate refuses the parent.
        let mut uncle = load_rsk_block_fixture("uncle-valid");
        uncle
            .bitcoin_merged_mining_header
            .as_mut()
            .context("uncle fixture carries a parent header")?
            .replace_range(10..74, &"11".repeat(32));
        uncle.parent_hash = Some(format!("0x{}", "22".repeat(32)));
        let inputs = ready_inputs(&context, &uncle, true, Some(0), Some(729_000));
        let outcome = capture_ready_rsk_inputs_for_test(&mut client, &context, inputs).await?;
        assert_eq!(outcome, BlockOutcome::NonBitcoinParent);
        assert_child_only_uncle_row(&client, context.source_id(), "non_bitcoin_parent").await?;
        let uncle_hash = hex::decode(uncle.hash.trim_start_matches("0x"))?;
        assert_eq!(
            observation_prev_hash(&client, context.source_id(), &uncle_hash).await?,
            Some(vec![0x22; 32])
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn an_rsk_uncle_without_a_complete_parent_header_is_kept_with_an_unverified_parent()
-> Result<()> {
    crate::run_mut_db_test!(client, {
        let context =
            rsk_context_with_known_miner(&client, ConfiguredParentClassifier::Disabled).await?;
        // A pre-RSKIP-92 fallback signature instead of an 80-byte parent header.
        let mut uncle = load_rsk_block_fixture("uncle-valid");
        uncle.bitcoin_merged_mining_header = Some("0x0100".to_owned());
        let outcome =
            capture_rsk_block_for_test(&mut client, &context, &uncle, true, Some(0), Some(729_000))
                .await?;
        assert_eq!(outcome, BlockOutcome::NoParentHeaderSkipped);
        assert_child_only_uncle_row(&client, context.source_id(), "unverified_parent").await?;
        Ok::<_, anyhow::Error>(())
    })
}

/// The single observation a child-only `uncle-valid` capture leaves, with its RSK facts.
async fn assert_child_only_uncle_row(client: &Client, source_id: i64, kind: &str) -> Result<()> {
    let row = client
        .query_one(
            "SELECT observation_kind, child_chain_role, uncle_index, uncle_parent_height, \
                    child_difficulty_reported::text, btc_parent_header_hash IS NULL, \
                    cardinality(child_identity_ids), child_block_time, child_pool_id IS NOT NULL \
             FROM child_block_observation WHERE source_id = $1",
            &[&source_id],
        )
        .await?;
    assert_eq!(row.get::<_, String>(0), kind);
    assert_eq!(row.get::<_, Option<String>>(1).as_deref(), Some("uncle"));
    assert_eq!(row.get::<_, Option<i32>>(2), Some(0));
    assert_eq!(row.get::<_, Option<i32>>(3), Some(729_000));
    assert_eq!(row.get::<_, Option<String>>(4).as_deref(), Some("4096"));
    assert!(row.get::<_, bool>(5), "no parent field is stored");
    assert_eq!(row.get::<_, i32>(6), 1, "the miner identity is kept");
    assert_eq!(row.get::<_, Option<i64>>(7), Some(0x6553_f101));
    assert!(
        row.get::<_, bool>(8),
        "the registry miner's pool is the child pool"
    );
    Ok(())
}

#[tokio::test]
async fn contradictory_rsk_sidecar_rolls_back_historical_provenance() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = load_rsk_block_fixture("canonical-valid");
        let context =
            rsk_context_with_known_miner(&client, ConfiguredParentClassifier::Disabled).await?;
        let inputs = ready_inputs(&context, &block, false, None, None);
        let event_id = event_id_of(
            write_rsk_capture(
                &mut client,
                context.source_id(),
                &inputs.payload,
                &inputs.evidence,
            )
            .await?,
        )?;
        let stored_miner = inputs.evidence.rsk_miner.clone();

        for (source_row_number, corrupt_miner) in [(1_i64, true), (2_i64, false)] {
            let mut payload = inputs.payload.clone();
            payload.historical_provenance = Some(HistoricalEventProvenance {
                publication_ref: "operator-csv".to_owned(),
                chain: "rsk".to_owned(),
                source_kind: "full_inventory".to_owned(),
                source_path: "fixture.csv".to_owned(),
                source_row_number,
                artifact_scope: "full_classifier_inventory".to_owned(),
                provenance: "fixture".to_owned(),
                classification: "canonical".to_owned(),
                btc_height: Some(730_000),
                validation_status: Some("VALID".to_owned()),
                btc_stale_relevance: None,
                relevance_reason: Some("canonical_parent".to_owned()),
            });
            let mut contradictory = inputs.evidence.clone();
            if corrupt_miner {
                contradictory.rsk_miner[0] ^= 0xff;
            } else {
                contradictory.merge_mining_hash[0] ^= 0xff;
            }

            let error =
                write_rsk_capture(&mut client, context.source_id(), &payload, &contradictory)
                    .await
                    .expect_err("contradictory immutable RSK evidence must fail");
            let expected = if corrupt_miner {
                "contradict the stored miner"
            } else {
                "immutable sidecar fields"
            };
            assert!(format!("{error:#}").contains(expected), "{error:#}");
        }

        let row = client
            .query_one(
                "SELECT ci.identifier, count(p.event_id)::bigint \
                 FROM merge_mining_event e \
                 JOIN child_block_observation o ON o.id = e.observation_id \
                 JOIN child_miner_identity ci \
                   ON ci.id = ANY (o.child_identity_ids) AND ci.namespace = 'rsk_miner_address' \
                 LEFT JOIN historical_event_provenance p ON p.event_id = e.id \
                 WHERE e.id = $1 \
                 GROUP BY ci.identifier",
                &[&event_id],
            )
            .await?;
        assert_eq!(row.get::<_, String>(0), hex::encode(stored_miner));
        assert_eq!(
            row.get::<_, i64>(1),
            0,
            "failed sidecars attach no provenance"
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rsk_capture_reconciles_read_model_in_transaction() -> Result<()> {
    crate::run_mut_db_test!(client, {
        crate::support::seed_synthetic_fixture_history(&client).await?;
        let block = load_rsk_block_fixture("canonical-valid");
        let header = btc_header_from_fixture(&block);
        let parent_hash = header.block_hash().to_byte_array().to_vec();
        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            canonical_verdict(&header, 730_000),
        ));
        let context = rsk_context_with_known_miner(&client, classifier).await?;
        let inputs = ready_inputs(&context, &block, false, None, None);

        let outcome = capture_ready_rsk_inputs_for_test(&mut client, &context, inputs).await?;
        assert_eq!(outcome, BlockOutcome::Written);

        let row = client
            .query_one(
                "SELECT event.btc_parent_kind, event.btc_parent_height, \
                        block.kind, evidence.proof_format \
                 FROM merge_mining_event AS event \
                 JOIN rsk_merge_mining_evidence AS evidence \
                   ON evidence.event_id = event.id \
                 JOIN block \
                   ON block.btc_header_hash = event.btc_parent_header_hash \
                 WHERE event.btc_parent_header_hash = $1",
                &[&parent_hash],
            )
            .await?;
        assert_eq!(row.get::<_, String>(0), "canonical");
        assert_eq!(row.get::<_, Option<i32>>(1), Some(730_000));
        assert_eq!(row.get::<_, String>(2), "canonical");
        assert_eq!(row.get::<_, String>(3), RSK_PROOF_FORMAT_OPAQUE);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn writes_pre_floor_full_header_event() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // RSK height 112,829 is below the 139,999 acquisition floor; capture/write
        // must not height-gate it, and its un-solved BTC nBits classifies as `near`.
        let block = load_rsk_block_fixture("canonical-pre-floor-full-header");
        let header = btc_header_from_fixture(&block);
        let parent_hash = header.block_hash().to_byte_array().to_vec();
        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            canonical_verdict(&header, 490_000),
        ));
        let context = rsk_context_with_known_miner(&client, classifier).await?;
        let inputs = ready_inputs(&context, &block, false, None, None);
        let child_block_hash = inputs
            .payload
            .child_block_hash
            .clone()
            .expect("RSK fixture carries an exact child-block identity");

        let outcome = capture_ready_rsk_inputs_for_test(&mut client, &context, inputs).await?;
        assert_eq!(outcome, BlockOutcome::Written);

        // A near is recorded as its observation, with no event.
        let row = client
            .query_one(
                "SELECT o.child_height, o.observation_kind, o.btc_parent_header_hash, \
                        EXISTS (SELECT 1 FROM merge_mining_event e \
                                 WHERE e.source_id = o.source_id \
                                   AND e.child_block_hash = o.child_block_hash) \
                 FROM child_block_observation o \
                 WHERE o.source_id = $1 AND o.child_block_hash = $2",
                &[&context.source_id(), &child_block_hash],
            )
            .await?;
        assert_eq!(row.get::<_, Option<i32>>(0), Some(112_829));
        assert_eq!(row.get::<_, String>(1), "near");
        assert_eq!(row.get::<_, Vec<u8>>(2), parent_hash);
        assert!(!row.get::<_, bool>(3));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rsk_role_flip_replay_refreshes_sidecar_role() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let context =
            rsk_context_with_known_miner(&client, ConfiguredParentClassifier::Disabled).await?;
        let block = load_rsk_block_fixture("canonical-valid");

        let canonical_event_id = event_id_of(
            write_ready_rsk_capture(&mut client, &context, &block, false, None, None).await?,
        )?;
        let canonical_role = rsk_evidence_role(&client, canonical_event_id).await?;
        assert_rsk_role(&canonical_role, false, None, None);
        let canonical_pool_identity = canonical_role.pool_identity_id;
        assert!(canonical_pool_identity.is_some());

        let replay_event_id = event_id_of(
            write_ready_rsk_capture(&mut client, &context, &block, true, Some(0), Some(729_005))
                .await?,
        )?;
        assert_eq!(replay_event_id, canonical_event_id);

        let uncle_role = rsk_evidence_role(&client, canonical_event_id).await?;
        assert_rsk_role(&uncle_role, true, Some(0), Some(729_005));
        assert_eq!(uncle_role.pool_identity_id, canonical_pool_identity);

        let count: i64 = client
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM rsk_merge_mining_evidence",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(count, 1);

        write_ready_rsk_capture(&mut client, &context, &block, false, None, None).await?;
        assert_rsk_role(
            &rsk_evidence_role(&client, canonical_event_id).await?,
            false,
            None,
            None,
        );

        Ok::<_, anyhow::Error>(())
    })
}

async fn rsk_context_with_known_miner(
    client: &Client,
    parent_classifier: ConfiguredParentClassifier,
) -> Result<RskCaptureContext> {
    let (_, mut pool_ids_by_slug) = default_pool_snapshot(client).await?;
    let registry = rsk_registry_with_known_miner();
    upsert_rsk_only_pools(client, &registry, &mut pool_ids_by_slug).await?;
    let identity_ids = upsert_rsk_pool_identities(client, &registry, &pool_ids_by_slug).await?;
    let source_id = get_source_id(client, RSK_SOURCE_CODE).await?;
    Ok(RskCaptureContext::from_parts_with_classifier(
        registry,
        identity_ids,
        pool_ids_by_slug,
        source_id,
        parent_classifier,
    ))
}

async fn run_rsk_canonical_and_uncle(client: &mut Client) -> Result<()> {
    let context =
        rsk_context_with_known_miner(client, ConfiguredParentClassifier::Disabled).await?;

    let canonical = load_rsk_block_fixture("canonical-valid");
    let uncle = load_rsk_block_fixture("uncle-valid");

    let first_event_id = event_id_of(
        write_ready_rsk_capture(client, &context, &canonical, false, None, None).await?,
    )?;
    let second_event_id = event_id_of(
        write_ready_rsk_capture(client, &context, &canonical, false, None, None).await?,
    )?;
    assert_eq!(first_event_id, second_event_id);

    // The uncle's parent misses Bitcoin's target: it is a near, recorded as its observation.
    assert!(matches!(
        write_ready_rsk_capture(client, &context, &uncle, true, Some(0), Some(729_000)).await?,
        CaptureWrite::Near { .. }
    ));

    let count_row = client
        .query_one(
            "SELECT COUNT(*)::BIGINT FROM merge_mining_event WHERE source_id = $1",
            &[&context.source_id()],
        )
        .await?;
    let count: i64 = count_row.get(0);
    assert_eq!(count, 1);

    let canonical_row = client
        .query_one(
            "SELECT e.btc_parent_kind, e.pow_validates_btc_target, o.pow_validates_child_target, \
                    e.btc_parent_height \
             FROM merge_mining_event e \
             JOIN child_block_observation o ON o.id = e.observation_id WHERE e.id = $1",
            &[&first_event_id],
        )
        .await?;
    let kind: String = canonical_row.get(0);
    let pow_btc: bool = canonical_row.get(1);
    let pow_child: Option<bool> = canonical_row.get(2);
    let parent_height: Option<i32> = canonical_row.get(3);
    assert_eq!(kind, "unknown");
    assert!(pow_btc);
    assert_eq!(pow_child, None);
    assert_eq!(parent_height, None);
    let child_pool_id: Option<i64> = client
        .query_one(
            "SELECT pool_id \
             FROM event_pool_attribution \
             WHERE event_id = $1 AND side = 'child_block'",
            &[&first_event_id],
        )
        .await?
        .get(0);
    assert!(child_pool_id.is_some());

    assert_rsk_evidence_rows_and_identity(client).await?;

    Ok(())
}

async fn assert_rsk_evidence_rows_and_identity(client: &Client) -> Result<()> {
    let evidence_rows = client
        .query(
            "SELECT event_id, proof_format FROM rsk_merge_mining_evidence ORDER BY event_id",
            &[],
        )
        .await?;
    assert_eq!(
        evidence_rows.len(),
        1,
        "only the canonical block has an event"
    );
    let canonical_event_id: i64 = evidence_rows[0].get(0);
    assert_eq!(
        evidence_rows[0].get::<_, String>(1),
        RSK_PROOF_FORMAT_OPAQUE
    );
    let canonical_pool_identity = miner_pool_identity(client, canonical_event_id).await?;
    assert!(canonical_pool_identity.is_some());

    let identity_row = client
        .query_one(
            "SELECT namespace, identifier FROM pool_identity WHERE id = $1",
            &[&canonical_pool_identity.unwrap()],
        )
        .await?;
    let namespace: String = identity_row.get(0);
    let identifier: String = identity_row.get(1);
    assert_eq!(namespace, RSK_MINER_ADDRESS_NAMESPACE);
    assert_eq!(identifier, KNOWN_MINER_HEX);

    let observations = client
        .query(
            "SELECT o.child_chain_role, o.uncle_index, o.uncle_parent_height, \
                    o.child_difficulty_reported::text, o.observation_kind, o.child_pool_id, \
                    (SELECT ci.identifier FROM child_miner_identity ci \
                      WHERE ci.id = ANY (o.child_identity_ids)), \
                    (SELECT a.pool_id FROM event_pool_attribution a \
                       JOIN merge_mining_event e ON e.id = a.event_id \
                      WHERE e.observation_id = o.id AND a.side = 'child_block') \
             FROM child_block_observation o \
             ORDER BY o.child_chain_role",
            &[],
        )
        .await?;
    let roles = observations
        .iter()
        .map(|row| {
            (
                row.get::<_, Option<String>>(0),
                row.get::<_, Option<i32>>(1),
                row.get::<_, Option<i32>>(2),
                row.get::<_, String>(4),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        roles,
        vec![
            (
                Some("canonical".to_owned()),
                None,
                None,
                "evidence".to_owned()
            ),
            (
                Some("uncle".to_owned()),
                Some(0),
                Some(729_000),
                "near".to_owned()
            ),
        ]
    );
    assert_eq!(
        observations[0].get::<_, Option<String>>(3).as_deref(),
        Some("4096")
    );
    let canonical_pool: Option<i64> = observations[0].get(5);
    assert!(canonical_pool.is_some());
    assert_eq!(
        observations[0].get::<_, Option<i64>>(7),
        canonical_pool,
        "the evidence observation's pool follows its event"
    );
    assert_eq!(
        observations[1].get::<_, Option<i64>>(5),
        canonical_pool,
        "the near observation's pool comes from its own attributions"
    );
    for observation in &observations {
        assert_eq!(
            observation.get::<_, Option<String>>(6).as_deref(),
            Some(KNOWN_MINER_HEX)
        );
    }
    Ok(())
}

async fn write_ready_rsk_capture(
    client: &mut Client,
    context: &RskCaptureContext,
    block: &RskBlock,
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
) -> Result<CaptureWrite> {
    let inputs = ready_inputs(context, block, is_uncle, uncle_index, uncle_parent_height);
    write_rsk_capture(
        client,
        context.source_id(),
        &inputs.payload,
        &inputs.evidence,
    )
    .await
}

async fn run_rsk_pool_identity_progression(client: &mut Client) -> Result<()> {
    let (_, mut pool_ids_by_slug) = default_pool_snapshot(client).await?;

    let empty_registry = empty_rsk_registry();
    upsert_rsk_only_pools(client, &empty_registry, &mut pool_ids_by_slug).await?;
    let empty_identity_ids =
        upsert_rsk_pool_identities(client, &empty_registry, &pool_ids_by_slug).await?;
    let source_id = mmm_store::get_source_id(client, RSK_SOURCE_CODE).await?;
    let context_v1 = RskCaptureContext::from_parts(
        empty_registry,
        empty_identity_ids,
        pool_ids_by_slug.clone(),
        source_id,
    );

    let block = load_rsk_block_fixture("uncle-second-miner");
    let v1_inputs = ready_inputs(&context_v1, &block, false, None, None);

    let stable_merge_mining_hash = v1_inputs.evidence.merge_mining_hash.clone();
    let stable_block_hash = v1_inputs.evidence.rsk_block_hash.clone();

    let event_id = event_id_of(
        write_rsk_capture(
            client,
            context_v1.source_id(),
            &v1_inputs.payload,
            &v1_inputs.evidence,
        )
        .await?,
    )?;

    assert_eq!(miner_pool_identity(client, event_id).await?, None);

    let (_, mut pool_ids_v2) = default_pool_snapshot(client).await?;
    let richer_registry = rsk_registry_with_second_miner();
    upsert_rsk_only_pools(client, &richer_registry, &mut pool_ids_v2).await?;
    let richer_identity_ids =
        upsert_rsk_pool_identities(client, &richer_registry, &pool_ids_v2).await?;
    let context_v2 =
        RskCaptureContext::from_parts(richer_registry, richer_identity_ids, pool_ids_v2, source_id);

    let v2_inputs = ready_inputs(&context_v2, &block, false, None, None);
    let replayed_event_id = event_id_of(
        write_rsk_capture(
            client,
            context_v2.source_id(),
            &v2_inputs.payload,
            &v2_inputs.evidence,
        )
        .await?,
    )?;
    assert_eq!(replayed_event_id, event_id);

    let row = client
        .query_one(
            "SELECT r.merge_mining_hash, o.child_block_hash \
             FROM rsk_merge_mining_evidence r \
             JOIN merge_mining_event e ON e.id = r.event_id \
             JOIN child_block_observation o ON o.id = e.observation_id \
             WHERE r.event_id = $1",
            &[&event_id],
        )
        .await?;
    let stored_merge_mining_hash: Vec<u8> = row.get(0);
    let stored_block_hash: Vec<u8> = row.get(1);
    assert!(miner_pool_identity(client, event_id).await?.is_some());
    assert_eq!(stored_merge_mining_hash, stable_merge_mining_hash);
    assert_eq!(stored_block_hash, stable_block_hash);

    Ok(())
}

fn ready_inputs(
    context: &RskCaptureContext,
    block: &RskBlock,
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
) -> mmm_producers::chains::rsk::RskCaptureInputs {
    match prepare_rsk_capture(
        context,
        block,
        is_uncle,
        uncle_index,
        uncle_parent_height,
        1_700_000_000,
    )
    .unwrap()
    {
        CaptureDecision::Ready(inputs) => *inputs,
        other => panic!("expected CaptureDecision::Ready, got {other:?}"),
    }
}

fn btc_header_from_fixture(block: &RskBlock) -> Header {
    let header_hex = block
        .bitcoin_merged_mining_header
        .as_deref()
        .expect("fixture includes a Bitcoin merged-mining header");
    let header_bytes =
        hex::decode(header_hex.trim_start_matches("0x")).expect("fixture header hex decodes");
    deserialize(&header_bytes).expect("fixture header is an 80-byte Bitcoin header")
}

struct RskEvidenceRole {
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
    pool_identity_id: Option<i64>,
}

async fn rsk_evidence_role(client: &Client, event_id: i64) -> Result<RskEvidenceRole> {
    let row = client
        .query_one(
            "SELECT o.child_chain_role = 'uncle', o.uncle_index, o.uncle_parent_height \
             FROM merge_mining_event e \
             JOIN child_block_observation o ON o.id = e.observation_id \
             WHERE e.id = $1",
            &[&event_id],
        )
        .await?;
    Ok(RskEvidenceRole {
        is_uncle: row.get(0),
        uncle_index: row.get(1),
        uncle_parent_height: row.get(2),
        pool_identity_id: miner_pool_identity(client, event_id).await?,
    })
}

/// The pool identity the event's RSK miner attribution resolved to.
async fn miner_pool_identity(client: &Client, event_id: i64) -> Result<Option<i64>> {
    Ok(client
        .query_one(
            "SELECT pool_identity_id FROM event_pool_attribution \
             WHERE event_id = $1 AND namespace = $2",
            &[&event_id, &RSK_MINER_ADDRESS_NAMESPACE],
        )
        .await?
        .get(0))
}

fn assert_rsk_role(
    role: &RskEvidenceRole,
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
) {
    assert_eq!(role.is_uncle, is_uncle);
    assert_eq!(role.uncle_index, uncle_index);
    assert_eq!(role.uncle_parent_height, uncle_parent_height);
}

fn rsk_registry_with_known_miner() -> PoolIdentityRegistry {
    rsk_registry(vec![RskMinerEntry {
        miner_address: KNOWN_MINER_HEX.to_owned(),
        pool_slug: "f2pool".to_owned(),
        pool_canonical_name: "F2Pool".to_owned(),
    }])
}

fn empty_rsk_registry() -> PoolIdentityRegistry {
    rsk_registry(vec![])
}

fn rsk_registry_with_second_miner() -> PoolIdentityRegistry {
    rsk_registry(vec![RskMinerEntry {
        miner_address: SECOND_MINER_HEX.to_owned(),
        pool_slug: "antpool".to_owned(),
        pool_canonical_name: "AntPool".to_owned(),
    }])
}

fn rsk_registry(entries: Vec<RskMinerEntry>) -> PoolIdentityRegistry {
    PoolIdentityRegistry::from_rsk_registry(RskMinerRegistry {
        schema_version: 1,
        generated_at: "test".to_owned(),
        scope: "test".to_owned(),
        source: PoolSnapshotSource {
            name: "test".to_owned(),
            upstream_url: None,
            license: None,
            notes: None,
        },
        entries,
    })
    .unwrap()
}
