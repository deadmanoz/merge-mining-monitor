use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use bitcoin::BlockHash;
use bitcoin::CompactTarget;
use bitcoin::block::{Header, Version};
use bitcoin::consensus::serialize;
use bitcoin::hash_types::TxMerkleNode;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    BitcoinCoreBlockCoinbase, ConfiguredParentClassifier, CoreHeader, FakeParentClassifier,
    FakeParentClassifierGate,
};
use mmm_capture::capture::ClassificationProof;
use mmm_capture::source_registry::{BITCOIN_SOURCE_CODE, NAMECOIN_SOURCE_CODE};
use mmm_producers::{
    BitcoinCoreBackboneSource, BitcoinCoreBackboneTip, BitcoinCoreSyncConfig,
    accept_live_repaired_target_for_test, initialize_follow_state,
    record_retryable_repair_failure_for_test, repair_near_tip_backbone_for_test,
    run_bitcoin_core_follow_tick_for_test, run_sync_bitcoin_core,
};
use mmm_read_model::{
    CoreCanonicalReplacement, ExpectedCoreCanonicalRow, compute_source_health_from_base,
    drain_core_reconcile_queue, drain_core_reconcile_queue_with_budget_for_test,
    rebuild_source_health, reconcile_from_merge_mining_event, replace_core_canonical_suffix,
};
use mmm_store::get_source_id;
use serde_json::json;
use time::Month;
use tokio::sync::{Notify, oneshot};
use tokio_postgres::Client;
use tokio_postgres::types::Json;

use crate::support::scenario::canonical_verdict;
use crate::support::seed::{
    EventSeed, day_epoch, hash_bytes, header_hash_and_prev, header_hash_bytes, insert_block,
    insert_event, test_header_chain,
};
use crate::support::{NamecoinEventFixture, capture_test_payload};

use crate::helpers::{FakeBitcoinCoreBackboneSource, project_tree};

fn fork_header_chain(
    original: &BTreeMap<i32, Header>,
    fork_height: i32,
    tip_height: i32,
    base_time: u32,
) -> BTreeMap<i32, Header> {
    let mut headers = original
        .range(..fork_height)
        .map(|(height, header)| (*height, *header))
        .collect::<BTreeMap<_, _>>();
    let mut prev = if fork_height == 0 {
        BlockHash::all_zeros()
    } else {
        headers[&(fork_height - 1)].block_hash()
    };
    for height in fork_height..=tip_height {
        let header = Header {
            version: Version::ONE,
            prev_blockhash: prev,
            merkle_root: TxMerkleNode::all_zeros(),
            time: base_time + height as u32,
            bits: CompactTarget::from_consensus(0x207f_ffff),
            nonce: 10_000 + height as u32,
        };
        prev = header.block_hash();
        headers.insert(height, header);
    }
    headers
}

fn core_cache_classifier(
    headers: &BTreeMap<i32, Header>,
    tip_height: i32,
) -> ConfiguredParentClassifier {
    let as_core_header = |height: i32| {
        let header = headers
            .get(&height)
            .unwrap_or_else(|| panic!("missing fake Core header at {height}"));
        CoreHeader {
            height,
            hash: header.block_hash(),
            header_time: i64::from(header.time),
            nbits: header.bits.to_consensus(),
        }
    };
    let mut classifier = FakeParentClassifier::new(canonical_verdict(
        headers
            .get(&tip_height)
            .unwrap_or_else(|| panic!("missing fake Core tip at {tip_height}")),
        tip_height,
    ))
    .with_synced_tip_height(tip_height)
    .with_canonical_header(as_core_header(0));
    if tip_height != 0 {
        classifier = classifier.with_canonical_header(as_core_header(tip_height));
    }
    ConfiguredParentClassifier::Fake(classifier)
}

#[derive(Clone)]
struct MovingTargetBackboneSource {
    captured: Arc<BTreeMap<i32, Header>>,
    current: Arc<BTreeMap<i32, Header>>,
    tip_height: i32,
    moved: Arc<AtomicBool>,
}

impl MovingTargetBackboneSource {
    fn new(
        tip_height: i32,
        captured: BTreeMap<i32, Header>,
        current: BTreeMap<i32, Header>,
    ) -> Self {
        Self {
            captured: Arc::new(captured),
            current: Arc::new(current),
            tip_height,
            moved: Arc::new(AtomicBool::new(false)),
        }
    }

    fn move_target(&self) {
        self.moved.store(true, Ordering::SeqCst);
    }

    fn active(&self) -> &BTreeMap<i32, Header> {
        if self.moved.load(Ordering::SeqCst) {
            &self.current
        } else {
            &self.captured
        }
    }

    fn header_for_hash(&self, hash: BlockHash) -> Result<Header> {
        self.captured
            .values()
            .chain(self.current.values())
            .find(|header| header.block_hash() == hash)
            .copied()
            .with_context(|| format!("moving fake Core source has no header {hash}"))
    }
}

impl BitcoinCoreBackboneSource for MovingTargetBackboneSource {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip> {
        Ok(BitcoinCoreBackboneTip {
            height: self.tip_height,
            hash: self.active()[&self.tip_height].block_hash(),
        })
    }

    async fn block_hash(&self, height: i32) -> Result<BlockHash> {
        self.active()
            .get(&height)
            .map(Header::block_hash)
            .with_context(|| format!("moving fake Core source has no height {height}"))
    }

    async fn block_header(&self, hash: BlockHash) -> Result<Header> {
        self.header_for_hash(hash)
    }

    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        Ok(BitcoinCoreBlockCoinbase {
            txid: hash.to_byte_array().to_vec(),
            script: b"/moving-fake-core/".to_vec(),
            outputs: vec![0],
        })
    }
}

#[derive(Clone)]
struct GatedTipBackboneSource {
    inner: FakeBitcoinCoreBackboneSource,
    tip_started: Arc<Notify>,
    release_tip: Arc<Notify>,
}

