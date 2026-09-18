use std::sync::Arc;

use anyhow::Result;
use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};
use mmm_bitcoin_core::{
    BitcoinCoreBlockCoinbase, ConfiguredParentClassifier, CoreHeader, FakeParentClassifier,
    FakeParentClassifierGate, ParentClassification,
};
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::capture::ClassificationProof;
use mmm_capture::nbits_table::NbitsLookup;
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use mmm_producers::refresh_bitcoin_core_header_cache;
use mmm_read_model::{
    CORE_RECOVERY_BATCH, CoreCanonicalReplacement, ExpectedCoreCanonicalRow, lock_block_hash,
    rebuild_source_health, reconcile_from_merge_mining_event, replace_core_canonical_suffix,
    revoke_merge_mining_event, run_exclusive_core_canonical_view_transaction,
    run_scheduled_recheck,
};
use mmm_store::{
    BitcoinCoreHeader, BitcoinCoreHeaderCacheUpdate, RecheckScope, acknowledge_recheck_pass,
    bind_recheck_pass, finish_bitcoin_core_header_cache_operation,
    finish_bitcoin_core_header_cache_shared_operation, get_source_id,
    is_bitcoin_core_header_cache_integrity_error, load_bitcoin_core_nbits_table,
    load_bitcoin_core_nbits_table_if_present, load_scheduled_recheck,
    lock_bitcoin_core_header_cache, lock_bitcoin_core_header_cache_shared,
    lock_bitcoin_core_header_cache_shared_in_transaction, record_bitcoin_core_header,
    replace_bitcoin_core_header_cache, schedule_core_recheck, upsert_merge_mining_event,
};

use tokio_postgres::Client;

use crate::support::db::connect_to_schema;
use crate::support::scenario::orphan_candidate_verdict;
use crate::support::seed::{insert_block, test_header_chain};
use crate::support::{NamecoinEventFixture, namecoin_event_payload, namecoin_fixture};

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
async fn migration_0019_schedules_a_generation_the_job_acknowledges() -> Result<()> {
    let (mut client, schema) =
        crate::support::db::new_test_db_through("0018_add_rod_source").await?;
    let result = async {
        client
            .execute(
                "UPDATE bitcoin_core_header_cache_state \
                 SET reclassification_needed = FALSE, orphan_recheck_needed = FALSE \
                 WHERE singleton",
                &[],
            )
            .await?;
        client
            .batch_execute(include_str!(
                "../../../../migrations/0019_recheck_orphans_after_hathor_bip34.sql"
            ))
            .await?;
        // The migrations between 0019 and the current store: 0026 adds the
        // cache generation, 0027 converts the booleans 0019 set into pending
        // generation 1 with the orphan scope, 0028 the queue's expansion flag;
        // the sparse-cache migrations between them do not touch this table.
        client
            .batch_execute(include_str!(
                "../../../../migrations/0026_add_child_chain_head.sql"
            ))
            .await?;
        client
            .batch_execute(include_str!(
                "../../../../migrations/0027_schedule_core_rechecks_by_generation.sql"
            ))
            .await?;
        client
            .batch_execute(include_str!(
                "../../../../migrations/0028_add_reconcile_queue_expand_unchanged.sql"
            ))
            .await?;
        let state = load_scheduled_recheck(&client).await?;
        assert_eq!(state.pending_generation, 1);
        assert!(state.pending_scope.orphans);
        assert_eq!(state.pending_scope.sources, None);
        assert!(state.is_pending());

        // A refresh no longer consumes the work: it reports it pending and
        // leaves the generation for the job.
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        let after_refresh = load_scheduled_recheck(&client).await?;
        assert!(after_refresh.is_pending());
        assert!(after_refresh.pending_scope.orphans);

        // The job, with nothing to reclassify here, acknowledges the generation
        // the refresh left (the refresh scheduled a generation of its own for
        // the empty cache it populated).
        let report = run_scheduled_recheck(&mut client, &classifier, 100).await?;
        assert_eq!(report.acknowledged, Some(after_refresh.pending_generation));
        let done = load_scheduled_recheck(&client).await?;
        assert!(!done.is_pending());
        assert_eq!(done.pass, None);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    crate::support::db::teardown_test_db(&client, &schema, result).await
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
                "UPDATE bitcoin_core_header_cache_state SET horizon_time = 0",
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
        assert!(first.scheduled);
        assert!(
            first.pending_orphans,
            "initial Core-cache population revisits classifications made before the cache existed"
        );
        // A pass bound to the pending generation, left in flight.
        let populated = load_scheduled_recheck(&client).await?;
        let pass = bind_recheck_pass(&client, populated.pending_scope).await?;

        let advanced_with_an_older_timestamp = advance_to_101(&mut client).await?;
        assert!(advanced_with_an_older_timestamp.scheduled);
        assert!(
            !advanced_with_an_older_timestamp.pending_orphans,
            "ordinary horizon advances do not revisit already classified orphans"
        );
        let advanced = load_scheduled_recheck(&client).await?;
        assert_eq!(
            advanced.pass.as_ref().map(|pass| pass.generation),
            Some(pass.generation),
            "an ordinary horizon advance is additive: it leaves a pass in flight bound"
        );
        acknowledge_recheck_pass(&client, pass.generation).await?;
        assert_eq!(
            load_bitcoin_core_nbits_table(&client).await?.horizon_time(),
            100
        );

        let retry = advance_to_101(&mut client).await?;
        assert!(
            !retry.scheduled,
            "an unchanged horizon schedules nothing new"
        );
        assert!(
            retry.pending,
            "but the unacknowledged generation stays pending"
        );
        let pending = load_scheduled_recheck(&client).await?;
        let pass = bind_recheck_pass(&client, pending.pending_scope).await?;
        acknowledge_recheck_pass(&client, pass.generation).await?;

        let settled = advance_to_101(&mut client).await?;
        assert!(!settled.scheduled);
        assert!(!settled.pending);

        let generation_before = core_cache_generation(&client).await?;
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
            boundary_overlaps_existing_coverage.pending_orphans,
            "a new retarget boundary inside prior timestamp coverage can change existing verdicts"
        );
        // Such a replacement also moves the cache generation, so child-chain
        // heads recorded before it stop being final; the plain horizon
        // advances above left it alone.
        assert_eq!(
            generation_before, 0,
            "populating an empty cache and plain horizon advances change no given verdict"
        );
        assert_eq!(core_cache_generation(&client).await?, 1);
        Ok(())
    })
}

