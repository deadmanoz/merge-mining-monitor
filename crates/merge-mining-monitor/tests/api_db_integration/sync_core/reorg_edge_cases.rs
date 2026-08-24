use super::*;

#[tokio::test]
async fn follow_repair_converges_core_promoted_duplicate_canonical_height() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let original = test_header_chain(4, 1_800_027_000);
        let original_source = FakeBitcoinCoreBackboneSource::new(3, original.clone());
        run_sync_bitcoin_core(
            &mut client,
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

        let active = fork_header_chain(&original, 3, 4, 1_800_027_100);
        let old_h3 = header_hash_bytes(&original[&3]);
        let new_h3 = header_hash_bytes(&active[&3]);
        let event_id = insert_event_with_real_parent_header(
            &client,
            namecoin,
            80,
            &hash_bytes(0x8010),
            &active[&3],
            "canonical",
            Some(3),
        )
        .await?;
        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            canonical_verdict(&active[&3], 3),
        ));
        reconcile_from_merge_mining_event(&mut client, event_id, &classifier, None).await?;

        let before: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block \
                 WHERE kind = 'canonical' AND btc_height = 3",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(
            before, 2,
            "Core-backed event reconciliation promotes N before O is demoted"
        );

        let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
        repair_near_tip_backbone_for_test(
            &mut client,
            &active_source,
            active_source.tip().await?,
            Duration::ZERO,
            4,
        )
        .await?;

        let rows = client
            .query(
                "SELECT btc_header_hash, kind FROM block \
                 WHERE btc_header_hash IN ($1, $2) ORDER BY btc_header_hash",
                &[&old_h3, &new_h3],
            )
            .await?;
        assert_eq!(rows.len(), 2);
        for row in rows {
            let hash: Vec<u8> = row.get(0);
            let expected_kind = if hash == new_h3 { "canonical" } else { "stale" };
            assert_eq!(row.get::<_, String>(1), expected_kind);
        }
        assert_completed_repair_state(&client, 4, active[&4].block_hash()).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn suffix_revalidates_target_after_waiting_for_exclusive_barrier() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let original = test_header_chain(4, 1_800_027_100);
        let original_source = FakeBitcoinCoreBackboneSource::new(3, original.clone());
        run_sync_bitcoin_core(
            &mut client,
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

        let captured = fork_header_chain(&original, 3, 4, 1_800_027_200);
        let current = fork_header_chain(&original, 3, 4, 1_800_027_300);
        let source = MovingTargetBackboneSource::new(4, captured.clone(), current.clone());
        let target = source.tip().await?;
        let (locked_tx, locked_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let blocker = tokio::spawn(hold_shared_core_view_barrier(
            schema.clone(),
            locked_tx,
            release_rx,
        ));
        locked_rx
            .await
            .context("shared barrier blocker ended before locking")?;

        let mut repair_client = crate::support::db::connect_to_schema(&schema).await?;
        let repair_pid: i32 = repair_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let repair_source = source.clone();
        let repair = tokio::spawn(async move {
            repair_near_tip_backbone_for_test(
                &mut repair_client,
                &repair_source,
                target,
                Duration::ZERO,
                4,
            )
            .await
        });
        wait_for_exclusive_core_view_barrier(&client, repair_pid).await?;
        source.move_target();
        release_tx
            .send(())
            .map_err(|()| anyhow::anyhow!("shared barrier blocker already ended"))?;
        blocker.await.expect("join shared barrier blocker")?;

        let err = repair
            .await
            .expect("join suffix repair")
            .expect_err("target movement while waiting must abort before suffix mutation");
        assert!(
            format!("{err:#}").contains("target moved during near-tip repair"),
            "unexpected repair error: {err:#}"
        );
        assert_target_move_left_suffix_unmodified(&client, &original, &captured, &current).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn delayed_ordinary_writer_preserves_pending_suffix_reconcile_state() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let original = test_header_chain(4, 1_800_027_400);
        let original_source = FakeBitcoinCoreBackboneSource::new(3, original.clone());
        run_sync_bitcoin_core(
            &mut client,
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

        let delayed_source = SwitchingBackboneSource::new(3, original.clone(), 3, original.clone());
        let writer_source = delayed_source.clone();
        let mut writer_client = crate::support::db::connect_to_schema(&schema).await?;
        let writer = tokio::spawn(async move {
            run_sync_bitcoin_core(
                &mut writer_client,
                &writer_source,
                one_height_overwrite_config(3),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), delayed_source.wait_for_coinbase())
            .await
            .expect("ordinary writer did not pause after fetching the old Core hash");

        let active = fork_header_chain(&original, 3, 4, 1_800_027_500);
        let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
        let expected = canonical_view(&client, 2, 4).await?;
        let replacements = vec![
            CoreCanonicalReplacement {
                height: 3,
                header: active[&3],
                coinbase: active_source
                    .block_coinbase(active[&3].block_hash())
                    .await?,
            },
            CoreCanonicalReplacement {
                height: 4,
                header: active[&4],
                coinbase: active_source
                    .block_coinbase(active[&4].block_hash())
                    .await?,
            },
        ];
        replace_core_canonical_suffix(&mut client, bitcoin, 3, 2, &expected, &replacements).await?;
        assert_pending_core_queue(&client, bitcoin, 3).await?;

        delayed_source.switch_and_release();
        let err = writer
            .await
            .expect("join delayed ordinary writer")
            .expect_err("pending suffix reconciliation must reject ordinary Core writes");
        assert!(err.to_string().contains("reconcile queue is pending"));

        let old_h3 = header_hash_bytes(&original[&3]);
        let new_h3 = header_hash_bytes(&active[&3]);
        let rows = client
            .query(
                "SELECT btc_header_hash, kind FROM block \
                 WHERE btc_header_hash IN ($1, $2) ORDER BY btc_header_hash",
                &[&old_h3, &new_h3],
            )
            .await?;
        assert_eq!(rows.len(), 2);
        for row in rows {
            let hash: Vec<u8> = row.get(0);
            let expected_kind = if hash == new_h3 { "canonical" } else { "stale" };
            assert_eq!(row.get::<_, String>(1), expected_kind);
        }
        let error_code: Option<String> = client
            .query_one(
                "SELECT last_error_code FROM bitcoin_core_sync_state \
                 WHERE source_id = $1 AND sync_mode = 'contiguous'",
                &[&bitcoin],
            )
            .await?
            .get(0);
        assert_eq!(
            error_code.as_deref(),
            Some("backbone_reorg_reconcile_pending")
        );
        assert_pending_core_queue(&client, bitcoin, 3).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn sync_progress_does_not_clear_a_concurrent_unrelated_error() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let original = test_header_chain(4, 1_800_027_600);
        let original_source = FakeBitcoinCoreBackboneSource::new(3, original.clone());
        run_sync_bitcoin_core(
            &mut client,
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

        let delayed_source = SwitchingBackboneSource::new(3, original.clone(), 3, original.clone());
        let writer_source = delayed_source.clone();
        let mut writer_client = crate::support::db::connect_to_schema(&schema).await?;
        let writer_pid: i32 = writer_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let writer = tokio::spawn(async move {
            run_sync_bitcoin_core(
                &mut writer_client,
                &writer_source,
                one_height_overwrite_config(3),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), delayed_source.wait_for_coinbase())
            .await
            .expect("ordinary writer did not pause before progress update");

        let mut blocker_client = crate::support::db::connect_to_schema(&schema).await?;
        let blocker_pid: i32 = blocker_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let blocker = blocker_client.transaction().await?;
        blocker
            .query_one(
                "SELECT 1 FROM bitcoin_core_sync_state \
                 WHERE source_id = $1 AND sync_mode = 'contiguous' FOR UPDATE",
                &[&bitcoin],
            )
            .await?;

        delayed_source.switch_and_release();
        wait_for_backend_blocked_by(&client, writer_pid, blocker_pid).await?;
        blocker
            .execute(
                "UPDATE bitcoin_core_sync_state \
                 SET last_error_code = 'concurrent_error', last_error_height = 99, \
                     last_error = 'concurrent error', last_error_details = '{}'::jsonb \
                 WHERE source_id = $1 AND sync_mode = 'contiguous'",
                &[&bitcoin],
            )
            .await?;
        blocker.commit().await?;
        writer.await.expect("join ordinary writer")?;

        let error = client
            .query_one(
                "SELECT last_error_code, last_error_height \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(
            error.get::<_, Option<String>>(0).as_deref(),
            Some("concurrent_error")
        );
        assert_eq!(error.get::<_, Option<i32>>(1), Some(99));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn accepting_live_target_does_not_clear_a_concurrent_error() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let headers = test_header_chain(3, 1_800_027_700);
        let original_source = FakeBitcoinCoreBackboneSource::new(2, headers);
        run_sync_bitcoin_core(
            &mut client,
            &original_source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(2),
                tip: true,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;
        let target = original_source.tip().await?;
        let gated_source = GatedTipBackboneSource::new(original_source);

        let mut accept_client = crate::support::db::connect_to_schema(&schema).await?;
        let accept_pid: i32 = accept_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let accept_source = gated_source.clone();
        let accept = tokio::spawn(async move {
            accept_live_repaired_target_for_test(
                &mut accept_client,
                &accept_source,
                bitcoin,
                target,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), gated_source.wait_for_tip())
            .await
            .expect("live-target acceptance did not reach its gated Core check");

        let error_client = crate::support::db::connect_to_schema(&schema).await?;
        let error_pid: i32 = error_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let error_write = tokio::spawn(async move {
            error_client
                .execute(
                    "UPDATE bitcoin_core_sync_state \
                     SET last_error_code = 'concurrent_error', last_error_height = 99, \
                         last_error = 'concurrent error', last_error_details = '{}'::jsonb \
                     WHERE source_id = $1 AND sync_mode = 'contiguous'",
                    &[&bitcoin],
                )
                .await
        });
        wait_for_backend_blocked_by(&client, error_pid, accept_pid).await?;
        gated_source.release_tip();
        accept.await.expect("join live-target acceptance")?;
        assert_eq!(error_write.await.expect("join concurrent error write")?, 1);

        let error = client
            .query_one(
                "SELECT last_error_code, last_error_height \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(
            error.get::<_, Option<String>>(0).as_deref(),
            Some("concurrent_error")
        );
        assert_eq!(error.get::<_, Option<i32>>(1), Some(99));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn follow_repair_refuses_reorg_beyond_window_without_chain_mutation() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(4, 1_800_030_000);
        let original_source = FakeBitcoinCoreBackboneSource::new(4, original.clone());
        run_sync_bitcoin_core(
            &mut client,
            &original_source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(4),
                tip: true,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;
        let active = fork_header_chain(&original, 2, 4, 1_800_031_000);
        let old_h2 = header_hash_bytes(&original[&2]);
        let new_h2 = header_hash_bytes(&active[&2]);
        let active_source = FakeBitcoinCoreBackboneSource::new(4, active);

        let err = repair_near_tip_backbone_for_test(
            &mut client,
            &active_source,
            active_source.tip().await?,
            Duration::ZERO,
            2,
        )
        .await
        .expect_err("window plus one reorg must fail closed");
        assert!(err.to_string().contains("no complete matching anchor"));

        let old_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&old_h2],
            )
            .await?
            .get(0);
        assert_eq!(old_kind, "canonical");
        let new_rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&new_h2],
            )
            .await?
            .get(0);
        assert_eq!(new_rows, 0);
        let error = client
            .query_one(
                "SELECT last_error_code, last_error_details FROM bitcoin_core_sync_state",
                &[],
            )
            .await?;
        assert_eq!(
            error.get::<_, Option<String>>(0).as_deref(),
            Some("near_tip_reorg_repair_failed")
        );
        let details: Json<serde_json::Value> = error.get(1);
        assert_eq!(details.0["reason"], json!("common_ancestor_outside_window"));
        let queued: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM bitcoin_core_reconcile_queue",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(queued, 0);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn follow_repair_diagnoses_stale_cursor_below_current_view() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(9, 1_800_035_000);
        let original_source = FakeBitcoinCoreBackboneSource::new(2, original.clone());
        run_sync_bitcoin_core(
            &mut client,
            &original_source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(2),
                tip: true,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        let active = fork_header_chain(&original, 2, 8, 1_800_036_000);
        let old_h2 = header_hash_bytes(&original[&2]);
        let active_h5 = header_hash_bytes(&active[&5]);
        let active_source = FakeBitcoinCoreBackboneSource::new(8, active);
        let err = repair_near_tip_backbone_for_test(
            &mut client,
            &active_source,
            active_source.tip().await?,
            Duration::ZERO,
            4,
        )
        .await
        .expect_err("a stale cursor below the repair view must fail before gap fill");

        assert!(err.to_string().contains("lies below bounded view start"));
        let old_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&old_h2],
            )
            .await?
            .get(0);
        assert_eq!(old_kind, "canonical");
        let active_rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&active_h5],
            )
            .await?
            .get(0);
        assert_eq!(
            active_rows, 0,
            "out-of-window detection mutates no newer rows"
        );
        let details: Json<serde_json::Value> = client
            .query_one(
                "SELECT last_error_details FROM bitcoin_core_sync_state",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(details.0["reason"], json!("common_ancestor_outside_window"));
        assert_eq!(details.0["first_conflict_height"], json!(2));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn core_suffix_queue_preserves_pending_status_and_survives_restart_drain() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let original = test_header_chain(4, 1_800_040_000);
        let original_source = FakeBitcoinCoreBackboneSource::new(3, original.clone());
        run_sync_bitcoin_core(
            &mut client,
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

        let active = fork_header_chain(&original, 3, 4, 1_800_041_000);
        insert_matching_upper_canonical(&client, &active[&4], 4).await?;
        let new_h3 = header_hash_bytes(&active[&3]);
        let (dependent_hash, grandchild_hash) =
            insert_two_hop_core_dependents(&client, namecoin, new_h3).await?;

        let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
        let expected = canonical_view(&client, 2, 4).await?;
        let replacements = vec![
            CoreCanonicalReplacement {
                height: 3,
                header: active[&3],
                coinbase: active_source
                    .block_coinbase(active[&3].block_hash())
                    .await?,
            },
            CoreCanonicalReplacement {
                height: 4,
                header: active[&4],
                coinbase: active_source
                    .block_coinbase(active[&4].block_hash())
                    .await?,
            },
        ];
        replace_core_canonical_suffix(&mut client, bitcoin, 3, 2, &expected, &replacements).await?;

        assert_pending_core_queue(&client, bitcoin, 3).await?;
        let pending_details: Json<serde_json::Value> = client
            .query_one(
                "SELECT last_error_details FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?
            .get(0);
        record_retryable_repair_failure_for_test(&mut client, bitcoin).await?;
        let preserved = client
            .query_one(
                "SELECT last_error_code, last_error_details \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(
            preserved.get::<_, Option<String>>(0).as_deref(),
            Some("backbone_reorg_reconcile_pending")
        );
        assert_eq!(
            preserved.get::<_, Json<serde_json::Value>>(1).0,
            pending_details.0,
            "retryable repair bookkeeping must not replace durable queue status"
        );
        drain_until_partial_core_frontier(&mut client, bitcoin, &dependent_hash, &grandchild_hash)
            .await?;

        let mut restart_client = crate::support::db::connect_to_schema(&schema).await?;
        drain_core_reconcile_queue(&mut restart_client, bitcoin).await?;
        assert_drained_core_queue(&restart_client, bitcoin).await?;
        let grandchild_rows: i64 = restart_client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&grandchild_hash],
            )
            .await?
            .get(0);
        assert_eq!(grandchild_rows, 1, "restart drain reaches the grandchild");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn core_suffix_reenqueue_resets_expansion_seed_to_primary_phase() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let original = test_header_chain(2, 1_800_050_000);
        let original_source = FakeBitcoinCoreBackboneSource::new(2, original.clone());
        run_sync_bitcoin_core(
            &mut client,
            &original_source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(2),
                tip: true,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        let active = fork_header_chain(&original, 2, 2, 1_800_051_000);
        let active_source = FakeBitcoinCoreBackboneSource::new(2, active.clone());
        let replacement_hash = header_hash_bytes(&active[&2]);
        let replacements = vec![CoreCanonicalReplacement {
            height: 2,
            header: active[&2],
            coinbase: active_source
                .block_coinbase(active[&2].block_hash())
                .await?,
        }];
        let expected = canonical_view(&client, 1, 2).await?;
        replace_core_canonical_suffix(&mut client, bitcoin, 2, 1, &expected, &replacements).await?;

        let initial = client
            .query_one(
                "SELECT primary_pending, generation \
                 FROM bitcoin_core_reconcile_queue \
                 WHERE source_id = $1 AND btc_parent_header_hash = $2",
                &[&bitcoin, &replacement_hash],
            )
            .await?;
        assert!(
            initial.get::<_, bool>(0),
            "a fresh suffix seed starts in primary phase"
        );
        assert_eq!(initial.get::<_, i64>(1), 1);

        let marked = client
            .execute(
                "UPDATE bitcoin_core_reconcile_queue SET primary_pending = FALSE \
                 WHERE source_id = $1 AND btc_parent_header_hash = $2",
                &[&bitcoin, &replacement_hash],
            )
            .await?;
        assert_eq!(marked, 1, "test seed enters expansion phase");

        let expected = canonical_view(&client, 1, 2).await?;
        replace_core_canonical_suffix(&mut client, bitcoin, 2, 1, &expected, &replacements).await?;
        let requeued = client
            .query_one(
                "SELECT primary_pending, generation \
                 FROM bitcoin_core_reconcile_queue \
                 WHERE source_id = $1 AND btc_parent_header_hash = $2",
                &[&bitcoin, &replacement_hash],
            )
            .await?;
        assert!(
            requeued.get::<_, bool>(0),
            "a newer generation returns an expansion-ready seed to primary phase"
        );
        assert_eq!(requeued.get::<_, i64>(1), 2);

        Ok::<_, anyhow::Error>(())
    })
}
