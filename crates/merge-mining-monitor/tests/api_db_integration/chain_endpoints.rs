use anyhow::Result;
use std::collections::HashMap;

use mmm_capture::capture::CHILD_PAYOUT_REGISTRY_SOURCE;
use mmm_producers::chains::hathor::{
    HATHOR_REWARD_ADDRESS_NAMESPACE, HathorBlockMeta, HathorRpc, HathorTransaction,
};
use tokio_postgres::Client;

use crate::support::seed::pool_id_for_slug;

/// A small in-memory [`HathorRpc`] so the per-height state machine can be driven
/// deterministically through `process_hathor_height` without the live REST API.
struct MockHathorRpc {
    block: Option<HathorBlockMeta>,
    txs: HashMap<String, HathorTransaction>,
}

impl MockHathorRpc {
    fn with_transactions(
        block: Option<HathorBlockMeta>,
        txs: impl IntoIterator<Item = (String, HathorTransaction)>,
    ) -> Self {
        Self {
            block,
            txs: txs.into_iter().collect(),
        }
    }
}

impl HathorRpc for MockHathorRpc {
    async fn get_block_at_height(&self, _height: i32) -> Result<Option<HathorBlockMeta>> {
        Ok(self.block.clone())
    }
    async fn get_transaction(&self, tx_id: &str) -> Result<Option<HathorTransaction>> {
        Ok(self.txs.get(tx_id).cloned())
    }
}

fn hathor_tx_from_fixture(json: &str) -> (String, HathorTransaction) {
    let fx: serde_json::Value = serde_json::from_str(json).unwrap();
    let tx_id = fx["tx_id"].as_str().unwrap().to_owned();
    (
        tx_id.clone(),
        HathorTransaction {
            raw: fx["raw_hex"].as_str().unwrap().to_owned(),
            aux_pow: Some(fx["aux_pow_hex"].as_str().unwrap().to_owned()),
            hash: tx_id,
            timestamp: 1_637_668_049,
        },
    )
}

fn hathor_fixture_block(tx_id: &str, height: i32, voided: bool) -> HathorBlockMeta {
    HathorBlockMeta {
        tx_id: tx_id.to_owned(),
        version: 3,
        height,
        is_voided: voided,
    }
}

async fn assert_hathor_reward_capture(client: &Client, source_id: i64) -> Result<()> {
    let event_id: i64 = client
        .query_one(
            "SELECT id FROM merge_mining_event WHERE source_id=$1",
            &[&source_id],
        )
        .await?
        .get(0);
    let attr = client
        .query_one(
            "SELECT source, matched_value, pool_id, pool_identity_id, details \
             FROM event_pool_attribution \
             WHERE event_id=$1 \
               AND side='child_block' \
               AND namespace=$2",
            &[&event_id, &HATHOR_REWARD_ADDRESS_NAMESPACE],
        )
        .await?;
    assert_eq!(
        attr.get::<_, String>("matched_value"),
        "HV3iKMJpuZpktXwpoBxKEUetG6NS3zfXje"
    );
    assert_eq!(
        attr.get::<_, String>("source"),
        CHILD_PAYOUT_REGISTRY_SOURCE
    );
    assert_eq!(
        attr.get::<_, Option<i64>>("pool_id"),
        Some(pool_id_for_slug(client, "poolin").await?)
    );
    assert!(attr.get::<_, Option<i64>>("pool_identity_id").is_some());
    let details: serde_json::Value = attr.get("details");
    assert_eq!(
        details,
        serde_json::json!({
            "address_source": "hathor_funds_graph",
            "sidecar": "hathor_merge_mining_evidence.funds_graph",
            "output_indexes": [0],
        })
    );

    let sidecar = client
        .query_one(
            "SELECT reward_output_details, reward_addresses \
             FROM hathor_merge_mining_evidence \
             WHERE event_id=$1",
            &[&event_id],
        )
        .await?;
    let reward_addresses: serde_json::Value = sidecar.get("reward_addresses");
    let reward_details: serde_json::Value = sidecar.get("reward_output_details");
    assert_eq!(
        reward_addresses,
        serde_json::json!(["HV3iKMJpuZpktXwpoBxKEUetG6NS3zfXje"])
    );
    assert_eq!(reward_details[0]["value"], 3200);
    assert_eq!(reward_details[0]["script_type"], "P2PKH");
    assert_eq!(reward_details[0]["skipped_reason"], serde_json::Value::Null);
    Ok(())
}

