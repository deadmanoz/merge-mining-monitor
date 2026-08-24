use anyhow::Result;
use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};
use mmm_bitcoin_core::{
    BitcoinCoreBlockCoinbase, ConfiguredParentClassifier, CoreHeader, FakeParentClassifier,
    ParentClassification,
};
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::capture::ClassificationProof;
use mmm_capture::nbits_table::NbitsLookup;
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use mmm_producers::refresh_bitcoin_core_header_cache;
use mmm_read_model::{
    CoreCanonicalReplacement, ExpectedCoreCanonicalRow, lock_block_hash, rebuild_source_health,
    reconcile_from_merge_mining_event, replace_core_canonical_suffix, revoke_merge_mining_event,
    run_exclusive_core_canonical_view_transaction,
};
use mmm_store::{
    BitcoinCoreHeader, complete_bitcoin_core_header_cache_reclassification,
    finish_bitcoin_core_header_cache_operation, get_source_id,
    is_bitcoin_core_header_cache_integrity_error, load_bitcoin_core_nbits_table,
    load_bitcoin_core_nbits_table_if_present, lock_bitcoin_core_header_cache,
    lock_bitcoin_core_header_cache_shared_in_transaction, record_bitcoin_core_header,
    replace_bitcoin_core_header_cache, upsert_merge_mining_event,
};

use crate::support::db::connect_to_schema;
use crate::support::scenario::orphan_candidate_verdict;
use crate::support::seed::{insert_block, test_header_chain};
use crate::support::{namecoin_event_payload, namecoin_fixture};

fn header(height: i32, hash_byte: u8, block_time: i64, bits: u32) -> BitcoinCoreHeader {
    BitcoinCoreHeader {
        height,
        block_hash: vec![hash_byte; 32],
        block_time,
        bits,
    }
}

fn core_header(height: i32, hash_byte: u8, block_time: i64, bits: u32) -> CoreHeader {
    CoreHeader {
        height,
        hash: BlockHash::from_byte_array([hash_byte; 32]),
        header_time: block_time,
        nbits: bits,
    }
}

struct ReplaceableCoreSuffix {
    source_id: i64,
    expected: Vec<ExpectedCoreCanonicalRow>,
    replacement_header: Header,
    replacements: Vec<CoreCanonicalReplacement>,
}

async fn seed_replaceable_core_suffix(
    client: &tokio_postgres::Client,
) -> Result<ReplaceableCoreSuffix> {
    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let original = test_header_chain(1, 1_800_060_000);
    for height in 0..=1 {
        let header = original[&height];
        insert_block(
            client,
            &header.block_hash().to_byte_array(),
            &header.prev_blockhash.to_byte_array(),
            Some(height),
            "canonical",
            i64::from(header.time),
            None,
        )
        .await?;
    }
    let old_tip_hash = original[&1].block_hash().to_byte_array().to_vec();
    client
        .execute(
            "INSERT INTO bitcoin_core_sync_state ( \
                source_id, sync_mode, target_tip_height, target_tip_hash, \
                contiguous_complete_height, created_at, updated_at \
             ) VALUES ($1, 'contiguous', 1, $2, 1, 1, 1)",
            &[&source_id, &old_tip_hash],
        )
        .await?;
    let expected = (0..=1)
        .map(|height| ExpectedCoreCanonicalRow {
            height,
            hash: original[&height].block_hash().to_byte_array().to_vec(),
            prev_hash: original[&height].prev_blockhash.to_byte_array().to_vec(),
        })
        .collect::<Vec<_>>();
    let replacement_header = Header {
        version: Version::ONE,
        prev_blockhash: original[&0].block_hash(),
        merkle_root: TxMerkleNode::all_zeros(),
        time: original[&1].time + 100,
        bits: CompactTarget::from_consensus(0x207f_ffff),
        nonce: 50_001,
    };
    let replacements = vec![CoreCanonicalReplacement {
        height: 1,
        header: replacement_header,
        coinbase: BitcoinCoreBlockCoinbase {
            txid: vec![0x71; 32],
            script: vec![0x51],
            outputs: Vec::new(),
        },
    }];
    Ok(ReplaceableCoreSuffix {
        source_id,
        expected,
        replacement_header,
        replacements,
    })
}

