use super::*;

struct SyncErrorExpectation<'a> {
    code: &'a str,
    height: i32,
    message: &'a str,
    details: &'a serde_json::Value,
}

impl SyncErrorExpectation<'_> {
    fn suspended_json(&self) -> serde_json::Value {
        json!({
            "code": self.code,
            "height": self.height,
            "message": self.message,
            "details": self.details,
        })
    }
}

async fn seed_sync_error(
    client: &Client,
    source_id: i64,
    expected: &SyncErrorExpectation<'_>,
) -> Result<()> {
    client
        .execute(
            "UPDATE bitcoin_core_sync_state SET last_error_code = $2, \
                 last_error_height = $3, last_error = $4, last_error_details = $5 \
             WHERE source_id = $1 AND sync_mode = 'contiguous'",
            &[
                &source_id,
                &expected.code,
                &expected.height,
                &expected.message,
                &Json(expected.details),
            ],
        )
        .await?;
    Ok(())
}

async fn assert_sync_error(
    client: &Client,
    source_id: i64,
    expected: &SyncErrorExpectation<'_>,
) -> Result<()> {
    let row = client
        .query_one(
            "SELECT last_error_code, last_error_height, last_error, last_error_details \
             FROM bitcoin_core_sync_state WHERE source_id = $1",
            &[&source_id],
        )
        .await?;
    assert_eq!(
        row.get::<_, Option<String>>(0).as_deref(),
        Some(expected.code)
    );
    assert_eq!(row.get::<_, Option<i32>>(1), Some(expected.height));
    assert_eq!(
        row.get::<_, Option<String>>(2).as_deref(),
        Some(expected.message)
    );
    assert_eq!(
        row.get::<_, Json<serde_json::Value>>(3).0,
        *expected.details
    );
    Ok(())
}

async fn seed_two_block_suffix(
    client: &mut Client,
    original_time: u32,
    active_time: u32,
) -> Result<(i64, BTreeMap<i32, Header>, Vec<CoreCanonicalReplacement>)> {
    let bitcoin = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let original = test_header_chain(3, original_time);
    let original_source = FakeBitcoinCoreBackboneSource::new(3, original.clone());
    run_sync_bitcoin_core(
        client,
        &original_source,
        BitcoinCoreSyncConfig {
            from_height: Some(0),
            to_height: Some(3),
            tip: true,
            missing_only: true,
            ..BitcoinCoreSyncConfig::default()
        },
    )
    .await?;

    let active = fork_header_chain(&original, 3, 4, active_time);
    let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
    let mut replacements = Vec::new();
    for height in 3..=4 {
        let header = active[&height];
        replacements.push(CoreCanonicalReplacement {
            height,
            header,
            coinbase: active_source.block_coinbase(header.block_hash()).await?,
        });
    }
    Ok((bitcoin, original, replacements))
}

async fn drain_core_reconcile_disabled(client: &mut Client, source_id: i64) -> Result<()> {
    drain_core_reconcile_queue(client, source_id, &ConfiguredParentClassifier::Disabled).await
}