impl GatedTipBackboneSource {
    fn new(inner: FakeBitcoinCoreBackboneSource) -> Self {
        Self {
            inner,
            tip_started: Arc::new(Notify::new()),
            release_tip: Arc::new(Notify::new()),
        }
    }

    async fn wait_for_tip(&self) {
        self.tip_started.notified().await;
    }

    fn release_tip(&self) {
        self.release_tip.notify_one();
    }
}

impl BitcoinCoreBackboneSource for GatedTipBackboneSource {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip> {
        self.tip_started.notify_one();
        self.release_tip.notified().await;
        self.inner.tip().await
    }

    async fn block_hash(&self, height: i32) -> Result<BlockHash> {
        self.inner.block_hash(height).await
    }

    async fn block_header(&self, hash: BlockHash) -> Result<Header> {
        self.inner.block_header(hash).await
    }

    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        self.inner.block_coinbase(hash).await
    }
}

#[derive(Clone)]
struct SwitchingBackboneSource {
    initial: Arc<BTreeMap<i32, Header>>,
    current: Arc<BTreeMap<i32, Header>>,
    initial_tip_height: i32,
    current_tip_height: i32,
    switched: Arc<AtomicBool>,
    coinbase_started: Arc<Notify>,
    release_coinbase: Arc<Notify>,
}

impl SwitchingBackboneSource {
    fn new(
        initial_tip_height: i32,
        initial: BTreeMap<i32, Header>,
        current_tip_height: i32,
        current: BTreeMap<i32, Header>,
    ) -> Self {
        Self {
            initial: Arc::new(initial),
            current: Arc::new(current),
            initial_tip_height,
            current_tip_height,
            switched: Arc::new(AtomicBool::new(false)),
            coinbase_started: Arc::new(Notify::new()),
            release_coinbase: Arc::new(Notify::new()),
        }
    }

    async fn wait_for_coinbase(&self) {
        self.coinbase_started.notified().await;
    }

    fn switch_and_release(&self) {
        self.switched.store(true, Ordering::SeqCst);
        self.release_coinbase.notify_one();
    }

    fn active(&self) -> (&BTreeMap<i32, Header>, i32) {
        if self.switched.load(Ordering::SeqCst) {
            (&self.current, self.current_tip_height)
        } else {
            (&self.initial, self.initial_tip_height)
        }
    }
}

impl BitcoinCoreBackboneSource for SwitchingBackboneSource {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip> {
        let (headers, tip_height) = self.active();
        Ok(BitcoinCoreBackboneTip {
            height: tip_height,
            hash: headers[&tip_height].block_hash(),
        })
    }

    async fn block_hash(&self, height: i32) -> Result<BlockHash> {
        let (headers, _) = self.active();
        headers
            .get(&height)
            .map(Header::block_hash)
            .with_context(|| format!("switching fake Core source has no height {height}"))
    }

    async fn block_header(&self, hash: BlockHash) -> Result<Header> {
        self.initial
            .values()
            .chain(self.current.values())
            .find(|header| header.block_hash() == hash)
            .copied()
            .with_context(|| format!("switching fake Core source has no header {hash}"))
    }

    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        self.coinbase_started.notify_one();
        self.release_coinbase.notified().await;
        Ok(BitcoinCoreBlockCoinbase {
            txid: hash.to_byte_array().to_vec(),
            script: b"/switching-fake-core/".to_vec(),
            outputs: vec![0],
        })
    }
}

async fn insert_event_with_real_parent_header(
    client: &Client,
    source_id: i64,
    child_height: i32,
    child_hash: &[u8],
    header: &Header,
    kind: &str,
    btc_height: Option<i32>,
) -> Result<i64> {
    let parent_hash = header.block_hash().to_byte_array().to_vec();
    let prev_hash = header.prev_blockhash.to_byte_array().to_vec();
    let header_bytes = serialize(header);
    let parent_time = header.time as i64;
    Ok(client
        .query_one(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_prev_header_hash, btc_parent_header_bytes, \
                btc_parent_header_time, btc_parent_height, btc_parent_kind, \
                pow_validates_btc_target, pow_validates_child_target, \
                discovered_at, confirmed_at \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, TRUE, TRUE, $8, $8 \
             ) RETURNING id",
            &[
                &source_id,
                &child_height,
                &child_hash,
                &(parent_time + child_height as i64),
                &parent_hash,
                &prev_hash,
                &header_bytes,
                &parent_time,
                &btc_height,
                &kind,
            ],
        )
        .await?
        .get(0))
}

async fn insert_matching_upper_canonical(
    client: &Client,
    header: &Header,
    height: i32,
) -> Result<()> {
    let hash = header.block_hash().to_byte_array().to_vec();
    let prev = header.prev_blockhash.to_byte_array().to_vec();
    insert_block(
        client,
        &hash,
        &prev,
        Some(height),
        "canonical",
        header.time as i64,
        None,
    )
    .await
}

async fn canonical_view(
    client: &Client,
    from_height: i32,
    to_height: i32,
) -> Result<Vec<ExpectedCoreCanonicalRow>> {
    Ok(client
        .query(
            "SELECT btc_height, btc_header_hash, btc_prev_header_hash \
             FROM block WHERE kind = 'canonical' AND btc_height BETWEEN $1 AND $2 \
             ORDER BY btc_height, btc_header_hash",
            &[&from_height, &to_height],
        )
        .await?
        .into_iter()
        .map(|row| ExpectedCoreCanonicalRow {
            height: row.get(0),
            hash: row.get(1),
            prev_hash: row.get(2),
        })
        .collect())
}