#[tokio::test]
async fn core_header_cache_retains_epochs_replaces_horizon_and_rejects_conflicts() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // `new_test_db` supplies this genesis row. An identical observation is
        // harmless, while a conflicting row below must fail closed.
        record_bitcoin_core_header(&client, &header(0, 0, 1, 0x1d00_ffff)).await?;
        record_bitcoin_core_header(&client, &header(2016, 1, 2, 0x1c00_ffff)).await?;
        replace_bitcoin_core_header_cache(
            &mut client,
            2016,
            &[],
            None,
            &header(2020, 2, 3, 0x1c00_ffff),
            false,
        )
        .await?;
        replace_bitcoin_core_header_cache(
            &mut client,
            2016,
            &[],
            None,
            &header(2030, 3, 4, 0x1c00_ffff),
            false,
        )
        .await?;

        let table = load_bitcoin_core_nbits_table(&client).await?;
        assert_eq!(table.horizon_height(), 2030);
        assert_eq!(table.expected_nbits(2017), NbitsLookup::Found(0x1c00_ffff));

        let stale_horizons: i64 = client
            .query_one(
                "SELECT count(*) FROM bitcoin_core_header WHERE height % 2016 <> 0",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(stale_horizons, 1);

        let err = record_bitcoin_core_header(&client, &header(2016, 1, 2, 0x1c00_fffe))
            .await
            .expect_err("a conflicting Core observation must fail closed");
        assert!(err.to_string().contains("disagrees"));
        assert!(is_bitcoin_core_header_cache_integrity_error(&err));
        Ok(())
    })
}

#[tokio::test]
async fn cache_refresh_keeps_timestamp_coverage_and_retries_an_unacknowledged_sweep() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        client
            .execute(
                "UPDATE bitcoin_core_header_cache_state \
                 SET horizon_time = 0, reclassification_needed = FALSE",
                &[],
            )
            .await?;
        let first = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[header(0, 0, 1, 0x1d00_ffff)],
            None,
            &header(100, 1, 100, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(first.reclassification_needed);
        assert!(
            first.recheck_orphans,
            "initial Core-cache population revisits classifications made before the cache existed"
        );
        complete_bitcoin_core_header_cache_reclassification(&client).await?;

        let advanced_with_an_older_timestamp = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[],
            None,
            &header(101, 2, 99, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(advanced_with_an_older_timestamp.reclassification_needed);
        assert!(
            !advanced_with_an_older_timestamp.recheck_orphans,
            "ordinary horizon advances do not revisit already classified orphans"
        );
        assert_eq!(
            load_bitcoin_core_nbits_table(&client).await?.horizon_time(),
            100
        );

        let retry = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[],
            None,
            &header(101, 2, 99, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(retry.reclassification_needed);
        complete_bitcoin_core_header_cache_reclassification(&client).await?;

        let settled = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[],
            None,
            &header(101, 2, 99, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(!settled.reclassification_needed);

        let boundary_overlaps_existing_coverage = replace_bitcoin_core_header_cache(
            &mut client,
            2016,
            &[header(2016, 3, 99, 0x1c00_ffff)],
            None,
            &header(2116, 4, 101, 0x1c00_ffff),
            false,
        )
        .await?;
        assert!(
            boundary_overlaps_existing_coverage.recheck_orphans,
            "a new retarget boundary inside prior timestamp coverage can change existing verdicts"
        );
        Ok(())
    })
}

#[tokio::test]
async fn fresh_database_refresh_without_sync_state_reads_core_headers() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let sync_state_count: i64 = client
            .query_one("SELECT count(*)::bigint FROM bitcoin_core_sync_state", &[])
            .await?
            .get(0);
        assert_eq!(sync_state_count, 0);
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        assert!(
            load_bitcoin_core_nbits_table_if_present(&client)
                .await?
                .is_none()
        );
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );

        let table = refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        assert_eq!(table.horizon_height(), 2030);
        assert_eq!(table.expected_nbits(2017), NbitsLookup::Found(0x1c00_ffff));

        let heights = client
            .query(
                "SELECT height FROM bitcoin_core_header ORDER BY height",
                &[],
            )
            .await?
            .iter()
            .map(|row| row.get::<_, i32>(0))
            .collect::<Vec<_>>();
        assert_eq!(heights, [0, 2016, 2030]);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_drains_pending_core_reconcile_before_reading_core_snapshot() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let queued_hash = vec![0x52_u8; 32];
        client
            .execute(
                "INSERT INTO bitcoin_core_sync_state ( \
                    source_id, sync_mode, contiguous_complete_height, \
                    last_error_code, last_error, last_error_details, created_at, updated_at \
                 ) VALUES ($1, 'contiguous', -1, \
                           'backbone_reorg_reconcile_pending', 'pending cascade', \
                           jsonb_build_object('queued', 1), 1, 1)",
                &[&bitcoin],
            )
            .await?;
        client
            .execute(
                "INSERT INTO bitcoin_core_reconcile_queue ( \
                    source_id, btc_parent_header_hash \
                 ) VALUES ($1, $2)",
                &[&bitcoin, &queued_hash],
            )
            .await?;

        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_error(),
        );
        let error = refresh_bitcoin_core_header_cache(&mut client, &classifier)
            .await
            .expect_err("the injected Core snapshot error must surface after queue drain");
        assert!(
            error
                .to_string()
                .contains("fake classifier: injected synced_tip error")
        );

        let queue_count: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM bitcoin_core_reconcile_queue \
                 WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?
            .get(0);
        assert_eq!(queue_count, 0);
        let pending_error = client
            .query_one(
                "SELECT last_error_code, last_error, last_error_details \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(pending_error.get::<_, Option<String>>(0), None);
        assert_eq!(pending_error.get::<_, Option<String>>(1), None);
        assert_eq!(
            pending_error.get::<_, serde_json::Value>(2),
            serde_json::json!({})
        );
        Ok(())
    })
}