#[tokio::test]
async fn cursor_suffix_pending_state_preserves_the_higher_live_target() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (bitcoin, _, replacements) =
            seed_two_block_suffix(&mut client, 1_800_043_000, 1_800_044_000).await?;
        let expected = canonical_view(&client, 2, 4).await?;
        let live = test_header_chain(81, 1_800_044_500);
        let live_target = (80, live[&80].block_hash());

        replace_core_canonical_suffix_validated(
            &mut client,
            bitcoin,
            3,
            2,
            CoreSuffixReplacementInput {
                expected_local: &expected,
                replacements: &replacements,
                pending_sync_target: Some(live_target),
            },
            (async |_txn| Ok(()), async |_txn| Ok(())),
        )
        .await?;

        let state = client
            .query_one(
                "SELECT target_tip_height, target_tip_hash, last_error_details \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(state.get::<_, Option<i32>>(0), Some(live_target.0));
        assert_eq!(
            state.get::<_, Option<Vec<u8>>>(1),
            Some(live_target.1.to_byte_array().to_vec())
        );
        let details: Json<serde_json::Value> = state.get(2);
        assert_eq!(details.0["replacement_target_height"], json!(4));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn core_suffix_queue_restores_suspended_error_without_nesting() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let (bitcoin, original, replacements) =
            seed_two_block_suffix(&mut client, 1_800_045_000, 1_800_046_000).await?;
        let prior_details = json!({
            "hash": hex::encode(header_hash_bytes(&original[&1])),
            "attempt": 7,
        });
        let prior = SyncErrorExpectation {
            code: "coinbase_fetch_failed",
            height: 1,
            message: "lower cursor coinbase is still incomplete",
            details: &prior_details,
        };
        seed_sync_error(&client, bitcoin, &prior).await?;

        let expected = canonical_view(&client, 2, 4).await?;
        replace_core_canonical_suffix(&mut client, bitcoin, 3, 2, &expected, &replacements).await?;
        let first_pending: Json<serde_json::Value> = client
            .query_one(
                "SELECT last_error_details FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?
            .get(0);
        assert_eq!(first_pending.0["suspended_error"], prior.suspended_json());

        // A newer generation while the first queue is pending must carry the
        // original tuple, not wrap another pending status around it.
        let expected = canonical_view(&client, 2, 4).await?;
        replace_core_canonical_suffix(&mut client, bitcoin, 4, 2, &expected, &replacements).await?;
        let second_pending: Json<serde_json::Value> = client
            .query_one(
                "SELECT last_error_details FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?
            .get(0);
        assert_eq!(
            second_pending.0["suspended_error"],
            first_pending.0["suspended_error"]
        );
        assert!(
            second_pending.0["suspended_error"]
                .get("suspended_error")
                .is_none()
        );

        let mut restart_client = crate::support::db::connect_to_schema(&schema).await?;
        drain_core_reconcile_disabled(&mut restart_client, bitcoin).await?;
        let queue_count: i64 = restart_client
            .query_one(
                "SELECT count(*)::bigint FROM bitcoin_core_reconcile_queue WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?
            .get(0);
        assert_eq!(queue_count, 0);
        assert_sync_error(&restart_client, bitcoin, &prior).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn core_suffix_queue_consumes_only_covered_structural_errors() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let (bitcoin, _, replacements) =
            seed_two_block_suffix(&mut client, 1_800_047_000, 1_800_048_000).await?;
        let covered_details = json!({ "height": 3 });
        let covered = SyncErrorExpectation {
            code: "backbone_height_conflict",
            height: 3,
            message: "same-height conflict replaced by suffix",
            details: &covered_details,
        };
        seed_sync_error(&client, bitcoin, &covered).await?;

        let expected = canonical_view(&client, 2, 4).await?;
        replace_core_canonical_suffix(&mut client, bitcoin, 3, 2, &expected, &replacements).await?;
        let pending = client
            .query_one(
                "SELECT last_error_code, last_error_details \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(
            pending.get::<_, Option<String>>(0).as_deref(),
            Some("backbone_reorg_reconcile_pending")
        );
        let pending_details: Json<serde_json::Value> = pending.get(1);
        assert!(pending_details.0.get("suspended_error").is_none());

        let mut restart_client = crate::support::db::connect_to_schema(&schema).await?;
        drain_core_reconcile_disabled(&mut restart_client, bitcoin).await?;
        assert_drained_core_queue(&restart_client, bitcoin).await?;

        let ancestor_details = json!({ "height": 2 });
        let ancestor = SyncErrorExpectation {
            code: "backbone_link_mismatch",
            height: 2,
            message: "link mismatch below replacement suffix",
            details: &ancestor_details,
        };
        seed_sync_error(&restart_client, bitcoin, &ancestor).await?;
        let expected = canonical_view(&restart_client, 2, 4).await?;
        replace_core_canonical_suffix(&mut restart_client, bitcoin, 4, 2, &expected, &replacements)
            .await?;
        let pending: Json<serde_json::Value> = restart_client
            .query_one(
                "SELECT last_error_details FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?
            .get(0);
        assert_eq!(pending.0["suspended_error"], ancestor.suspended_json());
        drain_core_reconcile_disabled(&mut restart_client, bitcoin).await?;
        assert_sync_error(&restart_client, bitcoin, &ancestor).await?;

        Ok::<_, anyhow::Error>(())
    })
}
