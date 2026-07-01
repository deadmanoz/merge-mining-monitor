use anyhow::Result;
use std::collections::HashMap;

use mmm_capture::capture::CHILD_PAYOUT_REGISTRY_SOURCE;
use mmm_capture::source_registry::{ELASTOS_SOURCE_CODE, HATHOR_SOURCE_CODE};
use mmm_producers::chains::hathor::{
    HATHOR_REWARD_ADDRESS_NAMESPACE, HathorBlockMeta, HathorRpc, HathorTransaction,
};
use mmm_store::get_source_id;
use tokio_postgres::Client;

use crate::support::seed::pool_id_for_slug;

async fn revoke_event(client: &Client, event_id: i64, revoked_at: i64, reason: &str) -> Result<()> {
    client
        .execute(
            "UPDATE merge_mining_event SET revoked_at=$2, revocation_reason=$3 WHERE id=$1",
            &[&event_id, &revoked_at, &reason],
        )
        .await?;
    Ok(())
}

async fn event_revoked_at(client: &Client, event_id: i64) -> Result<Option<i64>> {
    Ok(client
        .query_one(
            "SELECT revoked_at FROM merge_mining_event WHERE id=$1",
            &[&event_id],
        )
        .await?
        .get("revoked_at"))
}

#[tokio::test]
async fn elastos_recapture_restores_reversible_but_keeps_conflict_sticky() -> Result<()> {
    use bitcoin::consensus::serialize;
    use bitcoin::hashes::Hash as _;
    use mmm_capture::auxpow::{
        ELASTOS_AUXPOW_CHAIN_ID, parse_elastos_auxpow, verify_auxpow_commitment,
    };
    use mmm_capture::capture::{
        ClassificationProof, ELASTOS_REVOKE_CLASSIFIER_CONFLICT, ELASTOS_REVOKE_NON_BTC,
        NormalizedEventEvidence, ResolvedPoolAttributions, build_event_payload_from_evidence,
    };
    use mmm_producers::chains::elastos::ElastosBlock;
    use mmm_store::{retag_revocation_reason, write_elastos_capture_in_txn};

    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, ELASTOS_SOURCE_CODE).await?;

        // Build a real Elastos event payload from the committed stale fixture.
        let block: ElastosBlock =
            serde_json::from_str(include_str!("../../../../fixtures/elastos/ela-360062.json"))
                .unwrap();
        let recon = block.reconstruct()?;
        let auxpow = recon.auxpow.clone().expect("auxpow present in fixture");
        let parsed = parse_elastos_auxpow(recon.prefix_header.clone(), &auxpow)?;
        verify_auxpow_commitment(&parsed, recon.block_hash, ELASTOS_AUXPOW_CHAIN_ID)?;
        let payload = build_event_payload_from_evidence(
            NormalizedEventEvidence {
                child_height: recon.height,
                child_block_hash: recon.block_hash.to_byte_array().to_vec(),
                child_block_time: i64::from(recon.time),
                btc_parent_header: parsed.parent_header.header,
                pow_validates_child_target: Some(true),
                btc_parent_coinbase_txid: Some(
                    parsed.parent_coinbase_txid.to_byte_array().to_vec(),
                ),
                btc_parent_coinbase_script: Some(parsed.parent_coinbase_script.clone()),
                btc_parent_coinbase_outputs: Some(serialize(&parsed.parent_coinbase_outputs)),
                child_coinbase_txid: None,
                child_coinbase_script: None,
                child_coinbase_outputs: None,
                aux_merkle_proof: Some(parsed.auxpow_bytes.clone()),
            },
            ResolvedPoolAttributions::default(),
            ClassificationProof::default(),
            1_800_000_000,
        )?;

        let event_id = write_elastos_capture_in_txn(&client, source_id, &payload).await?;

        // Reversible (elastos_non_btc) revoke -> a Valid recapture reactivates it.
        revoke_event(&client, event_id, 1_800_000_001, ELASTOS_REVOKE_NON_BTC).await?;
        write_elastos_capture_in_txn(&client, source_id, &payload).await?;
        assert!(
            event_revoked_at(&client, event_id).await?.is_none(),
            "a reversible elastos_non_btc revocation must be restored on recapture"
        );

        // Reversible revoke -> retag sticky (the classifier-conflict path) -> a Valid
        // recapture must NOT reactivate it.
        revoke_event(&client, event_id, 1_800_000_002, ELASTOS_REVOKE_NON_BTC).await?;
        let retagged = retag_revocation_reason(
            &client,
            source_id,
            recon.height,
            ELASTOS_REVOKE_NON_BTC,
            ELASTOS_REVOKE_CLASSIFIER_CONFLICT,
        )
        .await?;
        assert_eq!(retagged, 1, "the reversible-revoked row is retagged sticky");
        write_elastos_capture_in_txn(&client, source_id, &payload).await?;
        assert!(
            event_revoked_at(&client, event_id).await?.is_some(),
            "a sticky classifier-conflict revoke must NOT be auto-restored on recapture"
        );

        Ok(())
    })
}

