use anyhow::Result;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification};
use mmm_capture::capture::HATHOR_REVOKE_NON_BTC;
use mmm_capture::nbits_table::daa_epoch_start;
use mmm_producers::chains::hathor::{
    HathorBlockMeta, HathorCaptureContext, HathorHeightOutcome, HathorRpc, HathorTransaction,
    process_hathor_height,
};
use tokio_postgres::Client;

/// A `HathorRpc` that always returns one fixed block + transaction, so a committed
/// Hathor block fixture can drive `process_hathor_height` end to end.
struct FixtureHathorRpc {
    meta: HathorBlockMeta,
    tx: HathorTransaction,
}

impl HathorRpc for FixtureHathorRpc {
    async fn get_block_at_height(&self, _height: i32) -> Result<Option<HathorBlockMeta>> {
        Ok(Some(self.meta.clone()))
    }

    async fn get_transaction(&self, _tx_id: &str) -> Result<Option<HathorTransaction>> {
        Ok(Some(self.tx.clone()))
    }
}

/// An `unknown` parent classification over the BTC genesis header, for fake
/// classifiers whose horizon outcome is driven by `synced_tip_height`, not
/// parent placement.
fn unknown_genesis_parent() -> ParentClassification {
    ParentClassification::unknown(
        &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
    )
}

async fn hathor_context(
    client: &Client,
    classifier: ConfiguredParentClassifier,
) -> Result<HathorCaptureContext> {
    crate::support::db::seed_bitcoin_core_header_cache_through(
        client,
        710_969,
        i64::MAX,
        0x170c_69ea,
    )
    .await?;
    HathorCaptureContext::new_with_classifier(client, classifier).await
}

fn hathor_1971823_fixture() -> (i32, FixtureHathorRpc) {
    let j: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hathor/1971823.json"
    )))
    .expect("deserialize Hathor 1971823 fixture");
    let height = j["hathor_height"].as_i64().unwrap() as i32;
    let meta = HathorBlockMeta {
        tx_id: j["tx_id"].as_str().unwrap().to_owned(),
        version: j["version"].as_i64().unwrap() as i32,
        is_voided: j["is_voided"].as_bool().unwrap_or(false),
    };
    let tx = HathorTransaction {
        raw: j["raw_hex"].as_str().unwrap().to_owned(),
        aux_pow: Some(j["aux_pow_hex"].as_str().unwrap().to_owned()),
        hash: j["tx_id"].as_str().unwrap().to_owned(),
        timestamp: j["timestamp"].as_i64().unwrap(),
    };
    (height, FixtureHathorRpc { meta, tx })
}

async fn assert_revoked_hathor_event(client: &Client, source_id: i64, height: i32) -> Result<()> {
    let row = client
        .query_one(
            "SELECT COUNT(*)::int8, \
                    COUNT(*) FILTER (WHERE revoked_at IS NULL)::int8, \
                    MAX(revocation_reason) \
             FROM merge_mining_event \
             WHERE source_id = $1 AND child_height = $2",
            &[&source_id, &height],
        )
        .await?;
    assert_eq!(
        row.get::<_, i64>(0),
        1,
        "reprocess must not write a replacement row"
    );
    assert_eq!(
        row.get::<_, i64>(1),
        0,
        "a rejected Hathor parent must revoke the active event"
    );
    assert_eq!(
        row.get::<_, Option<String>>(2).as_deref(),
        Some(HATHOR_REVOKE_NON_BTC)
    );
    Ok(())
}

