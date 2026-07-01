use anyhow::Result;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_store::get_source_id;
use tokio_postgres::{Client, Row};

use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use mmm_capture::capture::RSK_PROOF_FORMAT_OPAQUE;
use mmm_capture::pool_resolver::{
    PoolIdentityRegistry, PoolSnapshotSource, RSK_MINER_ADDRESS_NAMESPACE, RskMinerEntry,
    RskMinerRegistry,
};
use mmm_capture::source_registry::RSK_SOURCE_CODE;
use mmm_producers::chains::rsk::{
    BlockOutcome, CaptureDecision, RskBlock, RskCaptureContext, capture_ready_rsk_inputs_for_test,
    load_rsk_block_fixture, prepare_rsk_capture,
};
use mmm_store::{upsert_rsk_only_pools, upsert_rsk_pool_identities, write_rsk_capture};

use crate::support::default_pool_snapshot;
use crate::support::scenario::canonical_verdict;

// ─── RSK structure/capture integration tests ─────────────────────────────────

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
async fn rsk_capture_reconciles_read_model_in_transaction() -> Result<()> {
    crate::run_mut_db_test!(client, {
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
async fn rsk_role_flip_replay_refreshes_sidecar_role() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let context =
            rsk_context_with_known_miner(&client, ConfiguredParentClassifier::Disabled).await?;

        // The SAME RSK block (same hash + height => same conflict key) is first
        // captured as canonical, then, after a reorg, re-seen as an uncle. The
        // trailing rescan window must correct its role on replay.
        let block = load_rsk_block_fixture("canonical-valid");

        let canonical_event_id =
            write_ready_rsk_capture(&mut client, &context, &block, false, None, None).await?;

        // Confirm the sidecar starts as canonical with a resolved pool identity.
        let canonical_role = rsk_evidence_role(&client, canonical_event_id).await?;
        assert_rsk_role(&canonical_role, false, None, None);
        let canonical_pool_identity = canonical_role.pool_identity_id;
        assert!(canonical_pool_identity.is_some());

        // Replay the same block as an uncle of a later canonical block.
        let replay_event_id =
            write_ready_rsk_capture(&mut client, &context, &block, true, Some(0), Some(729_005))
                .await?;
        // Same conflict key => same event row, not a second event.
        assert_eq!(replay_event_id, canonical_event_id);

        // Role columns now reflect the uncle observation...
        let uncle_role = rsk_evidence_role(&client, canonical_event_id).await?;
        assert_rsk_role(&uncle_role, true, Some(0), Some(729_005));
        // ...while the resolved pool identity is preserved (COALESCE).
        assert_eq!(uncle_role.pool_identity_id, canonical_pool_identity);

        // Still exactly one event/sidecar row for this block.
        let count: i64 = client
            .query_one(
                "SELECT COUNT(*)::BIGINT FROM rsk_merge_mining_evidence",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(count, 1);

        // Reverse direction: re-seen as canonical again => role flips back.
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

    // First-write of the canonical block.
    let first_event_id =
        write_ready_rsk_capture(client, &context, &canonical, false, None, None).await?;

    // Replay of the same canonical block must be idempotent on event id.
    let second_event_id =
        write_ready_rsk_capture(client, &context, &canonical, false, None, None).await?;
    assert_eq!(first_event_id, second_event_id);

    // Uncle write referencing canonical height 729_000.
    let uncle_event_id =
        write_ready_rsk_capture(client, &context, &uncle, true, Some(0), Some(729_000)).await?;
    assert_ne!(uncle_event_id, first_event_id);

    // merge_mining_event has exactly two rows for this source.
    let count_row = client
        .query_one(
            "SELECT COUNT(*)::BIGINT FROM merge_mining_event WHERE source_id = $1",
            &[&context.source_id()],
        )
        .await?;
    let count: i64 = count_row.get(0);
    assert_eq!(count, 2);

    // The canonical row should classify as 'unknown' (BTC PoW valid against
    // its own nBits; no Bitcoin chain proof in this slice).
    let canonical_row = client
        .query_one(
            "SELECT btc_parent_kind, pow_validates_btc_target, pow_validates_child_target, \
                    btc_parent_height \
             FROM merge_mining_event WHERE id = $1",
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
    // Both rows have side-table evidence with correct uncle context.
    let evidence_rows = client
        .query(
            "SELECT event_id, is_uncle, uncle_index, uncle_parent_height, \
                    pool_identity_id, proof_format \
             FROM rsk_merge_mining_evidence ORDER BY event_id",
            &[],
        )
        .await?;
    assert_eq!(evidence_rows.len(), 2);
    let (canonical_evidence, uncle_evidence) = (&evidence_rows[0], &evidence_rows[1]);
    assert_evidence_row_role(canonical_evidence, false, None, None);
    let canonical_pool_identity: Option<i64> = canonical_evidence.get(4);
    let canonical_proof_format: String = canonical_evidence.get(5);
    assert!(canonical_pool_identity.is_some());
    assert_eq!(canonical_proof_format, RSK_PROOF_FORMAT_OPAQUE);

    assert_evidence_row_role(uncle_evidence, true, Some(0), Some(729_000));

    // pool_identity row exists with the expected namespace + identifier.
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
    Ok(())
}

async fn write_ready_rsk_capture(
    client: &mut Client,
    context: &RskCaptureContext,
    block: &RskBlock,
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
) -> Result<i64> {
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

    // Start with an EMPTY registry: the miner address is not yet known to
    // pool_identity.
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

    // The captured proof bytes that must survive the later replay.
    let stable_merge_mining_hash = v1_inputs.evidence.merge_mining_hash.clone();
    let stable_block_hash = v1_inputs.evidence.rsk_block_hash.clone();

    let event_id = write_rsk_capture(
        client,
        context_v1.source_id(),
        &v1_inputs.payload,
        &v1_inputs.evidence,
    )
    .await?;

    // pool_identity_id starts NULL because the registry had no entry.
    let row = client
        .query_one(
            "SELECT pool_identity_id FROM rsk_merge_mining_evidence WHERE event_id = $1",
            &[&event_id],
        )
        .await?;
    let initial_identity: Option<i64> = row.get(0);
    assert_eq!(initial_identity, None);

    // Now build a richer registry that includes SECOND_MINER_HEX and replay.
    let (_, mut pool_ids_v2) = default_pool_snapshot(client).await?;
    let richer_registry = rsk_registry_with_second_miner();
    upsert_rsk_only_pools(client, &richer_registry, &mut pool_ids_v2).await?;
    let richer_identity_ids =
        upsert_rsk_pool_identities(client, &richer_registry, &pool_ids_v2).await?;
    let context_v2 =
        RskCaptureContext::from_parts(richer_registry, richer_identity_ids, pool_ids_v2, source_id);

    let v2_inputs = ready_inputs(&context_v2, &block, false, None, None);
    assert!(v2_inputs.evidence.pool_identity_id.is_some());
    let replayed_event_id = write_rsk_capture(
        client,
        context_v2.source_id(),
        &v2_inputs.payload,
        &v2_inputs.evidence,
    )
    .await?;
    assert_eq!(replayed_event_id, event_id);

    // pool_identity_id has progressed NULL -> resolved.
    let row = client
        .query_one(
            "SELECT pool_identity_id, merge_mining_hash, rsk_block_hash \
             FROM rsk_merge_mining_evidence WHERE event_id = $1",
            &[&event_id],
        )
        .await?;
    let resolved_identity: Option<i64> = row.get(0);
    let stored_merge_mining_hash: Vec<u8> = row.get(1);
    let stored_block_hash: Vec<u8> = row.get(2);
    assert!(resolved_identity.is_some());
    // Stable captured bytes are preserved on replay.
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
            "SELECT is_uncle, uncle_index, uncle_parent_height, pool_identity_id \
             FROM rsk_merge_mining_evidence WHERE event_id = $1",
            &[&event_id],
        )
        .await?;
    Ok(RskEvidenceRole {
        is_uncle: row.get(0),
        uncle_index: row.get(1),
        uncle_parent_height: row.get(2),
        pool_identity_id: row.get(3),
    })
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

fn assert_evidence_row_role(
    row: &Row,
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
) {
    assert_eq!(row.get::<_, bool>(1), is_uncle);
    assert_eq!(row.get::<_, Option<i32>>(2), uncle_index);
    assert_eq!(row.get::<_, Option<i32>>(3), uncle_parent_height);
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