async fn assert_hathor_recapture_restored(client: &Client, event_id: i64) -> Result<()> {
    assert!(
        event_revoked_at(client, event_id).await?.is_none(),
        "a voided revocation must be restored on recapture"
    );
    let nbits: i64 = client
        .query_one(
            "SELECT expected_btc_nbits FROM hathor_merge_mining_evidence WHERE event_id=$1",
            &[&event_id],
        )
        .await?
        .get(0);
    assert_eq!(
        nbits, 222,
        "the sidecar expected_btc_nbits must refresh on recapture"
    );

    Ok(())
}

async fn assert_hathor_conflict_sticky(client: &Client, event_id: i64) -> Result<()> {
    assert!(
        event_revoked_at(client, event_id).await?.is_some(),
        "a classifier-conflict revocation must stay sticky"
    );

    Ok(())
}

#[tokio::test]
async fn hathor_recapture_restores_reversible_but_keeps_conflict_sticky() -> Result<()> {
    use bitcoin::hashes::Hash as _;
    use mmm_capture::capture::{
        ClassificationProof, HATHOR_PROOF_FORMAT_RFC0006, HATHOR_REVOKE_NBITS_CONFLICT,
        HATHOR_REVOKE_VOIDED, HathorEvidencePayload, NormalizedEventEvidence,
        ResolvedPoolAttributions, build_event_payload_from_evidence,
    };
    use mmm_producers::chains::hathor::reconstruct_from_blobs;
    use mmm_store::write_hathor_capture_in_txn;
    use std::str::FromStr;

    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, HATHOR_SOURCE_CODE).await?;

        // Reconstruct a real BTC parent from a committed fixture.
        let fx: serde_json::Value =
            serde_json::from_str(include_str!("../../../../fixtures/hathor/1971823.json")).unwrap();
        let raw = hex::decode(fx["raw_hex"].as_str().unwrap()).unwrap();
        let aux_pow = hex::decode(fx["aux_pow_hex"].as_str().unwrap()).unwrap();
        let expected = bitcoin::BlockHash::from_str(fx["tx_id"].as_str().unwrap()).unwrap();
        let (_aux, recon) = reconstruct_from_blobs(&raw, &aux_pow, expected).unwrap();
        let block_hash = recon.header.block_hash().to_byte_array().to_vec();

        let evidence = NormalizedEventEvidence {
            child_height: 1_971_823,
            child_block_hash: block_hash.clone(),
            child_block_time: 1_637_668_049,
            btc_parent_header: recon.header,
            pow_validates_child_target: None,
            btc_parent_coinbase_txid: None,
            btc_parent_coinbase_script: None,
            btc_parent_coinbase_outputs: None,
            child_coinbase_txid: None,
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            aux_merkle_proof: None,
        };
        let payload = build_event_payload_from_evidence(
            evidence,
            ResolvedPoolAttributions::default(),
            ClassificationProof::default(),
            1_800_000_000,
        )?;
        let sidecar = |nbits: i64| HathorEvidencePayload {
            hathor_block_hash: block_hash.clone(),
            hathor_height: 1_971_823,
            aux_pow: aux_pow.clone(),
            funds_graph: raw[..4].to_vec(),
            funds_graph_split: 35,
            reward_output_details: None,
            reward_addresses: None,
            expected_btc_nbits: nbits,
            proof_format: HATHOR_PROOF_FORMAT_RFC0006,
        };

        // Capture, void-revoke, then recapture with a CHANGED expected nBits.
        let event_id =
            write_hathor_capture_in_txn(&client, source_id, &payload, &sidecar(111)).await?;
        revoke_event(&client, event_id, 1_800_000_001, HATHOR_REVOKE_VOIDED).await?;
        write_hathor_capture_in_txn(&client, source_id, &payload, &sidecar(222)).await?;

        assert_hathor_recapture_restored(&client, event_id).await?;

        // A classifier-conflict revocation must be STICKY across a recapture.
        revoke_event(
            &client,
            event_id,
            1_800_000_002,
            HATHOR_REVOKE_NBITS_CONFLICT,
        )
        .await?;
        write_hathor_capture_in_txn(&client, source_id, &payload, &sidecar(222)).await?;
        assert_hathor_conflict_sticky(&client, event_id).await?;

        Ok(())
    })
}

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

