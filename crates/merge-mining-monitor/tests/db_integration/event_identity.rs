use anyhow::Result;
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use mmm_capture::capture::{
    ClassificationProof, MergeMiningEventPayload, ResolvedPoolAttributions, build_event_payload,
};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::{EventWriteDisposition, get_source_id, upsert_merge_mining_event};

use crate::support::parse_auxpow_fixture;

fn exact_payload(child_height: i32, observed_at: i64) -> Result<MergeMiningEventPayload> {
    let parsed = parse_auxpow_fixture("500000-valid-parent")?;
    build_event_payload(
        &parsed,
        Some(child_height),
        ResolvedPoolAttributions::default(),
        ClassificationProof::default(),
        observed_at,
    )
}

fn partial_payload(child_height: i32, observed_at: i64) -> Result<MergeMiningEventPayload> {
    let mut payload = exact_payload(child_height, observed_at)?;
    payload.child_block_hash = None;
    payload.child_header_bytes = None;
    payload.child_block_time = None;
    payload.child_nbits = None;
    Ok(payload)
}

#[tokio::test]
async fn migration_supports_nullable_child_evidence_columns() -> Result<()> {
    crate::run_db_test!(client, {
        let rows = client
            .query(
                "SELECT column_name, is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = current_schema() \
                   AND table_name = 'merge_mining_event' \
                   AND column_name = ANY($1) \
                 ORDER BY column_name",
                &[&vec![
                    "child_block_hash",
                    "child_block_time",
                    "child_header_bytes",
                    "child_height",
                    "child_nbits",
                ]],
            )
            .await?;
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|row| row.get::<_, String>(1) == "YES"));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn migration_rejects_legacy_duplicate_exact_identities_before_altering_schema() -> Result<()>
{
    let (client, schema) =
        crate::support::db::new_test_db_through("0006_add_known_stale_block").await?;
    let test_result = async {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let parsed = parse_auxpow_fixture("500000-valid-parent")?;
        let parent_hash = parsed
            .parent_header
            .header
            .block_hash()
            .to_byte_array()
            .to_vec();
        let parent_prev_hash = parsed
            .parent_header
            .header
            .prev_blockhash
            .to_byte_array()
            .to_vec();
        let parent_header = serialize(&parsed.parent_header.header);
        let parent_time = i64::from(parsed.parent_header.header.time);
        let child_hash = vec![0x41_u8; 32];

        for child_height in [10, 11] {
            client
                .execute(
                    "INSERT INTO merge_mining_event ( \
                        source_id, child_height, child_block_hash, child_block_time, \
                        btc_parent_header_hash, btc_parent_prev_header_hash, \
                        btc_parent_header_bytes, btc_parent_header_time, btc_parent_kind, \
                        pow_validates_btc_target, discovered_at, confirmed_at \
                     ) VALUES ($1, $2, $3, $4, $5, $6, $7, $4, 'unknown', FALSE, $8, $8)",
                    &[
                        &source_id,
                        &child_height,
                        &child_hash,
                        &parent_time,
                        &parent_hash,
                        &parent_prev_hash,
                        &parent_header,
                        &2_000_i64,
                    ],
                )
                .await?;
        }

        let migration = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../migrations/0007_support_partial_child_evidence.sql"),
        )?;
        let error = client
            .batch_execute(&migration)
            .await
            .expect_err("duplicate exact identities must block migration 0007");
        let database_error = error.as_db_error().expect("Postgres error detail");
        assert_eq!(
            database_error.message(),
            "migration 0007 found 1 duplicate exact child identities"
        );
        assert!(
            database_error
                .hint()
                .unwrap_or_default()
                .contains("duplicate-identity audit query")
        );

        let nullable: String = client
            .query_one(
                "SELECT is_nullable \
                 FROM information_schema.columns \
                 WHERE table_schema = current_schema() \
                   AND table_name = 'merge_mining_event' \
                   AND column_name = 'child_height'",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(nullable, "NO", "failed preflight must precede schema edits");
        Ok::<_, anyhow::Error>(())
    }
    .await;
    crate::support::db::teardown_test_db(&client, &schema, test_result).await
}

#[tokio::test]
async fn exact_and_partial_observations_are_idempotent() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        for payload in [exact_payload(1_000, 2_000)?, partial_payload(1_001, 2_001)?] {
            let first = upsert_merge_mining_event(&client, source_id, &payload).await?;
            let second = upsert_merge_mining_event(&client, source_id, &payload).await?;

            assert_eq!(first.disposition, EventWriteDisposition::Inserted);
            assert_eq!(second.disposition, EventWriteDisposition::Updated);
            assert_eq!(first.event_id, second.event_id);
        }
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event WHERE source_id = $1",
                &[&source_id],
            )
            .await?
            .get(0);
        assert_eq!(count, 2);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn exact_observation_satisfies_later_partial_observation() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let exact = exact_payload(1_002, 2_002)?;
        let partial = partial_payload(1_002, 2_003)?;

        let exact = upsert_merge_mining_event(&client, source_id, &exact).await?;
        let partial = upsert_merge_mining_event(&client, source_id, &partial).await?;

        assert_eq!(exact.disposition, EventWriteDisposition::Inserted);
        assert_eq!(
            partial.disposition,
            EventWriteDisposition::SatisfiedByExistingExact
        );
        assert_eq!(partial.event_id, exact.event_id);
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event WHERE source_id = $1",
                &[&source_id],
            )
            .await?
            .get(0);
        assert_eq!(count, 1);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn partial_observation_fills_and_checks_an_exact_events_child_header() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let mut exact = exact_payload(1_010, 2_010)?;
        let authenticated_header = exact
            .child_header_bytes
            .take()
            .expect("fixture child header");
        let exact_outcome = upsert_merge_mining_event(&client, source_id, &exact).await?;

        let mut partial = partial_payload(1_010, 2_011)?;
        partial.child_header_bytes = Some(authenticated_header.clone());
        let partial_outcome = upsert_merge_mining_event(&client, source_id, &partial).await?;
        assert_eq!(
            partial_outcome.disposition,
            EventWriteDisposition::SatisfiedByExistingExact
        );
        assert_eq!(partial_outcome.event_id, exact_outcome.event_id);
        let stored: Option<Vec<u8>> = client
            .query_one(
                "SELECT child_header_bytes FROM merge_mining_event WHERE id = $1",
                &[&exact_outcome.event_id],
            )
            .await?
            .get(0);
        assert_eq!(stored, Some(authenticated_header));

        let mut contradictory = partial;
        contradictory.child_header_bytes = Some(vec![0x42; 80]);
        let error = upsert_merge_mining_event(&client, source_id, &contradictory)
            .await
            .expect_err("a contradictory child header must be rejected");
        assert!(
            error
                .to_string()
                .contains("partial child evidence contradicts existing exact observation")
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn exact_observation_promotes_existing_partial_observation() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let mut partial = partial_payload(1_003, 2_003)?;
        partial.btc_parent_coinbase_txid = None;
        partial.btc_parent_coinbase_script = None;
        partial.btc_parent_coinbase_outputs = None;
        partial.aux_merkle_proof = None;
        let exact = exact_payload(1_003, 2_004)?;

        let partial = upsert_merge_mining_event(&client, source_id, &partial).await?;
        let exact_outcome = upsert_merge_mining_event(&client, source_id, &exact).await?;

        assert_eq!(partial.disposition, EventWriteDisposition::Inserted);
        assert_eq!(exact_outcome.disposition, EventWriteDisposition::Promoted);
        assert_eq!(exact_outcome.event_id, partial.event_id);
        let row = client
            .query_one(
                "SELECT child_block_hash, child_header_bytes, child_block_time, child_nbits, \
                        btc_parent_coinbase_txid, btc_parent_coinbase_script, \
                        btc_parent_coinbase_outputs, aux_merkle_proof \
                 FROM merge_mining_event WHERE id = $1",
                &[&exact_outcome.event_id],
            )
            .await?;
        assert_eq!(row.get::<_, Option<Vec<u8>>>(0), exact.child_block_hash);
        assert_eq!(row.get::<_, Option<Vec<u8>>>(1), exact.child_header_bytes);
        assert_eq!(row.get::<_, Option<i64>>(2), exact.child_block_time);
        assert_eq!(
            row.get::<_, Option<i64>>(3),
            exact.child_nbits.map(i64::from)
        );
        assert_eq!(
            row.get::<_, Option<Vec<u8>>>(4),
            exact.btc_parent_coinbase_txid
        );
        assert_eq!(
            row.get::<_, Option<Vec<u8>>>(5),
            exact.btc_parent_coinbase_script
        );
        assert_eq!(
            row.get::<_, Option<Vec<u8>>>(6),
            exact.btc_parent_coinbase_outputs
        );
        assert_eq!(row.get::<_, Option<Vec<u8>>>(7), exact.aux_merkle_proof);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn partial_observation_rejects_ambiguous_exact_matches() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let mut first = exact_payload(1_004, 2_004)?;
        first.child_header_bytes = None;
        first.child_nbits = None;
        let mut second = first.clone();
        second.child_block_hash = Some(vec![0x42; 32]);

        upsert_merge_mining_event(&client, source_id, &first).await?;
        upsert_merge_mining_event(&client, source_id, &second).await?;

        let error = upsert_merge_mining_event(&client, source_id, &partial_payload(1_004, 2_005)?)
            .await
            .expect_err("two exact matches must make a partial observation ambiguous");
        assert!(
            error
                .to_string()
                .contains("ambiguously matches 2 exact events")
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn exact_observation_rejects_contradictory_child_evidence() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let first = exact_payload(1_005, 2_005)?;
        let mut contradictory = first.clone();
        contradictory.child_block_time = first.child_block_time.map(|time| time + 1);

        upsert_merge_mining_event(&client, source_id, &first).await?;
        let error = upsert_merge_mining_event(&client, source_id, &contradictory)
            .await
            .expect_err("contradictory non-null evidence must be rejected");
        assert!(error.to_string().contains("contradicts stored evidence"));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn exact_observation_rejects_a_different_parent_for_the_same_child_hash() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let first = exact_payload(1_009, 2_009)?;
        let mut contradictory = first.clone();
        let other = parse_auxpow_fixture("500001-near-parent")?;
        contradictory.btc_parent_header_hash = other
            .parent_header
            .header
            .block_hash()
            .to_byte_array()
            .to_vec();
        contradictory.btc_parent_prev_header_hash = other
            .parent_header
            .header
            .prev_blockhash
            .to_byte_array()
            .to_vec();
        contradictory.btc_parent_header_bytes = serialize(&other.parent_header.header);
        contradictory.btc_parent_header_time = i64::from(other.parent_header.header.time);

        upsert_merge_mining_event(&client, source_id, &first).await?;
        let error = upsert_merge_mining_event(&client, source_id, &contradictory)
            .await
            .expect_err("one child ledger identity cannot select two parent observations");
        assert!(error.to_string().contains("contradicts stored evidence"));
        let count: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event WHERE source_id = $1",
                &[&source_id],
            )
            .await?
            .get(0);
        assert_eq!(count, 1);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn exact_partial_refinement_rejects_contradictory_child_evidence() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;

        let exact_after_partial = exact_payload(1_007, 2_007)?;
        let mut partial_first = partial_payload(1_007, 2_006)?;
        partial_first.child_block_time = exact_after_partial
            .child_block_time
            .map(|child_time| child_time + 1);
        upsert_merge_mining_event(&client, source_id, &partial_first).await?;
        let promotion_error = upsert_merge_mining_event(&client, source_id, &exact_after_partial)
            .await
            .expect_err("an exact refinement must not replace contradictory partial evidence");
        assert!(
            promotion_error
                .to_string()
                .contains("exact refinement contradicts")
        );

        let exact_first = exact_payload(1_008, 2_008)?;
        let mut partial_after_exact = partial_payload(1_008, 2_009)?;
        partial_after_exact.child_block_time = exact_first
            .child_block_time
            .map(|child_time| child_time + 1);
        upsert_merge_mining_event(&client, source_id, &exact_first).await?;
        let fill_error = upsert_merge_mining_event(&client, source_id, &partial_after_exact)
            .await
            .expect_err("partial evidence must not overwrite a contradictory exact observation");
        assert!(
            fill_error
                .to_string()
                .contains("partial child evidence contradicts")
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn observation_requires_a_child_hash_or_height() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let mut payload = partial_payload(1_006, 2_006)?;
        payload.child_height = None;

        let error = upsert_merge_mining_event(&client, source_id, &payload)
            .await
            .expect_err("identity-free observation must be rejected");
        assert!(
            error
                .to_string()
                .contains("requires a child hash or child height")
        );
        Ok::<_, anyhow::Error>(())
    })
}