async fn assert_completed_repair_state(
    client: &Client,
    target_height: i32,
    target_hash: BlockHash,
) -> Result<()> {
    let sync = client
        .query_one(
            "SELECT target_tip_height, target_tip_hash, contiguous_complete_height, \
                    last_error_code \
             FROM bitcoin_core_sync_state",
            &[],
        )
        .await?;
    assert_eq!(sync.get::<_, Option<i32>>(0), Some(target_height));
    assert_eq!(
        sync.get::<_, Option<Vec<u8>>>(1).as_deref(),
        Some(target_hash.to_byte_array().as_slice())
    );
    assert_eq!(sync.get::<_, i32>(2), target_height);
    assert_eq!(sync.get::<_, Option<String>>(3), None);
    let queued: i64 = client
        .query_one(
            "SELECT count(*)::bigint FROM bitcoin_core_reconcile_queue",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(queued, 0);
    Ok(())
}

async fn assert_source_health_kind_counts(
    client: &Client,
    source_id: i64,
    canonical: i64,
    stale: i64,
) -> Result<()> {
    let computed = compute_source_health_from_base(client).await?;
    let expected = computed
        .rows
        .iter()
        .find(|row| row.source_id == source_id)
        .expect("source present in source-health recompute");
    let maintained = client
        .query_one(
            "SELECT canonical_parents, stale_parents FROM source_health WHERE source_id = $1",
            &[&source_id],
        )
        .await?;
    assert_eq!(maintained.get::<_, i64>(0), expected.canonical_parents);
    assert_eq!(maintained.get::<_, i64>(1), expected.stale_parents);
    assert_eq!(expected.canonical_parents, canonical);
    assert_eq!(expected.stale_parents, stale);
    Ok(())
}

async fn assert_pending_core_queue(client: &Client, source_id: i64, count: i64) -> Result<()> {
    let pending = client
        .query_one(
            "SELECT count(*)::bigint, min(generation), max(generation) \
             FROM bitcoin_core_reconcile_queue WHERE source_id = $1",
            &[&source_id],
        )
        .await?;
    assert_eq!(pending.get::<_, i64>(0), count);
    assert_eq!(pending.get::<_, Option<i64>>(1), Some(1));
    assert_eq!(pending.get::<_, Option<i64>>(2), Some(1));
    let pending_code: Option<String> = client
        .query_one(
            "SELECT last_error_code FROM bitcoin_core_sync_state WHERE source_id = $1",
            &[&source_id],
        )
        .await?
        .get(0);
    assert_eq!(
        pending_code.as_deref(),
        Some("backbone_reorg_reconcile_pending")
    );
    Ok(())
}

async fn assert_drained_core_queue(client: &Client, source_id: i64) -> Result<()> {
    let queue_after: i64 = client
        .query_one(
            "SELECT count(*)::bigint FROM bitcoin_core_reconcile_queue WHERE source_id = $1",
            &[&source_id],
        )
        .await?
        .get(0);
    assert_eq!(queue_after, 0);
    let error_after = client
        .query_one(
            "SELECT last_error_code, last_error_height, last_error, last_error_details \
             FROM bitcoin_core_sync_state WHERE source_id = $1",
            &[&source_id],
        )
        .await?;
    assert_eq!(error_after.get::<_, Option<String>>(0), None);
    assert_eq!(error_after.get::<_, Option<i32>>(1), None);
    assert_eq!(error_after.get::<_, Option<String>>(2), None);
    assert_eq!(
        error_after.get::<_, Json<serde_json::Value>>(3).0,
        json!({})
    );
    Ok(())
}

async fn wait_for_exclusive_core_view_barrier(client: &Client, backend_pid: i32) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: bool = client
                .query_one(
                    "SELECT EXISTS ( \
                         SELECT 1 FROM pg_catalog.pg_locks \
                         WHERE pid = $1 AND locktype = 'advisory' \
                           AND classid = $2::int::oid AND objid = $3::int::oid \
                           AND mode = 'ExclusiveLock' AND NOT granted \
                     )",
                    &[&backend_pid, &0x4243_i32, &1_i32],
                )
                .await
                .context("query pending canonical-view lock")?
                .get(0);
            if waiting {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("suffix repair did not wait on the exclusive Core-view barrier")?
}

async fn wait_for_backend_blocked_by(
    client: &Client,
    waiting_pid: i32,
    blocker_pid: i32,
) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let blocked: bool = client
                .query_one(
                    "SELECT $2 = ANY(pg_catalog.pg_blocking_pids($1))",
                    &[&waiting_pid, &blocker_pid],
                )
                .await
                .context("query blocking backend relationship")?
                .get(0);
            if blocked {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("backend did not reach the expected row-lock wait")?
}

async fn hold_shared_core_view_barrier(
    schema: String,
    locked: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
) -> Result<()> {
    let mut client = crate::support::db::connect_to_schema(&schema).await?;
    let txn = client.transaction().await?;
    txn.execute(
        "SELECT pg_advisory_xact_lock_shared($1, $2)",
        &[&0x4243_i32, &1_i32],
    )
    .await?;
    locked
        .send(())
        .map_err(|()| anyhow::anyhow!("shared barrier test receiver dropped"))?;
    release
        .await
        .context("shared barrier test release sender dropped")?;
    txn.commit().await?;
    Ok(())
}

async fn assert_target_move_left_suffix_unmodified(
    client: &Client,
    original: &BTreeMap<i32, Header>,
    captured: &BTreeMap<i32, Header>,
    current: &BTreeMap<i32, Header>,
) -> Result<()> {
    let old_h3 = header_hash_bytes(&original[&3]);
    let old_kind: String = client
        .query_one(
            "SELECT kind FROM block WHERE btc_header_hash = $1",
            &[&old_h3],
        )
        .await?
        .get(0);
    assert_eq!(old_kind, "canonical");
    for new_hash in [
        header_hash_bytes(&captured[&3]),
        header_hash_bytes(&captured[&4]),
        header_hash_bytes(&current[&3]),
        header_hash_bytes(&current[&4]),
    ] {
        let rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&new_hash],
            )
            .await?
            .get(0);
        assert_eq!(rows, 0);
    }
    let state = client
        .query_one(
            "SELECT last_error_details, \
                    (SELECT count(*)::bigint FROM bitcoin_core_reconcile_queue) \
             FROM bitcoin_core_sync_state",
            &[],
        )
        .await?;
    let details: Json<serde_json::Value> = state.get(0);
    assert_eq!(
        details.0["reason"],
        json!("target_hash_moved_during_capture")
    );
    assert_eq!(state.get::<_, i64>(1), 0);
    Ok(())
}