/// A refresh whose horizon is height 101 with a timestamp older than the
/// coverage already recorded: an ordinary advance the first time, unchanged
/// after.
async fn advance_to_101(client: &mut Client) -> Result<BitcoinCoreHeaderCacheUpdate> {
    replace_bitcoin_core_header_cache(
        client,
        0,
        &[],
        None,
        &header(101, 2, 99, 0x1d00_ffff),
        false,
    )
    .await
}

/// The cache generation child-chain heads are pinned to.
async fn core_cache_generation(client: &Client) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT core_cache_generation FROM bitcoin_core_header_cache_state WHERE singleton",
            &[],
        )
        .await?
        .get(0))
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

/// The Namecoin fixture's unknown parent captured as an event, with the Core
/// cache seeded through its height: the starting point for the tests that
/// drive a cache change past an existing classification.
struct SeededParent {
    source_id: i64,
    event_id: i64,
    parent_height: i32,
    parent_header: Header,
    parent_hash: Vec<u8>,
}

async fn seed_unknown_parent(client: &Client) -> Result<SeededParent> {
    let fixture = NamecoinEventFixture::new(client).await?;
    let parent_height = parse_bip34_height(&fixture.parsed.parent_coinbase_script)
        .expect("Namecoin fixture carries a BIP34 parent height");
    let parent_header = fixture.parsed.parent_header.header;
    crate::support::db::seed_bitcoin_core_header_cache_through(
        client,
        parent_height,
        i64::from(parent_header.time),
        parent_header.bits.to_consensus(),
    )
    .await?;
    let inserted = fixture
        .insert_event(client, 500_000, ClassificationProof::default(), 1_000)
        .await?;
    Ok(SeededParent {
        source_id: fixture.source_id,
        event_id: inserted.id,
        parent_height,
        parent_header,
        parent_hash: inserted.parent_hash,
    })
}

