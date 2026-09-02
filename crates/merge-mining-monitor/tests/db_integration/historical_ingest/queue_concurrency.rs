use super::*;

#[tokio::test]
async fn orphan_historical_reconcile_locks_parent_before_queue_row() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let parent_hash = vec![0x64; 32];
        client
            .execute(
                "INSERT INTO historical_reconcile_queue (btc_parent_header_hash) VALUES ($1)",
                &[&parent_hash],
            )
            .await?;

        let mut blocker_client = crate::support::db::connect_to_schema(&schema).await?;
        let blocker_txn = blocker_client.transaction().await?;
        mmm_read_model::lock_block_hash(&blocker_txn, &parent_hash).await?;

        let mut worker_client = crate::support::db::connect_to_schema(&schema).await?;
        let worker_pid: i32 = worker_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let worker = tokio::spawn(async move {
            let classifier = ConfiguredParentClassifier::Disabled;
            let classifications = std::collections::HashMap::new();
            drain_historical_reconcile_queue_with_budget_for_test(
                &mut worker_client,
                &classifier,
                &classifications,
                8,
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let waiting: bool = client
                    .query_one(
                        "SELECT EXISTS ( \
                             SELECT 1 FROM pg_catalog.pg_locks \
                             WHERE pid = $1 AND locktype = 'advisory' \
                               AND mode = 'ExclusiveLock' AND NOT granted \
                         )",
                        &[&worker_pid],
                    )
                    .await
                    .context("query historical worker advisory-lock wait")?
                    .get(0);
                if waiting {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("historical worker did not wait for the parent block lock")??;

        // The worker is blocked on the parent advisory lock. It must not yet
        // own the queue row, otherwise this block-owning transaction and the
        // worker form the queue -> block -> queue deadlock that the orphan path
        // used to permit.
        let enqueue_result = tokio::time::timeout(
            Duration::from_secs(5),
            enqueue_historical_parent_reconcile(&blocker_txn, &parent_hash),
        )
        .await;

        blocker_txn.rollback().await?;
        let worker_result = tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .context("historical worker did not finish after the parent lock was released")?
            .context("join historical reconcile worker")?;
        enqueue_result
            .context("historical enqueue blocked behind the orphan worker's queue row")??;
        worker_result?;

        let remaining: i64 = client
            .query_one("SELECT count(*) FROM historical_reconcile_queue", &[])
            .await?
            .get(0);
        assert_eq!(remaining, 0);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn invalidated_cached_canonical_with_unknown_recheck_stays_pending() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let cached_header = header_meeting_bits(0x207f_ffff, 1_700_000_064, 64);
        let current_header = header_meeting_bits(0x207f_ffff, 1_700_000_065, 65);
        let cached_hash = cached_header.block_hash().to_byte_array().to_vec();
        let current_hash = current_header.block_hash().to_byte_array().to_vec();
        let height = 700_064;
        let event_id = seed_identity_event(
            &client,
            "auxpow:devcoin",
            &cached_header,
            64,
            Some(vec![0x64; 32]),
        )
        .await?;
        client
            .execute(
                "UPDATE merge_mining_event \
                 SET btc_parent_kind = 'canonical', btc_parent_height = $2, \
                     difficulty_epoch_ok = TRUE \
                 WHERE id = $1",
                &[&event_id, &height],
            )
            .await?;
        insert_block(
            &client,
            &current_hash,
            &current_header.prev_blockhash.to_byte_array(),
            Some(height),
            "canonical",
            i64::from(current_header.time),
            None,
        )
        .await?;
        enqueue_historical_parent_reconcile(&client, &cached_hash).await?;

        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            unknown_verdict(&cached_header),
        ));
        let classifications = std::collections::HashMap::from([(
            cached_hash.clone(),
            canonical_verdict(&cached_header, height),
        )]);
        let error = drain_historical_reconcile_queue(&mut client, &classifier, &classifications)
            .await
            .expect_err("an unresolved invalidated cache entry must remain pending");
        assert!(
            error
                .to_string()
                .contains("fresh classification is unknown")
        );

        let primary_pending: bool = client
            .query_one(
                "SELECT primary_pending FROM historical_reconcile_queue \
                 WHERE btc_parent_header_hash = $1",
                &[&cached_hash],
            )
            .await?
            .get(0);
        assert!(primary_pending);
        let canonical_hashes = client
            .query(
                "SELECT btc_header_hash FROM block \
                 WHERE kind = 'canonical' AND btc_height = $1 \
                 ORDER BY btc_header_hash",
                &[&height],
            )
            .await?
            .into_iter()
            .map(|row| row.get::<_, Vec<u8>>(0))
            .collect::<Vec<_>>();
        assert_eq!(canonical_hashes, vec![current_hash]);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn invalidated_cached_stale_with_unknown_competitor_stays_pending() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let stale_header = header_meeting_bits(0x207f_ffff, 1_700_000_088, 88);
        let competitor_header = header_meeting_bits(0x207f_ffff, 1_700_000_089, 89);
        let stale_hash = stale_header.block_hash().to_byte_array().to_vec();
        let competitor_hash = competitor_header.block_hash().to_byte_array().to_vec();
        let height = 700_088;
        let event_id = seed_identity_event(
            &client,
            "auxpow:devcoin",
            &stale_header,
            88,
            Some(vec![0x88; 32]),
        )
        .await?;
        client
            .execute(
                "UPDATE merge_mining_event \
                 SET btc_parent_kind = 'stale', btc_parent_height = $2, \
                     difficulty_epoch_ok = TRUE \
                 WHERE id = $1",
                &[&event_id, &height],
            )
            .await?;
        insert_block(
            &client,
            &competitor_hash,
            &competitor_header.prev_blockhash.to_byte_array(),
            None,
            "unknown",
            i64::from(competitor_header.time),
            None,
        )
        .await?;
        enqueue_historical_parent_reconcile(&client, &stale_hash).await?;

        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            unknown_verdict(&stale_header),
        ));
        let classifications = std::collections::HashMap::from([(
            stale_hash.clone(),
            crate::support::scenario::stale_verdict(&stale_header, height, competitor_hash.clone()),
        )]);
        let error = drain_historical_reconcile_queue(&mut client, &classifier, &classifications)
            .await
            .expect_err("a stale cache with a demoted competitor must remain pending");
        assert!(
            error
                .to_string()
                .contains("fresh classification is unknown")
        );

        let primary_pending: bool = client
            .query_one(
                "SELECT primary_pending FROM historical_reconcile_queue \
                 WHERE btc_parent_header_hash = $1",
                &[&stale_hash],
            )
            .await?
            .get(0);
        assert!(primary_pending);
        let competitor_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&competitor_hash],
            )
            .await?
            .get(0);
        assert_eq!(competitor_kind, "unknown");
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn anchored_historical_reconcile_locks_all_parent_events_before_queue() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_066, 66);
        let parent_hash = header.block_hash().to_byte_array().to_vec();
        seed_identity_event(&client, "auxpow:devcoin", &header, 66, Some(vec![0x66; 32])).await?;
        let second_event_id =
            seed_identity_event(&client, "auxpow:devcoin", &header, 67, Some(vec![0x67; 32]))
                .await?;
        enqueue_historical_parent_reconcile(&client, &parent_hash).await?;

        let mut blocker_client = crate::support::db::connect_to_schema(&schema).await?;
        let blocker_pid: i32 = blocker_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let blocker_txn = blocker_client.transaction().await?;
        blocker_txn
            .query_one(
                "SELECT id FROM merge_mining_event WHERE id = $1 FOR UPDATE",
                &[&second_event_id],
            )
            .await?;

        let mut worker_client = crate::support::db::connect_to_schema(&schema).await?;
        let worker_pid: i32 = worker_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let worker = tokio::spawn(async move {
            let classifier = ConfiguredParentClassifier::Disabled;
            let classifications = std::collections::HashMap::new();
            drain_historical_reconcile_queue(&mut worker_client, &classifier, &classifications)
                .await
        });

        wait_until_backend_is_blocked_by(
            &client,
            worker_pid,
            blocker_pid,
            "historical worker did not wait for the non-anchor event lock",
        )
        .await?;

        let enqueue_result = tokio::time::timeout(
            Duration::from_secs(5),
            enqueue_historical_parent_reconcile(&blocker_txn, &parent_hash),
        )
        .await;
        let blocker_commit = blocker_txn.commit().await;
        let worker_result = tokio::time::timeout(Duration::from_secs(5), worker)
            .await
            .context("historical worker did not finish after all event locks were released")?
            .context("join anchored historical reconcile worker")?;

        enqueue_result
            .context("historical enqueue blocked behind a worker that did not lock all events")??;
        blocker_commit?;
        worker_result?;
        let remaining: i64 = client
            .query_one("SELECT count(*) FROM historical_reconcile_queue", &[])
            .await?
            .get(0);
        assert_eq!(remaining, 0);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn authoritative_removal_locks_event_before_queue() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_068, 68);
        let parent_hash = header.block_hash().to_byte_array().to_vec();
        let event_id =
            seed_identity_event(&client, "auxpow:devcoin", &header, 68, Some(vec![0x68; 32]))
                .await?;
        let source_id = get_source_id(&client, "auxpow:devcoin").await?;
        enqueue_historical_parent_reconcile(&client, &parent_hash).await?;

        let mut blocker_client = crate::support::db::connect_to_schema(&schema).await?;
        let blocker_pid: i32 = blocker_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let blocker_txn = blocker_client.transaction().await?;
        blocker_txn
            .query_one(
                "SELECT id FROM merge_mining_event WHERE id = $1 FOR UPDATE",
                &[&event_id],
            )
            .await?;

        let mut removal_client = crate::support::db::connect_to_schema(&schema).await?;
        let removal_pid: i32 = removal_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let removal = tokio::spawn(async move {
            let txn = removal_client.transaction().await?;
            let removed = mmm_read_model::reconcile_authoritative_historical_source_in_transaction(
                &txn,
                source_id,
                pinned_publication_ref(),
                "devcoin",
            )
            .await?;
            txn.commit().await?;
            Ok::<_, anyhow::Error>(removed)
        });

        wait_until_backend_is_blocked_by(
            &client,
            removal_pid,
            blocker_pid,
            "authoritative removal did not wait for the event lock",
        )
        .await?;

        let enqueue_result = tokio::time::timeout(
            Duration::from_secs(5),
            enqueue_historical_parent_reconcile(&blocker_txn, &parent_hash),
        )
        .await;
        let blocker_commit = blocker_txn.commit().await;
        let removed = tokio::time::timeout(Duration::from_secs(5), removal)
            .await
            .context("authoritative removal did not finish after the event lock was released")?
            .context("join authoritative historical removal")??;

        enqueue_result
            .context("historical enqueue blocked behind authoritative removal's queue row")??;
        blocker_commit?;
        assert_eq!(removed, 1);
        let event_exists: bool = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM merge_mining_event WHERE id = $1)",
                &[&event_id],
            )
            .await?
            .get(0);
        assert!(!event_exists);
        let primary_pending: bool = client
            .query_one(
                "SELECT primary_pending FROM historical_reconcile_queue \
                 WHERE btc_parent_header_hash = $1",
                &[&parent_hash],
            )
            .await?
            .get(0);
        assert!(primary_pending);
        Ok::<_, anyhow::Error>(())
    })
}