#[tokio::test]
async fn hathor_state_machine_drives_capture_void_and_hold() -> Result<()> {
    use mmm_bitcoin_core::ConfiguredParentClassifier;
    use mmm_producers::chains::hathor::{
        ChainObservation, HathorCaptureContext, HathorHeightOutcome, process_hathor_height,
    };

    // The single event for the seeded Hathor source: NULL revoked_at means active.
    async fn active(client: &Client, source_id: i64) -> Result<bool> {
        Ok(client
            .query_one(
                "SELECT revoked_at IS NULL FROM merge_mining_event WHERE source_id=$1",
                &[&source_id],
            )
            .await?
            .get(0))
    }

    // Whether that event carries a displacement record.
    async fn displaced(client: &Client, source_id: i64) -> Result<bool> {
        Ok(client
            .query_one(
                "SELECT child_displaced_at IS NOT NULL FROM merge_mining_event WHERE source_id=$1",
                &[&source_id],
            )
            .await?
            .get(0))
    }

    crate::run_mut_db_test!(client, {
        let context = HathorCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Disabled,
            ChainObservation::Live { fork_window: 20 },
        )
        .await?;
        let source_id = context.source_id();

        let height = 1_971_823;
        let (tx_id, tx) =
            hathor_tx_from_fixture(include_str!("../../../../fixtures/hathor/1971823.json"));

        // 1) A live, non-voided v3 block writes an active event.
        let mut mock = MockHathorRpc::with_transactions(
            Some(hathor_fixture_block(&tx_id, height, false)),
            [(tx_id.clone(), tx)],
        );
        let out = process_hathor_height(&mut client, &mock, &context, height).await?;
        assert_eq!(out, HathorHeightOutcome::AuxpowWritten);
        assert!(active(&client, source_id).await?, "capture must be active");
        assert_hathor_reward_capture(&client, source_id).await?;

        // 2) The same height now voided: the block is not the chain's block, but
        // nothing names its replacement, so the event stays active and
        // undisplaced (a child-DAG void is not bad evidence).
        mock.block = Some(hathor_fixture_block(&tx_id, height, true));
        let out = process_hathor_height(&mut client, &mock, &context, height).await?;
        assert_eq!(out, HathorHeightOutcome::VoidedSkipped);
        assert!(active(&client, source_id).await?, "a void must not revoke");
        assert!(
            !displaced(&client, source_id).await?,
            "a void names no replacement"
        );

        // 3) Reappearing non-voided: still the one active, current event.
        mock.block = Some(hathor_fixture_block(&tx_id, height, false));
        let out = process_hathor_height(&mut client, &mock, &context, height).await?;
        assert_eq!(out, HathorHeightOutcome::AuxpowWritten);
        assert!(
            active(&client, source_id).await?,
            "recapture keeps it active"
        );
        assert!(!displaced(&client, source_id).await?);

        // 4) An absent block holds without mutating the active event.
        mock.block = None;
        let out = process_hathor_height(&mut client, &mock, &context, height).await?;
        assert_eq!(out, HathorHeightOutcome::AbsentHold);
        assert!(active(&client, source_id).await?, "absent hold is no-op");

        Ok(())
    })
}

/// The cache-ingest runner over an in-memory archive CSV: real capture path,
/// runner-level absent accounting, skip-ledger output, and idempotent re-run.
#[tokio::test]
async fn hathor_cache_ingest_streams_counts_and_is_idempotent() -> Result<()> {
    use mmm_bitcoin_core::ConfiguredParentClassifier;
    use mmm_producers::chains::hathor::{
        CACHE_CSV_HEADER, ChainObservation, HathorCacheConfig, HathorCaptureContext,
        run_hathor_cache_ingest,
    };

    crate::run_mut_db_test!(client, {
        let context = HathorCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Disabled,
            ChainObservation::ArchiveReplay,
        )
        .await?;
        let source_id = context.source_id();

        let fx: serde_json::Value =
            serde_json::from_str(include_str!("../../../../fixtures/hathor/1971823.json")).unwrap();
        let tx_id = fx["tx_id"].as_str().unwrap();
        let raw = fx["raw_hex"].as_str().unwrap();
        let aux = fx["aux_pow_hex"].as_str().unwrap();
        let funds = &raw[..raw.len() - aux.len()];
        let timestamp = fx["timestamp"].as_i64().unwrap();
        let height: i32 = fx["hathor_height"].as_i64().unwrap() as i32;
        let csv = format!("{CACHE_CSV_HEADER}\r\n{height},{tx_id},{timestamp},{funds},{aux}\r\n");

        // An explicit range around the single row exercises head and tail
        // absent-height accounting (3 below, 2 above).
        let config = HathorCacheConfig {
            csv_path: std::path::PathBuf::from("in-memory.csv"),
            start_height: Some(height - 3),
            end_height: Some(height + 2),
            progress_every: 1_000,
        };

        let mut ledger: Vec<u8> = Vec::new();
        let summary = run_hathor_cache_ingest(
            &mut client,
            &context,
            std::io::Cursor::new(csv.clone()),
            &mut ledger,
            &config,
        )
        .await?;
        assert_eq!(summary.rows_seen, 1);
        assert_eq!(summary.auxpow_written, 1);
        assert_eq!(summary.absent_heights, 5);
        assert_eq!(summary.height_attempts(), 6);
        assert_eq!(summary.first_processed_height, Some(height));
        assert_eq!(summary.last_processed_height, Some(height));

        let ledger_text = String::from_utf8(ledger.clone())?;
        assert!(
            ledger_text.contains(&format!("{}..{},Absent", height - 3, height - 1)),
            "head absent range missing from ledger: {ledger_text}"
        );
        assert!(
            ledger_text.contains(&format!("{}..{},Absent", height + 1, height + 2)),
            "tail absent range missing from ledger: {ledger_text}"
        );

        let events: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event WHERE source_id=$1",
                &[&source_id],
            )
            .await?
            .get(0);
        assert_eq!(events, 1, "exactly one event from the single archive row");
        let block_rows: i64 = client
            .query_one("SELECT count(*) FROM block", &[])
            .await?
            .get(0);
        assert_eq!(
            block_rows, 1,
            "the parent must reconcile into the read model"
        );

        // Idempotent re-run: same counts, no duplicate event, ledger appends a
        // second run section.
        let summary2 = run_hathor_cache_ingest(
            &mut client,
            &context,
            std::io::Cursor::new(csv),
            &mut ledger,
            &config,
        )
        .await?;
        assert_eq!(summary2, summary);
        let events: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event WHERE source_id=$1",
                &[&source_id],
            )
            .await?
            .get(0);
        assert_eq!(events, 1, "re-run must not duplicate the event");

        Ok(())
    })
}