/// A fake Core view 100 blocks past the seeded parent whose prior horizon
/// header differs from the cached one: a shallow reorg at the parent's
/// height, the way the shallow-reorg tests exercise it.
fn reorged_core_view(seeded: &SeededParent) -> FakeParentClassifier {
    let parent_header = seeded.parent_header;
    let core_tip = seeded.parent_height + 100;
    FakeParentClassifier::new(orphan_candidate_verdict(&parent_header))
        .with_synced_tip_height(core_tip)
        .with_canonical_header(core_header(
            mmm_capture::nbits_table::daa_epoch_start(seeded.parent_height),
            9,
            i64::from(parent_header.time) - 1,
            parent_header.bits.to_consensus(),
        ))
        .with_canonical_header(core_header(
            seeded.parent_height,
            7,
            i64::from(parent_header.time) + 1,
            parent_header.bits.to_consensus(),
        ))
        .with_canonical_header(core_header(
            core_tip,
            8,
            i64::from(parent_header.time) + 1,
            parent_header.bits.to_consensus(),
        ))
}

#[tokio::test]
async fn refresh_releases_the_lock_between_recovery_batches_and_replaces_on_an_empty_queue()
-> Result<()> {
    crate::run_db_test!(client, schema, {
        let seeded = seed_unknown_parent(&client).await?;
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        seed_recovery_queue(&client, bitcoin, &seeded.parent_hash).await?;

        let gate = FakeParentClassifierGate::new();
        let fake = reorged_core_view(&seeded).with_first_call_gate(Arc::clone(&gate));
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        let order = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));

        // The refresh takes the lock and stops at the gate inside its first
        // recovery batch, whose first primary is the fixture's parent.
        let refresh_client = connect_to_schema(&schema).await?;
        let refresh_order = Arc::clone(&order);
        let refresh = tokio::spawn(async move {
            let mut refresh_client = refresh_client;
            let table = refresh_bitcoin_core_header_cache(&mut refresh_client, &classifier).await;
            refresh_order.lock().unwrap().push("refresh finished");
            table
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.wait_started())
            .await
            .expect("the first recovery batch did not reach the gated classification");

        // A suffix replacement contends for the shared lock while the refresh
        // holds it through its first batch. Once the refresh releases the lock
        // with work still queued, the replacement commits one more cascade
        // primary, as a real replacement enqueues its dependents.
        let suffix_client = connect_to_schema(&schema).await?;
        let suffix_order = Arc::clone(&order);
        let mut suffix = tokio::spawn(async move {
            lock_bitcoin_core_header_cache_shared(&suffix_client).await?;
            let late = [0xee_u8; 32];
            let result = suffix_client
                .execute(
                    "INSERT INTO bitcoin_core_reconcile_queue (source_id, btc_parent_header_hash) \
                     VALUES ($1, $2)",
                    &[&bitcoin, &late.as_slice()],
                )
                .await
                .map(|_| suffix_order.lock().unwrap().push("late primary committed"))
                .map_err(anyhow::Error::from);
            finish_bitcoin_core_header_cache_shared_operation(&suffix_client, result).await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), &mut suffix)
                .await
                .is_err(),
            "the suffix replacement must wait while the refresh holds the lock"
        );
        gate.proceed();
        tokio::time::timeout(std::time::Duration::from_secs(10), &mut suffix)
            .await
            .expect("the refresh must release the lock between recovery batches")??;
        let table = tokio::time::timeout(std::time::Duration::from_secs(10), refresh)
            .await
            .expect("the refresh must finish once the queue drains")??;
        assert_eq!(table.horizon_height(), seeded.parent_height + 100);

        // The late primary was committed before the refresh finished, and the
        // refresh drained it before replacing the cache: the queue is empty.
        assert_eq!(
            order.lock().unwrap().as_slice(),
            ["late primary committed", "refresh finished"]
        );
        let queue_count: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM bitcoin_core_reconcile_queue WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?
            .get(0);
        assert_eq!(queue_count, 0);
        Ok(())
    })
}

