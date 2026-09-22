//! Real reviewed parents exercise catalogue promotion, witness preservation and
//! API semantics through the normal error-observation import path.

use std::path::PathBuf;

use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, FakeParentClassifier, HeightSource, ParentClassification,
};
use mmm_producers::run_error_observation_import_for_test;
use serde::Deserialize;
use tokio_postgres::Client;

use crate::support::seed::insert_block;

#[derive(Deserialize)]
struct ReviewedParent {
    height: i32,
    hash: String,
    header_hex: String,
    reason: String,
}

fn reviewed_parents() -> Result<Vec<ReviewedParent>> {
    Ok(serde_json::from_str(include_str!(
        "../../../../fixtures/error-blocks/body-invalid-parents.json"
    ))?)
}

fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/error-blocks/body-invalid-observations.csv")
}

fn classifier(parents: &[ReviewedParent]) -> Result<ConfiguredParentClassifier> {
    let first: Header = deserialize(&hex::decode(&parents[0].header_hex)?)?;
    let mut fake = FakeParentClassifier::new(ParentClassification::unknown(&first));
    for parent in parents {
        let header: Header = deserialize(&hex::decode(&parent.header_hex)?)?;
        header.validate_pow(header.target())?;
        assert_eq!(header.block_hash().to_string(), parent.hash);
        let entry = mmm_capture::error_blocks::lookup(&header.block_hash().to_byte_array())
            .expect("every reviewed body-invalid parent belongs in the pinned catalogue");
        assert_eq!(entry.height, parent.height);
        assert_eq!(entry.rejection_reason, parent.reason);
        fake = fake.with_verdicts_for(
            &header,
            [Some(ParentClassification::error_block(
                &header,
                parent.height,
                HeightSource::ErrorBlockCatalog,
                None,
                &parent.reason,
            ))],
        );
    }
    Ok(ConfiguredParentClassifier::Fake(fake))
}

async fn evidence_snapshot(client: &Client) -> Result<Vec<String>> {
    Ok(client.query(
        "SELECT json_build_array(id, source_id, child_height, child_block_hash, \
         btc_parent_header_hash, btc_parent_header_bytes, btc_parent_coinbase_script, \
         btc_parent_coinbase_outputs, aux_merkle_proof, child_header_bytes, child_block_time, child_nbits)::text \
         FROM merge_mining_event ORDER BY id", &[],
    ).await?.into_iter().map(|row| row.get(0)).collect())
}

async fn restore_legacy_stale_state(client: &Client, parents: &[ReviewedParent]) -> Result<()> {
    for parent in parents {
        let header: Header = deserialize(&hex::decode(&parent.header_hex)?)?;
        let hash = header.block_hash().to_byte_array().to_vec();
        let competitor = crate::support::seed::hash_bytes(u32::try_from(parent.height)?);
        insert_block(
            client,
            &competitor,
            &[0; 32],
            Some(parent.height),
            "canonical",
            i64::from(header.time),
            None,
        )
        .await?;
        client
            .execute(
                "UPDATE block SET kind='stale', btc_height_source='bitcoin-core', \
             canonical_competitor_hash=$2, error_block_reason=NULL WHERE btc_header_hash=$1",
                &[&hash, &competitor],
            )
            .await?;
    }
    client
        .execute("UPDATE merge_mining_event SET btc_parent_kind='stale'", &[])
        .await?;
    client
        .execute(
            "UPDATE historical_event_provenance SET artifact_scope='full_classifier_inventory', \
         classification='stale', validation_status='VALID', \
         publication_ref='e3dc6d620b984b72b80cff3d3c2a67c9532444ae'",
            &[],
        )
        .await?;
    Ok(())
}

// Match import-all ordering: error witnesses acquire protected provenance before
// ordinary snapshots remove their old stale coordinates.
async fn reconcile_old_snapshots(client: &mut Client) -> Result<()> {
    let sources = client
        .query(
            "SELECT DISTINCT e.source_id, p.chain FROM merge_mining_event e \
         JOIN historical_event_provenance p ON p.event_id=e.id",
            &[],
        )
        .await?;
    let txn = client.transaction().await?;
    for row in sources {
        let source_id: i64 = row.get(0);
        let chain: String = row.get(1);
        mmm_read_model::clear_authoritative_historical_provenance_in_transaction(&txn, &chain)
            .await?;
        let removed = mmm_read_model::reconcile_authoritative_historical_source_in_transaction(
            &txn,
            source_id,
            "new-ordinary-snapshot",
            &chain,
        )
        .await?;
        assert_eq!(
            removed, 0,
            "new error witnesses survive ordinary snapshot cleanup"
        );
    }
    txn.commit().await?;
    Ok(())
}

#[tokio::test]
async fn reviewed_body_invalid_import_promotes_stales_without_losing_witnesses() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let parents = reviewed_parents()?;
        assert_eq!(parents.len(), 10);
        let classifier = classifier(&parents)?;
        let hashes: Vec<[u8; 32]> = parents
            .iter()
            .map(|parent| {
                let header: Header = deserialize(&hex::decode(&parent.header_hex)?)?;
                Ok(header.block_hash().to_byte_array())
            })
            .collect::<Result<_>>()?;
        let path = fixture_path();
        let first =
            run_error_observation_import_for_test(&mut client, &classifier, &path, &hashes).await?;
        assert_eq!(first.ingested, 19);
        let before = evidence_snapshot(&client).await?;
        assert_eq!(before.len(), 19);
        restore_legacy_stale_state(&client, &parents).await?;
        let error = reconcile_old_snapshots(&mut client).await.unwrap_err();
        assert!(error.to_string().contains("run import-all"));
        assert_eq!(evidence_snapshot(&client).await?, before);
        let provenance_count: i64 = client
            .query_one("SELECT count(*) FROM historical_event_provenance", &[])
            .await?
            .get(0);
        assert_eq!(provenance_count, 19, "failed cleanup rolls back provenance");
        run_error_observation_import_for_test(&mut client, &classifier, &path, &hashes).await?;
        reconcile_old_snapshots(&mut client).await?;
        assert_eq!(evidence_snapshot(&client).await?, before);
        for parent in &parents {
            let payload = mmm_api::projection::block(&client, &parent.hash)
                .await
                .map_err(|error| anyhow::anyhow!("{error:?}"))?;
            assert_eq!(payload.block.kind, "error_block");
            assert_eq!(
                payload.block.error_block_reason.as_deref(),
                Some(parent.reason.as_str())
            );
            assert!(
                serde_json::to_value(&payload.block)?
                    .get("body_invalid")
                    .is_none()
            );
            assert!(payload.competition.is_none());
            assert!(payload.stale_branch.is_none());
            assert!(!payload.event_details.is_empty());
        }
        let remaining_stales: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event WHERE btc_parent_kind='stale'",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(remaining_stales, 0);
        let obsolete_table: Option<String> = client
            .query_one(
                "SELECT to_regclass(format('%I.body_invalid_stale', current_schema()))::text",
                &[],
            )
            .await?
            .get(0);
        assert!(obsolete_table.is_none());
        run_error_observation_import_for_test(&mut client, &classifier, &path, &hashes).await?;
        assert_eq!(
            evidence_snapshot(&client).await?,
            before,
            "replay is idempotent"
        );
        Ok(())
    })
}