#[tokio::test]
async fn refresh_rejects_a_non_mainnet_core_node() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_non_mainnet_synced_tip(2030),
        );
        let error = refresh_bitcoin_core_header_cache(&mut client, &classifier)
            .await
            .expect_err("testnet, signet, and regtest must not populate the mainnet cache");
        assert!(error.to_string().contains("connected to mainnet"));
        Ok(())
    })
}

#[tokio::test]
async fn core_header_cache_refresh_lock_serializes_sessions() -> Result<()> {
    crate::run_db_test!(client, schema, {
        lock_bitcoin_core_header_cache(&client).await?;
        let waiting_client = connect_to_schema(&schema).await?;
        let mut waiter = tokio::spawn(async move {
            lock_bitcoin_core_header_cache(&waiting_client).await?;
            finish_bitcoin_core_header_cache_operation(&waiting_client, Ok(())).await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "a second Core-cache refresh must wait for the current observation"
        );

        finish_bitcoin_core_header_cache_operation(&client, Ok(())).await?;
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("waiting Core-cache refresh did not resume")??;
        Ok(())
    })
}

#[tokio::test]
async fn core_header_cache_refresh_waits_for_an_in_flight_classification() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let classification = client.transaction().await?;
        lock_bitcoin_core_header_cache_shared_in_transaction(&classification).await?;
        let waiting_client = connect_to_schema(&schema).await?;
        let mut waiter = tokio::spawn(async move {
            lock_bitcoin_core_header_cache(&waiting_client).await?;
            finish_bitcoin_core_header_cache_operation(&waiting_client, Ok(())).await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "a cache refresh must wait until a classification transaction commits"
        );

        classification.commit().await?;
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("waiting Core-cache refresh did not resume")??;
        Ok(())
    })
}