fn hathor_fixture_block(tx_id: &str, voided: bool) -> HathorBlockMeta {
    HathorBlockMeta {
        tx_id: tx_id.to_owned(),
        version: 3,
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
async fn hathor_state_machine_drives_capture_void_restore_and_hold() -> Result<()> {
    use mmm_bitcoin_core::ConfiguredParentClassifier;
    use mmm_capture::capture::HATHOR_REVOKE_VOIDED;
    use mmm_producers::chains::hathor::{
        HathorCaptureContext, HathorHeightOutcome, process_hathor_height,
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

    crate::run_mut_db_test!(client, {
        let context = HathorCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Disabled,
        )
        .await?;
        let source_id = context.source_id();

        let height = 1_971_823;
        let (tx_id, tx) =
            hathor_tx_from_fixture(include_str!("../../../../fixtures/hathor/1971823.json"));

        // 1) A live, non-voided v3 block writes an active event.
        let mut mock = MockHathorRpc::with_transactions(
            Some(hathor_fixture_block(&tx_id, false)),
            [(tx_id.clone(), tx)],
        );
        let out = process_hathor_height(&mut client, &mock, &context, height).await?;
        assert_eq!(out, HathorHeightOutcome::AuxpowWritten);
        assert!(active(&client, source_id).await?, "capture must be active");
        assert_hathor_reward_capture(&client, source_id).await?;

        // 2) The same height now voided revokes the event with `hathor_voided`.
        mock.block = Some(hathor_fixture_block(&tx_id, true));
        let out = process_hathor_height(&mut client, &mock, &context, height).await?;
        assert_eq!(out, HathorHeightOutcome::VoidedSkipped);
        let reason: Option<String> = client
            .query_one(
                "SELECT revocation_reason FROM merge_mining_event WHERE source_id=$1",
                &[&source_id],
            )
            .await?
            .get(0);
        assert!(!active(&client, source_id).await?, "void must revoke");
        assert_eq!(reason.as_deref(), Some(HATHOR_REVOKE_VOIDED));

        // 3) Reappearing non-voided: the reversible void revocation restores.
        mock.block = Some(hathor_fixture_block(&tx_id, false));
        let out = process_hathor_height(&mut client, &mock, &context, height).await?;
        assert_eq!(out, HathorHeightOutcome::AuxpowWritten);
        assert!(active(&client, source_id).await?, "recapture must restore");

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
        CACHE_CSV_HEADER, HathorCacheConfig, HathorCaptureContext, run_hathor_cache_ingest,
    };

    crate::run_mut_db_test!(client, {
        let context = HathorCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Disabled,
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