async fn wait_until_backend_is_blocked_by(
    client: &tokio_postgres::Client,
    waiting_pid: i32,
    blocker_pid: i32,
    timeout_message: &str,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: bool = client
                .query_one(
                    "SELECT $2 = ANY(pg_catalog.pg_blocking_pids($1))",
                    &[&waiting_pid, &blocker_pid],
                )
                .await?
                .get(0);
            if blocked {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .with_context(|| timeout_message.to_owned())?
}

#[tokio::test]
async fn multi_chain_import_reuses_the_parent_classification_cache() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_036, 36);
        let ixcoin_path = write_normalized_csv_for_chain(
            "ixcoin",
            &header,
            "canonical",
            "",
            "canonical_parent",
            &[],
            700_036,
        )?;
        let devcoin_path = write_normalized_csv_for_chain(
            "devcoin",
            &header,
            "canonical",
            "",
            "canonical_parent",
            &[],
            700_036,
        )?;
        let result = async {
            let fake = FakeParentClassifier::new(canonical_verdict(&header, 700_036));
            let classifier = ConfiguredParentClassifier::Fake(fake.clone());
            let summary = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                two_chain_import_configs(&ixcoin_path, &devcoin_path),
            )
            .await?;

            assert_eq!(summary.chains.len(), 2);
            assert_eq!(summary.stale_branches_reconciled, 0);
            assert_eq!(
                fake.call_count().await,
                1,
                "the repeated parent must be classified once across both chains"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&ixcoin_path, &devcoin_path])
    })
}