#[tokio::test]
async fn cache_blocked_reconcile_does_not_block_canonical_exclusive() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let (resolver, pool_ids_by_slug, source_id, parsed) = namecoin_fixture(&client).await?;
        let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
            .expect("Namecoin fixture carries a BIP34 parent height");
        let parent_header = parsed.parent_header.header;
        crate::support::db::seed_bitcoin_core_header_cache_through(
            &client,
            parent_height,
            i64::from(parent_header.time),
            parent_header.bits.to_consensus(),
        )
        .await?;
        let payload = namecoin_event_payload(
            &parsed,
            &resolver,
            &pool_ids_by_slug,
            500_000,
            ClassificationProof::default(),
            1_000,
        )?;
        let event_id = upsert_merge_mining_event(&client, source_id, &payload)
            .await?
            .event_id;
        let cache_holder_pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        lock_bitcoin_core_header_cache(&client).await?;

        let mut reconcile_client = connect_to_schema(&schema).await?;
        let reconcile_pid: i32 = reconcile_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let fake = FakeParentClassifier::new(orphan_candidate_verdict(&parent_header));
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        let mut reconcile = tokio::spawn(async move {
            reconcile_from_merge_mining_event(&mut reconcile_client, event_id, &classifier, None)
                .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let blocked_by_cache_holder: bool = client
                    .query_one(
                        "SELECT $1 = ANY(pg_catalog.pg_blocking_pids($2))",
                        &[&cache_holder_pid, &reconcile_pid],
                    )
                    .await?
                    .get(0);
                if blocked_by_cache_holder {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("enabled reconcile did not wait for the cache lock")?;

        let mut canonical_client = connect_to_schema(&schema).await?;
        let mut canonical = tokio::spawn(async move {
            run_exclusive_core_canonical_view_transaction(
                &mut canonical_client,
                "cache-before-canonical lock-order probe",
                async |_txn| Ok::<_, anyhow::Error>(()),
            )
            .await
        });
        let canonical_while_cache_locked =
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut canonical).await;

        finish_bitcoin_core_header_cache_operation(&client, Ok(())).await?;
        tokio::time::timeout(std::time::Duration::from_secs(5), &mut reconcile)
            .await
            .expect("reconcile did not resume after the cache lock was released")??;
        match canonical_while_cache_locked {
            Ok(result) => result??,
            Err(_) => {
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut canonical)
                    .await
                    .expect("canonical-exclusive probe did not eventually finish")??;
                anyhow::bail!(
                    "cache-blocked reconcile acquired canonical-shared before cache-shared"
                );
            }
        }
        assert_eq!(fake.call_count().await, 1);
        Ok(())
    })
}