async fn insert_two_hop_core_dependents(
    client: &Client,
    source_id: i64,
    canonical_parent_hash: Vec<u8>,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let dependent_hash = hash_bytes(0xd300);
    let grandchild_hash = hash_bytes(0xd301);
    insert_event(
        client,
        EventSeed {
            source_id,
            child_height: 88,
            child_hash: hash_bytes(0x8800),
            parent_hash: dependent_hash.clone(),
            prev_hash: canonical_parent_hash,
            parent_time: 1_800_041_100,
            kind: "unknown",
            pow_validates_btc_target: true,
            btc_height: None,
            pool_id: None,
        },
    )
    .await?;
    insert_event(
        client,
        EventSeed {
            source_id,
            child_height: 89,
            child_hash: hash_bytes(0x8900),
            parent_hash: grandchild_hash.clone(),
            prev_hash: dependent_hash.clone(),
            parent_time: 1_800_041_101,
            kind: "unknown",
            pow_validates_btc_target: true,
            btc_height: None,
            pool_id: None,
        },
    )
    .await?;
    Ok((dependent_hash, grandchild_hash))
}

async fn drain_until_partial_core_frontier(
    client: &mut Client,
    source_id: i64,
    dependent_hash: &[u8],
    grandchild_hash: &[u8],
) -> Result<()> {
    let mut reached_partial_frontier = false;
    for _ in 0..8 {
        drain_core_reconcile_queue_with_budget_for_test(
            client,
            source_id,
            &ConfiguredParentClassifier::Disabled,
            1,
        )
        .await
        .expect_err("one-parent budget must stop before the full two-hop cascade");
        let materialized = client
            .query_one(
                "SELECT \
                     EXISTS (SELECT 1 FROM block WHERE btc_header_hash = $1), \
                     EXISTS (SELECT 1 FROM block WHERE btc_header_hash = $2)",
                &[&dependent_hash, &grandchild_hash],
            )
            .await?;
        if materialized.get::<_, bool>(0) && !materialized.get::<_, bool>(1) {
            reached_partial_frontier = true;
            break;
        }
    }
    assert!(
        reached_partial_frontier,
        "bounded drain commits the child while retaining its grandchild frontier"
    );
    let durable_frontier: bool = client
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM bitcoin_core_reconcile_queue \
                 WHERE source_id = $1 \
                   AND btc_parent_header_hash IN ($2, $3) \
             )",
            &[&source_id, &dependent_hash, &grandchild_hash],
        )
        .await?
        .get(0);
    assert!(
        durable_frontier,
        "the committed child or its discovered grandchild remains durable"
    );
    Ok(())
}

fn tip_missing_config(limit: i64) -> BitcoinCoreSyncConfig {
    BitcoinCoreSyncConfig {
        limit,
        tip: true,
        missing_only: true,
        ..BitcoinCoreSyncConfig::default()
    }
}

fn one_height_missing_config(height: i32) -> BitcoinCoreSyncConfig {
    BitcoinCoreSyncConfig {
        from_height: Some(height),
        to_height: Some(height),
        limit: 1,
        missing_only: true,
        ..BitcoinCoreSyncConfig::default()
    }
}

fn one_height_overwrite_config(height: i32) -> BitcoinCoreSyncConfig {
    BitcoinCoreSyncConfig {
        from_height: Some(height),
        to_height: Some(height),
        tip: false,
        missing_only: false,
        ..BitcoinCoreSyncConfig::default()
    }
}

#[tokio::test]
async fn sync_bitcoin_core_inserts_complete_linked_backbone() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let headers = test_header_chain(2, 1_700_000_000);
        let source = FakeBitcoinCoreBackboneSource::new(2, headers);
        let stats = run_sync_bitcoin_core(
            &mut client,
            &source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(2),
                limit: 3,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        assert_eq!(stats.attempted, 3);
        assert_eq!(stats.completed, 3);
        let complete_rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint \
                 FROM block \
                 WHERE kind = 'canonical' \
                   AND btc_coinbase_status = 'complete' \
                   AND btc_coinbase_script IS NOT NULL",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(complete_rows, 3);
        let cursor: i32 = client
            .query_one(
                "SELECT contiguous_complete_height FROM bitcoin_core_sync_state",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(cursor, 2);
        Ok::<_, anyhow::Error>(())
    })
}