/// A committed cascade of one more primary than a recovery batch holds, so a
/// refresh must release the lock once before the queue can be empty. Explicit
/// enqueue times put `first_hash` first, the way the drain orders its work.
async fn seed_recovery_queue(client: &Client, bitcoin: i64, first_hash: &[u8]) -> Result<()> {
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
            "INSERT INTO bitcoin_core_reconcile_queue \
                 (source_id, btc_parent_header_hash, enqueued_at) \
             VALUES ($1, $2, 1)",
            &[&bitcoin, &first_hash.to_vec()],
        )
        .await?;
    for index in 1..=CORE_RECOVERY_BATCH {
        let filler = [u8::try_from(index).expect("small index"); 32];
        client
            .execute(
                "INSERT INTO bitcoin_core_reconcile_queue \
                     (source_id, btc_parent_header_hash, enqueued_at) \
                 VALUES ($1, $2, 2)",
                &[&bitcoin, &filler.as_slice()],
            )
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn refresh_with_a_pending_generation_completes_without_scanning() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let seeded = seed_unknown_parent(&client).await?;
        let generation = schedule_core_recheck(&client, &RecheckScope::everything(), true).await?;

        // A producer's startup refresh with a full recheck pending: the fake
        // classifies nothing, because the refresh leaves the candidates to
        // the job, and the generation is still pending afterwards.
        let fake = reorged_core_view(&seeded);
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        assert_eq!(fake.call_count().await, 0);
        let state = load_scheduled_recheck(&client).await?;
        assert!(state.is_pending());
        assert!(state.pending_generation >= generation);
        assert_eq!(state.pass, None);
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
async fn shallow_cache_reorg_schedules_a_recheck_the_job_applies() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let seeded = seed_unknown_parent(&client).await?;
        let SeededParent {
            source_id,
            event_id,
            parent_height,
            parent_header,
            parent_hash,
        } = seeded;
        let epoch = mmm_capture::nbits_table::daa_epoch_start(parent_height);
        client
            .execute(
                "UPDATE bitcoin_core_header SET is_final = FALSE WHERE height = $1",
                &[&epoch],
            )
            .await?;
        // The fixture writer intentionally bypasses incremental maintenance;
        // establish its derived baseline before testing a cache-driven update.
        rebuild_source_health(&mut client).await?;
        let absent = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&parent_header),
        ));
        reconcile_from_merge_mining_event(&mut client, event_id, &absent, None).await?;
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

        // The refresh only schedules: the verdict is untouched and a recheck
        // covering already classified orphans is pending.
        let after_refresh: Option<String> = client
            .query_one(
                "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await?
            .get(0);
        assert_eq!(after_refresh.as_deref(), Some("strict_btc_orphan"));
        let scheduled = load_scheduled_recheck(&client).await?;
        assert!(scheduled.is_pending());
        assert!(scheduled.pending_scope.orphans);

        // The job applies the new verdict and maintains source health.
        let report = run_scheduled_recheck(&mut client, &reorged, 100).await?;
        assert_eq!(report.changed, 1);
        assert_eq!(report.acknowledged, Some(scheduled.pending_generation));
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
async fn a_strict_classification_error_keeps_the_pass_and_the_verdict() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let seeded = seed_unknown_parent(&client).await?;
        let baseline = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&seeded.parent_header),
        ));
        reconcile_from_merge_mining_event(&mut client, seeded.event_id, &baseline, None).await?;
        let parent_hash = seeded.parent_hash.clone();
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

        let fake = reorged_core_view(&seeded).with_classification_error_on_call(1);
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());
        // The refresh classifies nothing itself, so the injected error on the
        // first classification is never reached by it.
        refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        assert_eq!(fake.call_count().await, 0);
        let scheduled = load_scheduled_recheck(&client).await?;
        assert!(scheduled.is_pending());

        // The job's strict classification hits it: the run fails, the pass
        // stays bound with its cursor past the queued page, the generation
        // stays pending, and the verdict is untouched, so a later run repeats
        // the candidate from the durable queue.
        let error = run_scheduled_recheck(&mut client, &classifier, 100)
            .await
            .expect_err("a strict classification failure must fail the scheduled run");
        let error_detail = format!("{error:#}");
        assert!(
            error_detail.contains("injected classification error on call 1"),
            "unexpected error: {error_detail}"
        );
        assert_eq!(fake.call_count().await, 1);
        let retry_state = load_scheduled_recheck(&client).await?;
        assert!(retry_state.is_pending());
        assert_eq!(
            retry_state.pass.as_ref().map(|pass| pass.generation),
            Some(scheduled.pending_generation)
        );
        assert!(
            retry_state
                .pass
                .as_ref()
                .is_some_and(|pass| pass.cursor.is_some())
        );
        let queued: i64 = client
            .query_one(
                "SELECT count(*) FROM bitcoin_core_reconcile_queue WHERE primary_pending",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(queued, 1);
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