#[tokio::test]
async fn cache_blocked_suffix_does_not_block_canonical_exclusive() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let fixture = seed_replaceable_core_suffix(&client).await?;

        let cache_holder_pid: i32 = client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        lock_bitcoin_core_header_cache(&client).await?;
        let mut suffix_client = connect_to_schema(&schema).await?;
        let suffix_pid: i32 = suffix_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let mut suffix = tokio::spawn(async move {
            replace_core_canonical_suffix(
                &mut suffix_client,
                fixture.source_id,
                1,
                0,
                &fixture.expected,
                &fixture.replacements,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let blocked_by_cache_holder: bool = client
                    .query_one(
                        "SELECT $1 = ANY(pg_catalog.pg_blocking_pids($2))",
                        &[&cache_holder_pid, &suffix_pid],
                    )
                    .await?
                    .get(0);
                if blocked_by_cache_holder {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("suffix replacement did not wait for the cache lock")?;

        let mut canonical_client = connect_to_schema(&schema).await?;
        let mut canonical = tokio::spawn(async move {
            run_exclusive_core_canonical_view_transaction(
                &mut canonical_client,
                "suffix cache-before-canonical lock-order probe",
                async |_txn| Ok::<_, anyhow::Error>(()),
            )
            .await
        });
        let canonical_while_cache_locked =
            tokio::time::timeout(std::time::Duration::from_secs(1), &mut canonical).await;

        finish_bitcoin_core_header_cache_operation(&client, Ok(())).await?;
        let suffix_summary = tokio::time::timeout(std::time::Duration::from_secs(5), &mut suffix)
            .await
            .expect("suffix replacement did not resume after cache release")??;
        match canonical_while_cache_locked {
            Ok(result) => result??,
            Err(_) => {
                tokio::time::timeout(std::time::Duration::from_secs(5), &mut canonical)
                    .await
                    .expect("canonical-exclusive suffix probe did not eventually finish")??;
                anyhow::bail!(
                    "cache-blocked suffix acquired canonical-exclusive before cache-shared"
                );
            }
        }
        assert_eq!(suffix_summary.replaced_from_height, 1);
        assert_eq!(suffix_summary.replaced_through_height, 1);
        let canonical_hash: Vec<u8> = client
            .query_one(
                "SELECT btc_header_hash FROM block \
                 WHERE kind = 'canonical' AND btc_height = 1",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(
            canonical_hash,
            fixture.replacement_header.block_hash().to_byte_array()
        );
        Ok(())
    })
}

#[tokio::test]
async fn revocation_waits_for_cache_before_taking_a_parent_lock() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let (resolver, pool_ids_by_slug, source_id, parsed) = namecoin_fixture(&client).await?;
        let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
            .expect("Namecoin fixture carries a BIP34 parent height");
        let parent_header = parsed.parent_header.header;
        crate::support::db::seed_bitcoin_core_header_cache_through(
            &client,
            parent_height,
            i64::from(parent_header.time),
            parent_header.bits.to_consensus(),
        )
        .await?;
        let payload = namecoin_event_payload(
            &parsed,
            &resolver,
            &pool_ids_by_slug,
            500_000,
            ClassificationProof::default(),
            1_000,
        )?;
        let event_id = upsert_merge_mining_event(&client, source_id, &payload)
            .await?
            .event_id;
        let parent_hash = parsed.parent_header.hash().to_byte_array().to_vec();
        let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&parent_header),
        ));
        // This test uses the low-level fixture writer, so materialize the
        // parent before exercising its real revocation path.
        reconcile_from_merge_mining_event(&mut client, event_id, &classifier, None).await?;

        let refresh = connect_to_schema(&schema).await?;
        lock_bitcoin_core_header_cache(&refresh).await?;
        let mut revoker = connect_to_schema(&schema).await?;
        let revoker_pid: i32 = revoker
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let revoker_classifier = classifier.clone();
        let mut revocation = tokio::spawn(async move {
            revoke_merge_mining_event(
                &mut revoker,
                event_id,
                "cache_lock_order",
                &revoker_classifier,
            )
            .await
        });

        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let waiting: i64 = client
                    .query_one(
                        "SELECT COUNT(*)::int8 FROM pg_locks \
                         WHERE locktype = 'advisory' \
                           AND pid = $1 \
                           AND mode = 'ShareLock' \
                           AND NOT granted",
                        &[&revoker_pid],
                    )
                    .await?
                    .get(0);
                if waiting > 0 {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await??;

        let mut parent_locker = connect_to_schema(&schema).await?;
        let mut probe = tokio::spawn(async move {
            let transaction = parent_locker.transaction().await?;
            lock_block_hash(&transaction, &parent_hash).await?;
            transaction.commit().await?;
            Ok::<(), anyhow::Error>(())
        });
        let probe_result =
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut probe).await;

        finish_bitcoin_core_header_cache_operation(&refresh, Ok(())).await?;
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut revocation)
            .await
            .expect("revocation did not resume after cache refresh")??;
        match probe_result {
            Ok(result) => result??,
            Err(_) => {
                tokio::time::timeout(std::time::Duration::from_secs(1), &mut probe)
                    .await
                    .expect("parent-lock probe did not resume")??;
                anyhow::bail!(
                    "revocation held a parent lock while waiting for the Core-cache reader lock"
                );
            }
        }
        Ok(())
    })
}