#[tokio::test]
async fn hathor_in_table_valid_far_future_height_is_revoked_against_fresh_tip() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();

        let write_context = hathor_context(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        assert_eq!(
            process_hathor_height(&mut client, &rpc, &write_context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let active: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2 AND revoked_at IS NULL",
                &[&write_context.source_id(), &height],
            )
            .await?
            .get(0);
        assert_eq!(
            active, 1,
            "the in-table Valid fixture must first write an active event"
        );

        // BTC parent height 710,969 is in-table Valid, but a fresh Core tip far
        // below it proves the claimed height fabricated; the production Valid arm
        // must revoke the active event.
        let revoke_context = hathor_context(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(500_000),
            ),
        )
        .await?;
        assert_eq!(revoke_context.source_id(), write_context.source_id());
        assert_eq!(
            process_hathor_height(&mut client, &rpc, &revoke_context, height).await?,
            HathorHeightOutcome::NonBtcParentSkipped
        );
        assert_revoked_hathor_event(&client, revoke_context.source_id(), height).await?;
        Ok(())
    })
}

#[tokio::test]
async fn core_cache_nbits_mismatch_revokes_an_existing_event() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();
        let context = hathor_context(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        assert_eq!(
            process_hathor_height(&mut client, &rpc, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );

        client
            .execute(
                "UPDATE bitcoin_core_header SET bits = $1 WHERE height = $2",
                &[&i64::from(0x170c_69ea_u32 ^ 1), &daa_epoch_start(710_969)],
            )
            .await?;
        assert_eq!(
            process_hathor_height(&mut client, &rpc, &context, height).await?,
            HathorHeightOutcome::NonBtcParentSkipped
        );
        assert_revoked_hathor_event(&client, context.source_id(), height).await?;
        Ok(())
    })
}

#[tokio::test]
async fn in_table_valid_writes_the_event_end_to_end() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // A real validated Hathor block (BTC parent 710,969, in-table Valid). Core
        // enabled + a fresh tip above the parent height -> Valid (not far-future) ->
        // the event is WRITTEN through the full production `write_valid_capture` path,
        // the same Hathor write the above-horizon Valid arm dispatches to (which has
        // no real above-horizon Hathor block to exercise it directly yet).
        let (height, rpc) = hathor_1971823_fixture();
        let context = hathor_context(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        let outcome = process_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, HathorHeightOutcome::AuxpowWritten);
        let active: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2 AND revoked_at IS NULL",
                &[&context.source_id(), &height],
            )
            .await?
            .get(0);
        assert_eq!(
            active, 1,
            "an in-table Valid Hathor parent must write one active event"
        );
        Ok(())
    })
}

#[tokio::test]
async fn live_capture_promotes_a_hashless_historical_row_without_revoking_it() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();
        let context = hathor_context(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        assert_eq!(
            process_hathor_height(&mut client, &rpc, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let event_id: i64 = client
            .query_one(
                "SELECT id FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2",
                &[&context.source_id(), &height],
            )
            .await?
            .get(0);

        client
            .execute(
                "DELETE FROM hathor_merge_mining_evidence WHERE event_id = $1",
                &[&event_id],
            )
            .await?;
        client
            .execute(
                "UPDATE merge_mining_event SET child_block_hash = NULL WHERE id = $1",
                &[&event_id],
            )
            .await?;

        assert_eq!(
            process_hathor_height(&mut client, &rpc, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let row = client
            .query_one(
                "SELECT id, child_block_hash, revoked_at, \
                        EXISTS (SELECT 1 FROM hathor_merge_mining_evidence h WHERE h.event_id = e.id) \
                 FROM merge_mining_event e \
                 WHERE source_id = $1 AND child_height = $2",
                &[&context.source_id(), &height],
            )
            .await?;
        assert_eq!(
            row.get::<_, i64>(0),
            event_id,
            "the partial row is promoted in place"
        );
        assert!(row.get::<_, Option<Vec<u8>>>(1).is_some());
        assert_eq!(
            row.get::<_, Option<i64>>(2),
            None,
            "the promoted row stays active"
        );
        assert!(row.get::<_, bool>(3), "the live sidecar is attached");
        let pending: i64 = client
            .query_one(
                "SELECT count(*) FROM poll_pending_reconcile WHERE source_id = $1",
                &[&context.source_id()],
            )
            .await?
            .get(0);
        assert_eq!(pending, 0);
        Ok::<_, anyhow::Error>(())
    })
}