/// w0m regression: under `missing_only` (which `--follow` forces), a retry from a
/// cursor pinned by a coinbase-fetch failure must SKIP the already-complete tail
/// of the page rather than re-fetching it every interval. Column assertions
/// alone cannot prove this (a redundant complete-row rewrite leaves columns
/// unchanged), so this asserts `skipped_complete` AND that the fake source's
/// block_coinbase / block_header are invoked only for the failed early height.
#[tokio::test]
async fn follow_missing_only_retry_skips_completed_tail() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let headers = test_header_chain(5, 1_700_000_000);
        let h2 = header_hash_bytes(&headers[&2]);
        let h3 = header_hash_bytes(&headers[&3]);
        let h4 = header_hash_bytes(&headers[&4]);
        let h5 = header_hash_bytes(&headers[&5]);
        let mut failures = BTreeSet::new();
        failures.insert(h2.clone());

        // Seed: height 2's coinbase fails, leaving it canonical-but-FAILED while
        // 3..5 complete; the contiguous cursor pins at 1 (the gap at 2).
        let seed = FakeBitcoinCoreBackboneSource::with_coinbase_failures(
            5,
            headers.clone(),
            failures.clone(),
        );
        run_sync_bitcoin_core(
            &mut client,
            &seed,
            BitcoinCoreSyncConfig {
                tip: true,
                limit: 10,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;
        let cursor: i32 = client
            .query_one(
                "SELECT contiguous_complete_height FROM bitcoin_core_sync_state",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(cursor, 1, "cursor pins below the failed height");
        let h2_status: String = client
            .query_one(
                "SELECT btc_coinbase_status FROM block WHERE btc_height = 2",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(h2_status, "failed", "height 2 is canonical-but-failed");

        // Retry from the pinned cursor (a fresh source so call counters are clean):
        // a follow-shaped batch (tip + missing_only).
        let retry = FakeBitcoinCoreBackboneSource::with_coinbase_failures(
            5,
            headers.clone(),
            failures.clone(),
        );
        let stats = run_sync_bitcoin_core(&mut client, &retry, tip_missing_config(10)).await?;

        assert_eq!(
            stats.coinbase_failed, 1,
            "only the failed height is re-attempted"
        );
        assert_eq!(
            stats.skipped_complete, 3,
            "the three completed tail heights are skipped, not re-synced"
        );
        assert_eq!(stats.completed, 0, "no new completions on the retry");

        // The failed height IS re-fetched; the completed tail is NOT.
        assert!(retry.coinbase_fetched(&h2), "failed height retried");
        assert!(retry.header_fetched(&h2), "failed height header retried");
        for (label, hash) in [("3", &h3), ("4", &h4), ("5", &h5)] {
            assert!(
                !retry.coinbase_fetched(hash),
                "completed height {label} coinbase must not be re-fetched"
            );
            assert!(
                !retry.header_fetched(hash),
                "completed height {label} header must not be re-fetched"
            );
        }

        Ok::<_, anyhow::Error>(())
    })
}

/// w0m: the follow daemon must initialize its sync-state row before reading the
/// cursor. On a freshly migrated DB no row exists yet; `initialize_follow_state`
/// (a finite, shutdown-free wrapper) must insert the default row and return the
/// default cursor without driving the infinite follow loop.
#[tokio::test]
async fn initialize_follow_state_seeds_row_on_fresh_db() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let before: i64 = client
            .query_one("SELECT count(*)::bigint FROM bitcoin_core_sync_state", &[])
            .await?
            .get(0);
        assert_eq!(before, 0, "fresh DB has no Bitcoin Core sync-state row");

        let cch = initialize_follow_state(&client, source_id).await?;
        assert_eq!(cch, -1, "fresh cursor is the migration default of -1");

        let after: i64 = client
            .query_one("SELECT count(*)::bigint FROM bitcoin_core_sync_state", &[])
            .await?
            .get(0);
        assert_eq!(after, 1, "initialize_follow_state inserts the default row");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn sync_bitcoin_core_tip_limits_and_resumes_contiguous_prefix() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let headers = test_header_chain(5, 1_700_000_000);
        let source = FakeBitcoinCoreBackboneSource::new(5, headers.clone());

        let first = run_sync_bitcoin_core(&mut client, &source, tip_missing_config(2)).await?;

        assert_eq!(first.attempted, 2);
        assert_eq!(first.completed, 2);
        let row = client
            .query_one(
                "SELECT target_tip_height, target_tip_hash, contiguous_complete_height \
                 FROM bitcoin_core_sync_state",
                &[],
            )
            .await?;
        assert_eq!(row.get::<_, Option<i32>>(0), Some(5));
        assert_eq!(
            row.get::<_, Option<Vec<u8>>>(1).as_deref(),
            Some(headers[&5].block_hash().to_byte_array().as_slice())
        );
        assert_eq!(row.get::<_, i32>(2), 1);

        let second = run_sync_bitcoin_core(&mut client, &source, tip_missing_config(2)).await?;

        assert_eq!(second.attempted, 2);
        assert_eq!(second.completed, 2);
        let cursor: i32 = client
            .query_one(
                "SELECT contiguous_complete_height FROM bitcoin_core_sync_state",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(cursor, 3);
        let complete_heights: Vec<i32> = client
            .query(
                "SELECT btc_height \
                 FROM block \
                 WHERE kind = 'canonical' \
                   AND btc_coinbase_status = 'complete' \
                 ORDER BY btc_height",
                &[],
            )
            .await?
            .into_iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(complete_heights, vec![0, 1, 2, 3]);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn sync_bitcoin_core_tip_rejects_changed_target_tip_hash() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original_headers = test_header_chain(2, 1_700_000_000);
        let original_source = FakeBitcoinCoreBackboneSource::new(2, original_headers);
        run_sync_bitcoin_core(&mut client, &original_source, tip_missing_config(1)).await?;

        let reorged_headers = test_header_chain(2, 1_700_001_000);
        let reorged_source = FakeBitcoinCoreBackboneSource::new(2, reorged_headers);
        let err = run_sync_bitcoin_core(&mut client, &reorged_source, tip_missing_config(1))
            .await
            .expect_err("changed same-height target tip should fail");

        assert!(err.to_string().contains("Bitcoin Core target tip changed"));
        let row = client
            .query_one(
                "SELECT last_error_code, last_error_height, last_error_details \
                 FROM bitcoin_core_sync_state",
                &[],
            )
            .await?;
        assert_eq!(
            row.get::<_, Option<String>>(0).as_deref(),
            Some("target_tip_changed")
        );
        assert_eq!(row.get::<_, Option<i32>>(1), Some(2));
        let details: Json<serde_json::Value> = row.get(2);
        assert!(details.0["existing_hash"].is_string());
        assert!(details.0["current_hash"].is_string());

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn sync_bitcoin_core_missing_only_skip_cascades_dependents() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 12);
        let headers = test_header_chain(1, ts as u32);
        let source = FakeBitcoinCoreBackboneSource::new(1, headers.clone());
        let (h1, prev1) = header_hash_and_prev(&headers[&1]);
        insert_block(&client, &h1, &prev1, Some(1), "canonical", ts + 1, None).await?;

        let child = hash_bytes(0x0c0d);
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 44,
                child_hash: hash_bytes(0x4400),
                parent_hash: child.clone(),
                prev_hash: h1.clone(),
                parent_time: ts + 2,
                kind: "unknown",
                pow_validates_btc_target: true,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;
        let before: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&child],
            )
            .await?
            .get(0);
        assert_eq!(before, 0);

        let stats =
            run_sync_bitcoin_core(&mut client, &source, one_height_missing_config(1)).await?;

        assert_eq!(stats.skipped_complete, 1);
        let child_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&child],
            )
            .await?
            .get(0);
        assert_eq!(child_kind, "unknown");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn sync_bitcoin_core_rejects_same_height_canonical_conflict() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let headers = test_header_chain(1, 1_700_000_000);
        let source = FakeBitcoinCoreBackboneSource::new(1, headers.clone());
        let conflicting_hash = hash_bytes(0x0000_c0f1);
        insert_block(
            &client,
            &conflicting_hash,
            &hash_bytes(0x0000_c0f0),
            Some(1),
            "canonical",
            1_700_000_001,
            None,
        )
        .await?;

        let err = run_sync_bitcoin_core(&mut client, &source, one_height_missing_config(1))
            .await
            .expect_err("same-height conflict should fail");
        assert!(err.to_string().contains("same-height canonical conflict"));

        let code: String = client
            .query_one("SELECT last_error_code FROM bitcoin_core_sync_state", &[])
            .await?
            .get(0);
        assert_eq!(code, "backbone_height_conflict");
        let core_hash = header_hash_bytes(&headers[&1]);
        let core_rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&core_hash],
            )
            .await?
            .get(0);
        assert_eq!(core_rows, 0, "conflicting Core row must not be inserted");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn sync_bitcoin_core_rejects_adjacent_link_mismatch() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let headers = test_header_chain(1, 1_700_000_000);
        let source = FakeBitcoinCoreBackboneSource::new(1, headers);
        insert_block(
            &client,
            &hash_bytes(0x0000_bad0),
            &BlockHash::all_zeros().to_byte_array(),
            Some(0),
            "canonical",
            1_700_000_000,
            None,
        )
        .await?;

        let err = run_sync_bitcoin_core(&mut client, &source, one_height_missing_config(1))
            .await
            .expect_err("link mismatch should fail");
        assert!(err.to_string().contains("canonical link mismatch"));

        let details: Json<serde_json::Value> = client
            .query_one(
                "SELECT last_error_details FROM bitcoin_core_sync_state",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(details.0["previous_height"], json!(0));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn follow_repair_switches_production_shaped_fork_atomically() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(4, 1_800_000_000);
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

        let active = fork_header_chain(&original, 3, 4, 1_800_001_000);
        insert_matching_upper_canonical(&client, &active[&4], 4).await?;
        let old_h3 = header_hash_bytes(&original[&3]);
        let new_h3 = header_hash_bytes(&active[&3]);
        let h4 = header_hash_bytes(&active[&4]);
        let old_coinbase_script: Vec<u8> = client
            .query_one(
                "SELECT btc_coinbase_script FROM block WHERE btc_header_hash = $1",
                &[&old_h3],
            )
            .await?
            .get(0);

        let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
        let target = active_source.tip().await?;
        let stats = repair_near_tip_backbone_for_test(
            &mut client,
            &active_source,
            target,
            Duration::ZERO,
            4,
        )
        .await?;

        assert_eq!(
            stats.completed, 2,
            "both 3 and already-matching 4 are replaced"
        );
        let old = client
            .query_one(
                "SELECT kind, btc_height, canonical_competitor_hash, btc_coinbase_script, \
                        btc_coinbase_status, core_attested \
                 FROM block WHERE btc_header_hash = $1",
                &[&old_h3],
            )
            .await?;
        assert_eq!(old.get::<_, String>(0), "stale");
        assert_eq!(old.get::<_, Option<i32>>(1), Some(3));
        assert_eq!(
            old.get::<_, Option<Vec<u8>>>(2).as_deref(),
            Some(new_h3.as_slice())
        );
        assert_eq!(
            old.get::<_, Option<Vec<u8>>>(3).as_deref(),
            Some(old_coinbase_script.as_slice())
        );
        assert_eq!(old.get::<_, String>(4), "complete");
        assert!(old.get::<_, bool>(5));

        let canonical_h3: Vec<u8> = client
            .query_one(
                "SELECT btc_header_hash FROM block WHERE kind = 'canonical' AND btc_height = 3",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(canonical_h3, new_h3);
        let canonical_h4 = client
            .query_one(
                "SELECT btc_header_hash, btc_prev_header_hash, btc_coinbase_status, core_attested \
                 FROM block WHERE kind = 'canonical' AND btc_height = 4",
                &[],
            )
            .await?;
        assert_eq!(canonical_h4.get::<_, Vec<u8>>(0), h4);
        assert_eq!(canonical_h4.get::<_, Vec<u8>>(1), new_h3);
        assert_eq!(canonical_h4.get::<_, String>(2), "complete");
        assert!(canonical_h4.get::<_, bool>(3));

        assert_completed_repair_state(&client, 4, active[&4].block_hash()).await?;
        project_tree(&client, Some("from_height=0&to_height=4")).await?;
        project_tree(&client, None).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn follow_tick_forces_repair_when_scheduled_sweep_is_recent() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(4, 1_800_005_000);
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

        let active = fork_header_chain(&original, 3, 4, 1_800_006_000);
        let old_h3 = header_hash_bytes(&original[&3]);
        let new_h3 = header_hash_bytes(&active[&3]);
        let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
        let cache_classifier = core_cache_classifier(&active, 4);
        let outcome = run_bitcoin_core_follow_tick_for_test(
            &mut client,
            &active_source,
            &cache_classifier,
            BitcoinCoreSyncConfig {
                near_tip_repair_window_heights: 4,
                follow_interval: Duration::from_secs(60),
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        assert_eq!(outcome, (true, true));
        let old_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&old_h3],
            )
            .await?
            .get(0);
        assert_eq!(old_kind, "stale");
        let canonical_h3: Vec<u8> = client
            .query_one(
                "SELECT btc_header_hash FROM block WHERE kind = 'canonical' AND btc_height = 3",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(canonical_h3, new_h3);
        assert_completed_repair_state(&client, 4, active[&4].block_hash()).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn forced_repair_propagates_structural_conflict_below_bounded_view() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let active = test_header_chain(8, 1_800_006_200);
        let active_source = FakeBitcoinCoreBackboneSource::new(8, active.clone());
        run_sync_bitcoin_core(
            &mut client,
            &active_source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(0),
                tip: true,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        insert_matching_upper_canonical(&client, &active[&1], 1).await?;
        let stale_island = fork_header_chain(&active, 2, 2, 1_800_006_400);
        insert_matching_upper_canonical(&client, &stale_island[&2], 2).await?;
        let cache_classifier = core_cache_classifier(&active, 8);
        let err = run_bitcoin_core_follow_tick_for_test(
            &mut client,
            &active_source,
            &cache_classifier,
            BitcoinCoreSyncConfig {
                near_tip_repair_window_heights: 4,
                follow_interval: Duration::from_secs(60),
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await
        .expect_err("a forced sweep cannot hide a structural conflict below its view");

        assert!(
            format!("{err:#}").contains("lies below bounded view start"),
            "unexpected forced-repair error: {err:#}"
        );
        let state = client
            .query_one(
                "SELECT last_error_code, last_error_height FROM bitcoin_core_sync_state",
                &[],
            )
            .await?;
        assert_eq!(
            state.get::<_, Option<String>>(0).as_deref(),
            Some("backbone_link_mismatch"),
            "repair-only telemetry must not mask the earlier sync failure"
        );
        assert_eq!(state.get::<_, Option<i32>>(1), Some(3));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn failed_same_height_follow_repair_is_non_idle_and_visible() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(4, 1_800_007_000);
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

        let active = fork_header_chain(&original, 3, 3, 1_800_008_000);
        let old_h3 = header_hash_bytes(&original[&3]);
        let new_h3 = header_hash_bytes(&active[&3]);
        let cache_classifier = core_cache_classifier(&active, 3);
        let active_source = FakeBitcoinCoreBackboneSource::with_coinbase_failures(
            3,
            active,
            BTreeSet::from([new_h3.clone()]),
        );
        let outcome = run_bitcoin_core_follow_tick_for_test(
            &mut client,
            &active_source,
            &cache_classifier,
            BitcoinCoreSyncConfig {
                near_tip_repair_window_heights: 4,
                follow_interval: Duration::from_secs(60),
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        assert_eq!(outcome, (false, false));
        let state = client
            .query_one(
                "SELECT last_error_code, last_error_details FROM bitcoin_core_sync_state",
                &[],
            )
            .await?;
        assert_eq!(
            state.get::<_, Option<String>>(0).as_deref(),
            Some("near_tip_reorg_repair_retry")
        );
        let details: Json<serde_json::Value> = state.get(1);
        assert_eq!(details.0["retryable"], json!(true));
        let old_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&old_h3],
            )
            .await?
            .get(0);
        assert_eq!(old_kind, "canonical");
        let new_rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&new_h3],
            )
            .await?
            .get(0);
        assert_eq!(new_rows, 0);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn suffix_waits_for_inflight_cascade_reconcile_then_wins() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let original = test_header_chain(4, 1_800_017_000);
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

        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let old_header = original[&3];
        let old_h3 = header_hash_bytes(&old_header);
        let event_id = insert_event_with_real_parent_header(
            &client,
            namecoin,
            79,
            &hash_bytes(0x7910),
            &old_header,
            "canonical",
            Some(3),
        )
        .await?;
        rebuild_source_health(&mut client).await?;

        let gate = FakeParentClassifierGate::new();
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(canonical_verdict(&old_header, 3))
                .with_first_call_gate(gate.clone()),
        );
        let mut reconcile_client = crate::support::db::connect_to_schema(&schema).await?;
        let reconcile_classifier = classifier.clone();
        let reconcile_task = tokio::spawn(async move {
            reconcile_from_merge_mining_event(
                &mut reconcile_client,
                event_id,
                &reconcile_classifier,
                None,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), gate.wait_started())
            .await
            .expect("cascade classifier did not reach its gate");

        let active = fork_header_chain(&original, 3, 4, 1_800_018_000);
        let new_h3 = header_hash_bytes(&active[&3]);
        let active_source = FakeBitcoinCoreBackboneSource::new(4, active);
        let target = active_source.tip().await?;
        let mut repair_client = crate::support::db::connect_to_schema(&schema).await?;
        let repair_pid: i32 = repair_client
            .query_one("SELECT pg_backend_pid()", &[])
            .await?
            .get(0);
        let repair_task = tokio::spawn(async move {
            repair_near_tip_backbone_for_test(
                &mut repair_client,
                &active_source,
                target,
                Duration::ZERO,
                4,
            )
            .await
        });
        wait_for_exclusive_core_view_barrier(&client, repair_pid).await?;

        gate.proceed();
        reconcile_task.await.expect("join cascade reconcile task")?;
        repair_task.await.expect("join suffix repair task")?;

        let event_kind: String = client
            .query_one(
                "SELECT btc_parent_kind FROM merge_mining_event WHERE id = $1",
                &[&event_id],
            )
            .await?
            .get(0);
        assert_eq!(event_kind, "stale");
        let old = client
            .query_one(
                "SELECT kind, canonical_competitor_hash FROM block \
                 WHERE btc_header_hash = $1",
                &[&old_h3],
            )
            .await?;
        assert_eq!(old.get::<_, String>(0), "stale");
        assert_eq!(old.get::<_, Option<Vec<u8>>>(1), Some(new_h3));
        assert_source_health_kind_counts(&client, namecoin, 0, 1).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn ordinary_writer_rechecks_database_after_suffix_switch_aba() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let original = test_header_chain(4, 1_800_019_000);
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

        let active = fork_header_chain(&original, 3, 4, 1_800_019_500);
        let switching_source =
            SwitchingBackboneSource::new(3, original.clone(), 3, original.clone());
        let writer_source = switching_source.clone();
        let mut writer_client = crate::support::db::connect_to_schema(&schema).await?;
        let writer_task = tokio::spawn(async move {
            run_sync_bitcoin_core(
                &mut writer_client,
                &writer_source,
                one_height_overwrite_config(3),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), switching_source.wait_for_coinbase())
            .await
            .expect("ordinary writer did not pause after fetching the old Core hash");

        let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
        repair_near_tip_backbone_for_test(
            &mut client,
            &active_source,
            active_source.tip().await?,
            Duration::ZERO,
            4,
        )
        .await?;
        switching_source.switch_and_release();

        let writer_err = writer_task
            .await
            .expect("join ordinary writer")
            .expect_err("a delayed old-chain writer must reject its stale Core observation");
        assert!(
            writer_err
                .to_string()
                .contains("same-height canonical conflict")
        );

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
            .query_one("SELECT last_error_code FROM bitcoin_core_sync_state", &[])
            .await?
            .get(0);
        assert_eq!(error_code.as_deref(), Some("backbone_height_conflict"));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn concurrent_empty_height_writers_serialize_before_topology_check() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let original = test_header_chain(4, 1_800_019_700);
        let seed_source = FakeBitcoinCoreBackboneSource::new(2, original.clone());
        run_sync_bitcoin_core(
            &mut client,
            &seed_source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(2),
                tip: true,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        let competing = fork_header_chain(&original, 3, 3, 1_800_019_800);
        let source_a = SwitchingBackboneSource::new(3, original.clone(), 3, original);
        let source_b = SwitchingBackboneSource::new(3, competing.clone(), 3, competing);
        let worker_a_source = source_a.clone();
        let worker_b_source = source_b.clone();
        let mut worker_a_client = crate::support::db::connect_to_schema(&schema).await?;
        let mut worker_b_client = crate::support::db::connect_to_schema(&schema).await?;
        let worker_a = tokio::spawn(async move {
            run_sync_bitcoin_core(
                &mut worker_a_client,
                &worker_a_source,
                one_height_overwrite_config(3),
            )
            .await
        });
        let worker_b = tokio::spawn(async move {
            run_sync_bitcoin_core(
                &mut worker_b_client,
                &worker_b_source,
                one_height_overwrite_config(3),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), source_a.wait_for_coinbase())
            .await
            .expect("first ordinary writer did not reach its pre-write gate");
        tokio::time::timeout(Duration::from_secs(5), source_b.wait_for_coinbase())
            .await
            .expect("second ordinary writer did not reach its pre-write gate");
        source_a.switch_and_release();
        source_b.switch_and_release();

        let result_a = worker_a.await.expect("join first ordinary writer");
        let result_b = worker_b.await.expect("join second ordinary writer");
        assert_ne!(
            result_a.is_ok(),
            result_b.is_ok(),
            "exactly one conflicting same-height writer may commit"
        );
        let canonical_count: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block \
                 WHERE kind = 'canonical' AND btc_height = 3",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(canonical_count, 1);
        let error_code: Option<String> = client
            .query_one("SELECT last_error_code FROM bitcoin_core_sync_state", &[])
            .await?
            .get(0);
        assert_eq!(error_code.as_deref(), Some("backbone_height_conflict"));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn follow_repair_coinbase_failure_leaves_fork_unmodified() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(4, 1_800_020_000);
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

        let active = fork_header_chain(&original, 3, 4, 1_800_021_000);
        insert_matching_upper_canonical(&client, &active[&4], 4).await?;
        let old_h3 = header_hash_bytes(&original[&3]);
        let new_h3 = header_hash_bytes(&active[&3]);
        let mut failures = BTreeSet::new();
        failures.insert(new_h3.clone());
        let active_source =
            FakeBitcoinCoreBackboneSource::with_coinbase_failures(4, active, failures);

        repair_near_tip_backbone_for_test(
            &mut client,
            &active_source,
            active_source.tip().await?,
            Duration::ZERO,
            4,
        )
        .await
        .expect_err("replacement coinbase failure must abort before mutation");

        let old_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&old_h3],
            )
            .await?
            .get(0);
        assert_eq!(old_kind, "canonical");
        let new_rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&new_h3],
            )
            .await?
            .get(0);
        assert_eq!(new_rows, 0);
        let sync = client
            .query_one(
                "SELECT target_tip_height, contiguous_complete_height FROM bitcoin_core_sync_state",
                &[],
            )
            .await?;
        assert_eq!(sync.get::<_, Option<i32>>(0), Some(3));
        assert_eq!(sync.get::<_, i32>(1), 3);
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
async fn follow_repair_target_move_leaves_chain_rows_unmodified() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(4, 1_800_025_000);
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

        let captured = fork_header_chain(&original, 3, 4, 1_800_026_000);
        let current = fork_header_chain(&original, 3, 4, 1_800_027_000);
        let target = BitcoinCoreBackboneTip {
            height: 4,
            hash: captured[&4].block_hash(),
        };
        let source = MovingTargetBackboneSource::new(4, captured.clone(), current.clone());
        source.move_target();
        let err =
            repair_near_tip_backbone_for_test(&mut client, &source, target, Duration::ZERO, 4)
                .await
                .expect_err("a moved captured target must fail before replacement");
        assert!(
            err.to_string()
                .contains("target moved during near-tip repair")
        );

        assert_target_move_left_suffix_unmodified(&client, &original, &captured, &current).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[path = "sync_core/reorg_edge_cases.rs"]
mod reorg_edge_cases;

#[path = "sync_core/suffix_status.rs"]
mod suffix_status;