#[tokio::test]
async fn refresh_replaces_a_shallow_epoch_boundary_from_core() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let first = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &first).await?;

        let replacement = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(2016, 9, 4, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &replacement).await?;

        let block_hash: Vec<u8> = client
            .query_one(
                "SELECT block_hash FROM bitcoin_core_header WHERE height = 2016",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(block_hash, vec![9; 32]);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_retries_a_same_height_core_reorg_before_writing() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header_sequence([
                core_header(2030, 2, 3, 0x1c00_ffff),
                core_header(2030, 9, 4, 0x1c00_ffff),
            ]),
        );

        let table = refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        assert_eq!(table.horizon_height(), 2030);
        let horizon_hash: Vec<u8> = client
            .query_one(
                "SELECT block_hash FROM bitcoin_core_header WHERE height = 2030",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(horizon_hash, vec![9; 32]);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_detects_an_advancing_tip_reorg_at_the_prior_horizon() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let first = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 100, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &first).await?;
        assert_eq!(
            load_bitcoin_core_nbits_table(&client).await?.horizon_time(),
            100
        );

        // A new tip can conceal a reorg at the old horizon because that height
        // is no longer part of the next sparse replacement. The refresh must
        // verify it directly before preserving the prior timestamp high-water.
        let reorged = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2031)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 9, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2031, 8, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &reorged).await?;
        assert_eq!(
            load_bitcoin_core_nbits_table(&client).await?.horizon_time(),
            3,
            "a reorg replaces, rather than preserves, the stale timestamp horizon"
        );
        Ok(())
    })
}

#[tokio::test]
async fn refresh_rejects_a_horizon_below_an_existing_final_epoch() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let finalized = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2116)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2116, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &finalized).await?;

        let lagging = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2015)
            .with_canonical_header(core_header(2015, 3, 4, 0x1d00_ffff)),
        );
        let error = refresh_bitcoin_core_header_cache(&mut client, &lagging)
            .await
            .expect_err("a lagging Core node must not move the cache before a finalized epoch");
        assert!(
            error.to_string().contains("highest finalized epoch"),
            "unexpected error: {error:#}"
        );
        Ok(())
    })
}

#[tokio::test]
async fn shallow_cache_reorg_reclassifies_existing_orphans_and_source_health() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (resolver, pool_ids_by_slug, source_id, parsed) = namecoin_fixture(&client).await?;
        let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
            .expect("Namecoin fixture carries a BIP34 parent height");
        let parent_header = parsed.parent_header.header;
        crate::support::db::seed_bitcoin_core_header_cache_through(
            &client,
            parent_height,
            i64::from(parent_header.time),
            parent_header.bits.to_consensus(),
        )
        .await?;
        let epoch = mmm_capture::nbits_table::daa_epoch_start(parent_height);
        client
            .execute(
                "UPDATE bitcoin_core_header SET is_final = FALSE WHERE height = $1",
                &[&epoch],
            )
            .await?;
        let payload = namecoin_event_payload(
            &parsed,
            &resolver,
            &pool_ids_by_slug,
            500_000,
            ClassificationProof::default(),
            1_000,
        )?;
        let event_id = upsert_merge_mining_event(&client, source_id, &payload)
            .await?
            .event_id;
        // The fixture writer intentionally bypasses incremental maintenance;
        // establish its derived baseline before testing a cache-driven update.
        rebuild_source_health(&mut client).await?;
        let absent = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&parent_header),
        ));
        reconcile_from_merge_mining_event(&mut client, event_id, &absent, None).await?;
        let parent_hash = parsed.parent_header.hash().to_byte_array().to_vec();
        let before: Option<String> = client
            .query_one(
                "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await?
            .get(0);
        assert_eq!(before.as_deref(), Some("strict_btc_orphan"));

        let core_tip = parent_height + 100;
        let reorged = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(orphan_candidate_verdict(&parent_header))
                .with_synced_tip_height(core_tip)
                .with_canonical_header(core_header(
                    epoch,
                    9,
                    i64::from(epoch) + 1,
                    parent_header.bits.to_consensus() ^ 1,
                ))
                .with_canonical_header(core_header(
                    parent_height,
                    7,
                    i64::from(parent_header.time) + 1,
                    parent_header.bits.to_consensus(),
                ))
                .with_canonical_header(core_header(
                    core_tip,
                    8,
                    i64::from(parent_header.time) + 1,
                    parent_header.bits.to_consensus(),
                )),
        );
        refresh_bitcoin_core_header_cache(&mut client, &reorged).await?;

        let after: Option<String> = client
            .query_one(
                "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await?
            .get(0);
        assert_eq!(after.as_deref(), Some("excluded"));
        let source_health = client
            .query_one(
                "SELECT strict_orphan_parents, unknown_parents \
                 FROM source_health WHERE source_id = $1",
                &[&source_id],
            )
            .await?;
        assert_eq!(source_health.get::<_, i64>(0), 0);
        assert_eq!(source_health.get::<_, i64>(1), 1);
        Ok(())
    })
}

