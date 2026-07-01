use anyhow::Result;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification};
use mmm_capture::capture::HATHOR_REVOKE_NON_BTC;
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
/// classifiers whose horizon outcome is driven by `synced_tip_height` /
/// `epoch_nbits`, not `classify_parent`.
fn unknown_genesis_parent() -> ParentClassification {
    ParentClassification::unknown(
        &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
    )
}

async fn hathor_context(
    client: &Client,
    classifier: ConfiguredParentClassifier,
) -> Result<HathorCaptureContext> {
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
        let row = client
            .query_one(
                "SELECT COUNT(*)::int8, \
                        COUNT(*) FILTER (WHERE revoked_at IS NULL)::int8, \
                        MAX(revocation_reason) \
                 FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2",
                &[&revoke_context.source_id(), &height],
            )
            .await?;
        let total_rows: i64 = row.get(0);
        let active_rows: i64 = row.get(1);
        let reason: Option<String> = row.get(2);
        assert_eq!(total_rows, 1, "reprocess must not write a replacement row");
        assert_eq!(
            active_rows, 0,
            "fresh-tip far-future guard must revoke the active Hathor event"
        );
        assert_eq!(reason.as_deref(), Some(HATHOR_REVOKE_NON_BTC));
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