#[tokio::test]
async fn strict_cache_reclassification_error_at_barrier_keeps_retry_markers_and_verdict()
-> Result<()> {
    crate::run_mut_db_test!(client, {
        let (resolver, pool_ids_by_slug, source_id, parsed) = namecoin_fixture(&client).await?;
        let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
            .expect("Namecoin fixture carries a BIP34 parent height");
        let parent_header = parsed.parent_header.header;
        crate::support::db::seed_bitcoin_core_header_cache_through(
            &client,
            parent_height,
            i64::from(parent_header.time),
            parent_header.bits.to_consensus(),
        )
        .await?;
        let epoch = mmm_capture::nbits_table::daa_epoch_start(parent_height);
        client
            .execute(
                "UPDATE bitcoin_core_header SET is_final = FALSE WHERE height = $1",
                &[&epoch],
            )
            .await?;
        let payload = namecoin_event_payload(
            &parsed,
            &resolver,
            &pool_ids_by_slug,
            500_000,
            ClassificationProof::default(),
            1_000,
        )?;
        let event_id = upsert_merge_mining_event(&client, source_id, &payload)
            .await?
            .event_id;
        let baseline = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&parent_header),
        ));
        reconcile_from_merge_mining_event(&mut client, event_id, &baseline, None).await?;
        let parent_hash = parsed.parent_header.hash().to_byte_array().to_vec();
        let before: (String, Option<String>) = client
            .query_one(
                "SELECT kind, btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await
            .map(|row| (row.get(0), row.get(1)))?;
        assert_eq!(
            before,
            ("unknown".to_owned(), Some("strict_btc_orphan".to_owned()))
        );

        let core_tip = parent_height + 100;
        let fake = FakeParentClassifier::new(orphan_candidate_verdict(&parent_header))
            .with_classification_error_on_call(2)
            .with_synced_tip_height(core_tip)
            .with_canonical_header(core_header(
                epoch,
                9,
                i64::from(epoch) + 1,
                parent_header.bits.to_consensus() ^ 1,
            ))
            .with_canonical_header(core_header(
                parent_height,
                7,
                i64::from(parent_header.time) + 1,
                parent_header.bits.to_consensus(),
            ))
            .with_canonical_header(core_header(
                core_tip,
                8,
                i64::from(parent_header.time) + 1,
                parent_header.bits.to_consensus(),
            ));
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        let error = refresh_bitcoin_core_header_cache(&mut client, &classifier)
            .await
            .expect_err("barrier-time strict classification failure must abort refresh");
        let error_detail = format!("{error:#}");
        assert!(
            error_detail.contains("injected classification error on call 2"),
            "unexpected error: {error_detail}"
        );
        assert_eq!(fake.call_count().await, 2);

        let retry_state = client
            .query_one(
                "SELECT reclassification_needed, orphan_recheck_needed \
                 FROM bitcoin_core_header_cache_state WHERE singleton",
                &[],
            )
            .await?;
        assert!(retry_state.get::<_, bool>(0));
        assert!(retry_state.get::<_, bool>(1));
        let after: (String, Option<String>) = client
            .query_one(
                "SELECT kind, btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await
            .map(|row| (row.get(0), row.get(1)))?;
        assert_eq!(after, before);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_finalizes_a_shallow_boundary_after_a_missed_verification_window() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let first = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &first).await?;

        let reorged = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(4032)
            .with_canonical_header(core_header(2016, 9, 4, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 8, 4, 0x1c00_ffff))
            .with_canonical_header(core_header(4032, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &reorged).await?;
        let row = client
            .query_one(
                "SELECT block_hash, is_final FROM bitcoin_core_header WHERE height = 2016",
                &[],
            )
            .await?;
        assert_eq!(row.get::<_, Vec<u8>>(0), vec![9; 32]);
        assert!(row.get::<_, bool>(1));
        Ok(())
    })
}
