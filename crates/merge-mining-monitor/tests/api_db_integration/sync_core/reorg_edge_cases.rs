use super::*;
use crate::support::default_pool_snapshot;
use mmm_capture::capture::MergeMiningEventPayload;
use tokio::task::JoinHandle;

type CaptureTask = JoinHandle<Result<i64>>;
type RepairTask = JoinHandle<Result<()>>;

async fn drain_core_reconcile_disabled(client: &mut Client, source_id: i64) -> Result<()> {
    drain_core_reconcile_queue(client, source_id, &ConfiguredParentClassifier::Disabled).await
}

async fn spawn_gated_canonical_capture(
    schema: &str,
    source_id: i64,
    mut payload: MergeMiningEventPayload,
    header: Header,
    height: i32,
) -> Result<(Arc<FakeParentClassifierGate>, CaptureTask)> {
    let gate = FakeParentClassifierGate::new();
    let classifier = ConfiguredParentClassifier::Fake(
        FakeParentClassifier::new(canonical_verdict(&header, height))
            .with_first_call_gate(gate.clone()),
    );
    let mut capture_client = crate::support::db::connect_to_schema(schema).await?;
    let task = tokio::spawn(async move {
        capture_test_payload(&mut capture_client, source_id, &classifier, &mut payload).await
    });
    tokio::time::timeout(Duration::from_secs(5), gate.wait_started())
        .await
        .context("capture classifier did not reach its gate")?;
    Ok((gate, task))
}

async fn spawn_near_tip_repair(
    schema: &str,
    source: FakeBitcoinCoreBackboneSource,
    window_heights: i32,
) -> Result<(BitcoinCoreBackboneTip, i32, RepairTask)> {
    let target = source.tip().await?;
    let mut client = crate::support::db::connect_to_schema(schema).await?;
    let backend_pid: i32 = client
        .query_one("SELECT pg_backend_pid()", &[])
        .await?
        .get(0);
    let task = tokio::spawn(async move {
        repair_near_tip_backbone_for_test(
            &mut client,
            &source,
            target,
            Duration::ZERO,
            window_heights,
        )
        .await
        .map(|_| ())
    });
    Ok((target, backend_pid, task))
}

#[derive(Clone)]
struct GatedBlockHashBackboneSource {
    inner: FakeBitcoinCoreBackboneSource,
    hash_started: Arc<Notify>,
    release_hash: Arc<Notify>,
    hash_gate_claimed: Arc<AtomicBool>,
}

impl GatedBlockHashBackboneSource {
    fn new(inner: FakeBitcoinCoreBackboneSource) -> Self {
        Self {
            inner,
            hash_started: Arc::new(Notify::new()),
            release_hash: Arc::new(Notify::new()),
            hash_gate_claimed: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn wait_for_hash(&self) {
        self.hash_started.notified().await;
    }

    fn release_hash(&self) {
        self.release_hash.notify_one();
    }
}

impl BitcoinCoreBackboneSource for GatedBlockHashBackboneSource {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip> {
        self.inner.tip().await
    }

    async fn block_hash(&self, height: i32) -> Result<BlockHash> {
        if self
            .hash_gate_claimed
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.hash_started.notify_one();
            self.release_hash.notified().await;
        }
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
struct OneShotCursorForkSource {
    active: FakeBitcoinCoreBackboneSource,
    cursor_fork: FakeBitcoinCoreBackboneSource,
    cursor_height: i32,
    served_cursor_fork: Arc<AtomicBool>,
}

impl OneShotCursorForkSource {
    fn new(
        active: FakeBitcoinCoreBackboneSource,
        cursor_fork: FakeBitcoinCoreBackboneSource,
        cursor_height: i32,
    ) -> Self {
        Self {
            active,
            cursor_fork,
            cursor_height,
            served_cursor_fork: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl BitcoinCoreBackboneSource for OneShotCursorForkSource {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip> {
        self.active.tip().await
    }

    async fn block_hash(&self, height: i32) -> Result<BlockHash> {
        if height == self.cursor_height && !self.served_cursor_fork.swap(true, Ordering::SeqCst) {
            self.cursor_fork.block_hash(height).await
        } else {
            self.active.block_hash(height).await
        }
    }

    async fn block_header(&self, hash: BlockHash) -> Result<Header> {
        match self.active.block_header(hash).await {
            Ok(header) => Ok(header),
            Err(_) => self.cursor_fork.block_header(hash).await,
        }
    }

    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        match self.active.block_coinbase(hash).await {
            Ok(coinbase) => Ok(coinbase),
            Err(_) => self.cursor_fork.block_coinbase(hash).await,
        }
    }
}

struct DisplacedAuxpowEvidence<'a> {
    event_id: i64,
    source_id: i64,
    old_hash: &'a [u8],
    new_hash: &'a [u8],
    coinbase_before: &'a [u8],
    bitcoin_miner_pool_id: i64,
    proof_id_before: i64,
}

async fn assert_displaced_auxpow_evidence(
    client: &Client,
    expected: &DisplacedAuxpowEvidence<'_>,
) -> Result<()> {
    let event = client
        .query_one(
            "SELECT btc_parent_kind, btc_parent_height FROM merge_mining_event WHERE id = $1",
            &[&expected.event_id],
        )
        .await?;
    assert_eq!(event.get::<_, String>(0), "stale");
    assert_eq!(event.get::<_, Option<i32>>(1), Some(3));

    let old = client
        .query_one(
            "SELECT kind, canonical_competitor_hash, btc_coinbase_script, \
                    total_attestations, distinct_sources, core_attested, \
                    bitcoin_miner_pool_id \
             FROM block WHERE btc_header_hash = $1",
            &[&expected.old_hash],
        )
        .await?;
    assert_eq!(old.get::<_, String>(0), "stale");
    assert_eq!(
        old.get::<_, Option<Vec<u8>>>(1).as_deref(),
        Some(expected.new_hash)
    );
    assert_eq!(
        old.get::<_, Option<Vec<u8>>>(2).as_deref(),
        Some(expected.coinbase_before)
    );
    assert_eq!(old.get::<_, i32>(3), 1);
    assert_eq!(old.get::<_, i32>(4), 2, "Core plus one AuxPoW source");
    assert!(old.get::<_, bool>(5));
    assert_eq!(
        old.get::<_, Option<i64>>(6),
        Some(expected.bitcoin_miner_pool_id)
    );

    let proof = client
        .query_one(
            "SELECT id, evidence FROM attestation_proof \
             WHERE btc_header_hash = $1 AND source_id = $2 AND proof_kind = 'auxpow'",
            &[&expected.old_hash, &expected.source_id],
        )
        .await?;
    assert_eq!(proof.get::<_, i64>(0), expected.proof_id_before);
    let proof_evidence: Json<serde_json::Value> = proof.get(1);
    assert_eq!(
        proof_evidence.0["contributing_event_ids"],
        json!([expected.event_id])
    );
    assert_source_health_kind_counts(client, expected.source_id, 0, 1).await
}

struct StaleDependentFixture {
    event_id: i64,
    event_hash: Vec<u8>,
    core_only_hash: Vec<u8>,
}

async fn seed_stale_dependents(
    client: &mut Client,
    namecoin: i64,
    original: &BTreeMap<i32, Header>,
    old_h3: &[u8],
) -> Result<StaleDependentFixture> {
    let event_stale_chain = fork_header_chain(original, 3, 3, 1_800_027_150);
    let event_stale_header = event_stale_chain[&3];
    let event_hash = header_hash_bytes(&event_stale_header);
    let event_id = insert_event_with_real_parent_header(
        client,
        namecoin,
        81,
        &hash_bytes(0x8110),
        &event_stale_header,
        "stale",
        Some(3),
    )
    .await?;
    let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
        crate::support::scenario::stale_verdict(&event_stale_header, 3, old_h3.to_vec()),
    ));
    reconcile_from_merge_mining_event(client, event_id, &classifier, None).await?;

    let mut core_only_header = event_stale_header;
    core_only_header.nonce = core_only_header.nonce.wrapping_add(1);
    let core_only_hash = header_hash_bytes(&core_only_header);
    insert_block(
        client,
        &core_only_hash,
        &core_only_header.prev_blockhash.to_byte_array(),
        Some(3),
        "stale",
        i64::from(core_only_header.time),
        Some(old_h3),
    )
    .await?;
    client
        .execute(
            "UPDATE block SET core_attested = TRUE, live_observed = TRUE, \
                 distinct_sources = 99, difficulty_epoch_ok = TRUE \
             WHERE btc_header_hash = $1",
            &[&core_only_hash],
        )
        .await?;

    Ok(StaleDependentFixture {
        event_id,
        event_hash,
        core_only_hash,
    })
}

async fn assert_rebound_stale_dependents(
    client: &Client,
    fixture: &StaleDependentFixture,
    competitor: &[u8],
) -> Result<()> {
    let event_kind: String = client
        .query_one(
            "SELECT btc_parent_kind FROM merge_mining_event WHERE id = $1",
            &[&fixture.event_id],
        )
        .await?
        .get(0);
    assert_eq!(event_kind, "stale");
    for (hash, expected_attestations, expected_sources) in [
        (&fixture.event_hash, 1_i64, 2_i64),
        (&fixture.core_only_hash, 0_i64, 1_i64),
    ] {
        let row = client
            .query_one(
                "SELECT kind, canonical_competitor_hash, core_attested, \
                        total_attestations::bigint, distinct_sources::bigint \
                 FROM block WHERE btc_header_hash = $1",
                &[hash],
            )
            .await?;
        assert_eq!(row.get::<_, String>(0), "stale");
        assert_eq!(
            row.get::<_, Option<Vec<u8>>>(1).as_deref(),
            Some(competitor)
        );
        assert!(row.get::<_, bool>(2));
        assert_eq!(row.get::<_, i64>(3), expected_attestations);
        assert_eq!(row.get::<_, i64>(4), expected_sources);
    }
    Ok(())
}

async fn assert_inferred_stale_child(
    client: &Client,
    event_id: i64,
    hash: &[u8],
    competitor: &[u8],
    source_id: i64,
) -> Result<()> {
    let event = client
        .query_one(
            "SELECT btc_parent_kind, btc_parent_height \
             FROM merge_mining_event WHERE id = $1",
            &[&event_id],
        )
        .await?;
    assert_eq!(event.get::<_, String>(0), "stale");
    assert_eq!(event.get::<_, Option<i32>>(1), Some(4));
    let block = client
        .query_one(
            "SELECT kind, btc_height, btc_height_source, canonical_competitor_hash, \
                    core_attested \
             FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await?;
    assert_eq!(block.get::<_, String>(0), "stale");
    assert_eq!(block.get::<_, Option<i32>>(1), Some(4));
    assert_eq!(
        block.get::<_, Option<String>>(2).as_deref(),
        Some("prev-stale")
    );
    assert_eq!(
        block.get::<_, Option<Vec<u8>>>(3).as_deref(),
        Some(competitor)
    );
    assert!(!block.get::<_, bool>(4));
    assert_source_health_kind_counts(client, source_id, 0, 1).await
}

async fn assert_retained_core_primary(client: &Client, source_id: i64, hash: &[u8]) -> Result<()> {
    let retained = client
        .query_one(
            "SELECT primary_pending FROM bitcoin_core_reconcile_queue \
             WHERE source_id = $1 AND btc_parent_header_hash = $2",
            &[&source_id, &hash],
        )
        .await?;
    assert!(retained.get::<_, bool>(0));
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

#[tokio::test]
async fn follow_repair_preserves_displaced_auxpow_evidence_and_source_health() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let (_, pool_ids_by_slug) = default_pool_snapshot(&client).await?;
        let kncminer = *pool_ids_by_slug
            .get("kncminer")
            .context("default snapshot missing kncminer")?;
        let original = test_header_chain(4, 1_800_010_000);
        let event_id = insert_event_with_real_parent_header(
            &client,
            namecoin,
            77,
            &hash_bytes(0x7700),
            &original[&3],
            "canonical",
            Some(3),
        )
        .await?;
        // The child event claims F2Pool, while the persisted Core coinbase
        // resolves to KnCMiner. A displaced stale replay must keep Core's
        // higher-precedence attribution.
        let event_coinbase_script = b"/F2Pool/".to_vec();
        client
            .execute(
                "UPDATE merge_mining_event SET btc_parent_coinbase_script = $2 WHERE id = $1",
                &[&event_id, &event_coinbase_script],
            )
            .await?;
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
        let old_h3 = header_hash_bytes(&original[&3]);
        let core_coinbase_script = b"/KnCMiner/".to_vec();
        client
            .execute(
                "UPDATE block SET btc_coinbase_script = $2, bitcoin_miner_pool_id = $3 \
                 WHERE btc_header_hash = $1",
                &[&old_h3, &core_coinbase_script, &kncminer],
            )
            .await?;
        let proof_id_before: i64 = client
            .query_one(
                "SELECT id FROM attestation_proof \
                 WHERE btc_header_hash = $1 AND source_id = $2 AND proof_kind = 'auxpow'",
                &[&old_h3, &namecoin],
            )
            .await?
            .get(0);
        rebuild_source_health(&mut client).await?;

        let active = fork_header_chain(&original, 3, 4, 1_800_011_000);
        insert_matching_upper_canonical(&client, &active[&4], 4).await?;
        let new_h3 = header_hash_bytes(&active[&3]);
        let coinbase_before: Vec<u8> = client
            .query_one(
                "SELECT btc_coinbase_script FROM block WHERE btc_header_hash = $1",
                &[&old_h3],
            )
            .await?
            .get(0);

        let active_source = FakeBitcoinCoreBackboneSource::new(4, active.clone());
        repair_near_tip_backbone_for_test(
            &mut client,
            &active_source,
            active_source.tip().await?,
            Duration::ZERO,
            4,
        )
        .await?;

        assert_displaced_auxpow_evidence(
            &client,
            &DisplacedAuxpowEvidence {
                event_id,
                source_id: namecoin,
                old_hash: &old_h3,
                new_hash: &new_h3,
                coinbase_before: &coinbase_before,
                bitcoin_miner_pool_id: kncminer,
                proof_id_before,
            },
        )
        .await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn gap_plan_retries_suffix_after_inflight_capture_commits_conflict() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let original = test_header_chain(5, 1_800_014_000);
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

        let fixture = NamecoinEventFixture::new(&client).await?;
        let old_header = original[&3];
        let old_h3 = header_hash_bytes(&old_header);
        let mut payload = fixture.payload(77, ClassificationProof::default(), 1_800_014_100)?;
        payload.child_block_hash = Some(hash_bytes(0x7710));
        payload.btc_parent_header_hash = old_h3.clone();
        payload.btc_parent_prev_header_hash = old_header.prev_blockhash.to_byte_array().to_vec();
        payload.btc_parent_header_bytes = serialize(&old_header);
        payload.btc_parent_header_time = old_header.time as i64;
        let (gate, capture_task) =
            spawn_gated_canonical_capture(&schema, fixture.source_id, payload, old_header, 3)
                .await?;

        let active = fork_header_chain(&original, 3, 4, 1_800_014_500);
        let new_h3 = header_hash_bytes(&active[&3]);
        let active_source = FakeBitcoinCoreBackboneSource::new(4, active);
        let (target, repair_pid, repair_task) =
            spawn_near_tip_repair(&schema, active_source, 4).await?;

        // Height 3 is absent from the committed local view, so reaching this
        // exclusive wait proves the initial scan selected the gap path. The
        // classifier still owns the shared barrier and has not committed its
        // old-branch canonical row yet.
        wait_for_exclusive_core_view_barrier(&client, repair_pid).await?;
        assert!(
            !repair_task.is_finished(),
            "gap repair waits while capture holds the shared Core-view barrier"
        );

        gate.proceed();
        let event_id = capture_task.await.expect("join capture task")?;
        repair_task.await.expect("join repair task")?;

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
        assert_completed_repair_state(&client, target.height, target.hash).await?;
        assert_source_health_kind_counts(&client, fixture.source_id, 0, 1).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn suffix_waits_for_inflight_capture_then_reclassifies_it() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let original = test_header_chain(4, 1_800_015_000);
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

        let fixture = NamecoinEventFixture::new(&client).await?;
        let old_header = original[&3];
        let old_h3 = header_hash_bytes(&old_header);
        let mut payload = fixture.payload(78, ClassificationProof::default(), 1_800_015_100)?;
        payload.child_block_hash = Some(hash_bytes(0x7810));
        payload.btc_parent_header_hash = old_h3.clone();
        payload.btc_parent_prev_header_hash = old_header.prev_blockhash.to_byte_array().to_vec();
        payload.btc_parent_header_bytes = serialize(&old_header);
        payload.btc_parent_header_time = old_header.time as i64;
        let (gate, capture_task) =
            spawn_gated_canonical_capture(&schema, fixture.source_id, payload, old_header, 3)
                .await?;

        let active = fork_header_chain(&original, 3, 4, 1_800_016_000);
        let new_h3 = header_hash_bytes(&active[&3]);
        let active_source = FakeBitcoinCoreBackboneSource::new(4, active);
        let (_, repair_pid, repair_task) = spawn_near_tip_repair(&schema, active_source, 4).await?;
        wait_for_exclusive_core_view_barrier(&client, repair_pid).await?;
        assert!(
            !repair_task.is_finished(),
            "exclusive suffix switch waits while capture holds the shared Core-view barrier"
        );

        gate.proceed();
        let event_id = capture_task.await.expect("join capture task")?;
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
        assert_source_health_kind_counts(&client, fixture.source_id, 0, 1).await?;

        Ok::<_, anyhow::Error>(())
    })
}

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
async fn suffix_rebinds_stale_dependents_before_queue_drain() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let original = test_header_chain(4, 1_800_027_050);
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

        let old_h3 = header_hash_bytes(&original[&3]);
        let stale = seed_stale_dependents(&mut client, namecoin, &original, &old_h3).await?;

        let active = fork_header_chain(&original, 3, 4, 1_800_027_250);
        let new_h3 = header_hash_bytes(&active[&3]);
        insert_block(
            &client,
            &new_h3,
            &active[&3].prev_blockhash.to_byte_array(),
            Some(3),
            "stale",
            i64::from(active[&3].time),
            Some(&old_h3),
        )
        .await?;
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
        let promoted_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&new_h3],
            )
            .await?
            .get(0);
        assert_eq!(promoted_kind, "canonical");
        for (hash, expected_sources) in
            [(&stale.event_hash, 2_i64), (&stale.core_only_hash, 99_i64)]
        {
            let row = client
                .query_one(
                    "SELECT canonical_competitor_hash, distinct_sources::bigint \
                     FROM block WHERE btc_header_hash = $1",
                    &[hash],
                )
                .await?;
            assert_eq!(row.get::<_, Option<Vec<u8>>>(0), Some(new_h3.clone()));
            assert_eq!(row.get::<_, i64>(1), expected_sources);
        }

        drain_core_reconcile_disabled(&mut client, bitcoin).await?;
        assert_rebound_stale_dependents(&client, &stale, &new_h3).await?;
        assert_source_health_kind_counts(&client, namecoin, 0, 1).await?;
        assert_completed_repair_state(&client, 4, active[&4].block_hash()).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn suffix_queue_retries_then_reclassifies_newly_inferable_unknown_child() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let original = test_header_chain(3, 1_800_027_300);
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

        // This parent extended the old tip while no same-height canonical
        // competitor existed, so its persisted verdict remained unknown.
        let unknown_parent = Header {
            version: Version::ONE,
            prev_blockhash: original[&3].block_hash(),
            merkle_root: TxMerkleNode::all_zeros(),
            time: 1_800_027_404,
            bits: original[&3].bits,
            nonce: 44_404,
        };
        let unknown_hash = header_hash_bytes(&unknown_parent);
        let event_id = insert_event_with_real_parent_header(
            &client,
            namecoin,
            82,
            &hash_bytes(0x8210),
            &unknown_parent,
            "unknown",
            None,
        )
        .await?;

        let active = fork_header_chain(&original, 3, 4, 1_800_027_500);
        let new_h4 = header_hash_bytes(&active[&4]);
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
        let before_kind: String = client
            .query_one(
                "SELECT btc_parent_kind FROM merge_mining_event WHERE id = $1",
                &[&event_id],
            )
            .await?
            .get(0);
        assert_eq!(before_kind, "unknown");

        let mut inferred =
            crate::support::scenario::stale_verdict(&unknown_parent, 4, new_h4.clone());
        inferred.height_source = Some(mmm_bitcoin_core::HeightSource::PrevStale);
        inferred.live_observed = false;
        inferred.core_attested = false;
        let fake = FakeParentClassifier::new(inferred).with_classification_error_on_call(1);
        let classifier = ConfiguredParentClassifier::Fake(fake.clone());

        let err = drain_core_reconcile_queue(&mut client, bitcoin, &classifier)
            .await
            .expect_err("a transient strict classifier failure must retain queue work");
        assert!(format!("{err:#}").contains("injected classification error"));
        assert_retained_core_primary(&client, bitcoin, &unknown_hash).await?;

        drain_core_reconcile_queue(&mut client, bitcoin, &classifier).await?;
        assert_inferred_stale_child(&client, event_id, &unknown_hash, &new_h4, namecoin).await?;
        assert_eq!(fake.call_count().await, 2);
        assert_drained_core_queue(&client, bitcoin).await?;

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
async fn blocked_integrity_error_preserves_new_reconcile_pending_status() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let original = test_header_chain(4, 1_800_027_550);
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

        let active = fork_header_chain(&original, 3, 3, 1_800_027_650);
        let gated_source =
            GatedBlockHashBackboneSource::new(FakeBitcoinCoreBackboneSource::new(3, active));
        let writer_source = gated_source.clone();
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
        tokio::time::timeout(Duration::from_secs(5), gated_source.wait_for_hash())
            .await
            .expect("ordinary writer did not pause before its topology preflight");

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

        gated_source.release_hash();
        wait_for_backend_blocked_by(&client, writer_pid, blocker_pid).await?;

        let pending_hash = header_hash_bytes(&original[&3]);
        let pending_details = Json(json!({ "sentinel": "suffix-pending" }));
        blocker
            .execute(
                "INSERT INTO bitcoin_core_reconcile_queue (source_id, btc_parent_header_hash) \
                 VALUES ($1, $2)",
                &[&bitcoin, &pending_hash],
            )
            .await?;
        blocker
            .execute(
                "UPDATE bitcoin_core_sync_state \
                 SET last_error_code = 'backbone_reorg_reconcile_pending', \
                     last_error_height = 3, last_error = 'suffix pending', \
                     last_error_details = $2 \
                 WHERE source_id = $1 AND sync_mode = 'contiguous'",
                &[&bitcoin, &pending_details],
            )
            .await?;
        blocker.commit().await?;

        let err = writer
            .await
            .expect("join blocked ordinary writer")
            .expect_err("the forked Core hash must still fail the topology guard");
        assert!(err.to_string().contains("same-height canonical conflict"));
        let state = client
            .query_one(
                "SELECT last_error_code, last_error_height, last_error, last_error_details \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(
            state.get::<_, Option<String>>(0).as_deref(),
            Some("backbone_reorg_reconcile_pending")
        );
        assert_eq!(state.get::<_, Option<i32>>(1), Some(3));
        assert_eq!(
            state.get::<_, Option<String>>(2).as_deref(),
            Some("suffix pending")
        );
        assert_eq!(
            state.get::<_, Json<serde_json::Value>>(3).0,
            pending_details.0
        );

        drain_core_reconcile_disabled(&mut client, bitcoin).await?;
        assert_drained_core_queue(&client, bitcoin).await?;

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
                 SET last_error_code = 'concurrent_error', last_error_height = 1, \
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
        assert_eq!(error.get::<_, Option<i32>>(1), Some(1));

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
async fn repair_status_preserves_unrelated_error_and_clears_repair_owned_errors() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let headers = test_header_chain(3, 1_800_027_800);
        let source = FakeBitcoinCoreBackboneSource::new(2, headers.clone());
        run_sync_bitcoin_core(
            &mut client,
            &source,
            BitcoinCoreSyncConfig {
                from_height: Some(0),
                to_height: Some(2),
                tip: true,
                missing_only: true,
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;
        let target = source.tip().await?;
        let target_height = target.height;
        let target_hash = target.hash.to_byte_array().to_vec();

        let coinbase_details = Json(json!({ "sentinel": "unrelated-coinbase-error" }));
        client
            .execute(
                "UPDATE bitcoin_core_sync_state \
                 SET target_tip_height = NULL, target_tip_hash = NULL, \
                     last_error_code = 'coinbase_fetch_failed', last_error_height = 0, \
                     last_error = 'persisted coinbase failure', last_error_details = $2 \
                 WHERE source_id = $1 AND sync_mode = 'contiguous'",
                &[&bitcoin, &coinbase_details],
            )
            .await?;

        let regressed_source = FakeBitcoinCoreBackboneSource::new(1, headers);
        let err = repair_near_tip_backbone_for_test(
            &mut client,
            &regressed_source,
            regressed_source.tip().await?,
            Duration::ZERO,
            4,
        )
        .await
        .expect_err("a target below the contiguous cursor must fail closed");
        assert!(
            err.to_string()
                .contains("below the proven contiguous cursor")
        );

        record_retryable_repair_failure_for_test(&mut client, bitcoin).await?;
        accept_live_repaired_target_for_test(&mut client, &source, bitcoin, target).await?;
        let preserved = client
            .query_one(
                "SELECT last_error_code, last_error_height, last_error, last_error_details, \
                        target_tip_height, target_tip_hash \
                 FROM bitcoin_core_sync_state WHERE source_id = $1",
                &[&bitcoin],
            )
            .await?;
        assert_eq!(
            preserved.get::<_, Option<String>>(0).as_deref(),
            Some("coinbase_fetch_failed")
        );
        assert_eq!(preserved.get::<_, Option<i32>>(1), Some(0));
        assert_eq!(
            preserved.get::<_, Option<String>>(2).as_deref(),
            Some("persisted coinbase failure")
        );
        assert_eq!(
            preserved.get::<_, Json<serde_json::Value>>(3).0,
            coinbase_details.0
        );
        assert_eq!(preserved.get::<_, Option<i32>>(4), Some(target_height));
        assert_eq!(preserved.get::<_, Option<Vec<u8>>>(5), Some(target_hash));

        for owned_code in [
            "near_tip_reorg_repair_retry",
            "target_tip_changed",
            "near_tip_reorg_repair_failed",
            "live_window_invariant_failed",
        ] {
            client
                .execute(
                    "UPDATE bitcoin_core_sync_state \
                     SET last_error_code = $2, last_error_height = 2, \
                         last_error = 'repair-owned failure', \
                         last_error_details = '{\"retryable\":true}'::jsonb \
                     WHERE source_id = $1 AND sync_mode = 'contiguous'",
                    &[&bitcoin, &owned_code],
                )
                .await?;
            accept_live_repaired_target_for_test(&mut client, &source, bitcoin, target).await?;
            let cleared = client
                .query_one(
                    "SELECT last_error_code, last_error_height, last_error, last_error_details \
                     FROM bitcoin_core_sync_state WHERE source_id = $1",
                    &[&bitcoin],
                )
                .await?;
            assert_eq!(cleared.get::<_, Option<String>>(0), None);
            assert_eq!(cleared.get::<_, Option<i32>>(1), None);
            assert_eq!(cleared.get::<_, Option<String>>(2), None);
            assert_eq!(cleared.get::<_, Json<serde_json::Value>>(3).0, json!({}));
        }

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
        assert_eq!(details.0["view"], json!("live_tip"));
        assert_eq!(details.0["target_tip_height"], json!(4));
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

fn assert_bounded_cursor_recovery_rpc(source: &FakeBitcoinCoreBackboneSource) {
    let header_calls = source.header_calls.lock().unwrap().len();
    assert!(
        header_calls <= 15,
        "cursor recovery RPC work must stay bounded by the cursor and live windows, not the target gap; fetched {header_calls} headers"
    );
}

#[tokio::test]
async fn cursor_repair_revalidates_its_own_target_before_mutation() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(3, 1_800_033_000);
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

        let active = fork_header_chain(&original, 2, 80, 1_800_034_000);
        let transient = fork_header_chain(&original, 2, 2, 1_800_034_500);
        let source = OneShotCursorForkSource::new(
            FakeBitcoinCoreBackboneSource::new(80, active),
            FakeBitcoinCoreBackboneSource::new(2, transient.clone()),
            2,
        );

        let err = repair_near_tip_backbone_for_test(
            &mut client,
            &source,
            source.tip().await?,
            Duration::ZERO,
            4,
        )
        .await
        .expect_err("a cursor view from a transient branch must not be committed");
        assert!(format!("{err:#}").contains("target moved during near-tip repair"));
        assert_eq!(canonical_view(&client, 2, 2).await?.len(), 1);
        let transient_rows: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE btc_header_hash = $1",
                &[&header_hash_bytes(&transient[&2])],
            )
            .await?
            .get(0);
        assert_eq!(transient_rows, 0);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn scheduled_follow_repairs_stale_cursor_clears_error_and_resumes() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let original = test_header_chain(81, 1_800_035_000);
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

        let active = fork_header_chain(&original, 2, 80, 1_800_036_000);
        let old_h2 = header_hash_bytes(&original[&2]);
        insert_matching_upper_canonical(&client, &active[&2], 2).await?;
        insert_matching_upper_canonical(&client, &active[&3], 3).await?;
        client
            .execute(
                "UPDATE bitcoin_core_sync_state \
                 SET last_scanned_height = 3, last_attempted_height = 3, \
                     last_error_code = 'backbone_link_mismatch', last_error_height = 3, \
                     last_error = 'canonical link mismatch after delayed startup', \
                     last_error_details = '{\"previous_height\": 2}'::jsonb",
                &[],
            )
            .await?;
        let active_source = FakeBitcoinCoreBackboneSource::new(80, active.clone());
        let cache_classifier = core_cache_classifier(&active, 80);
        let outcome = run_bitcoin_core_initial_follow_tick_for_test(
            &mut client,
            &active_source,
            &cache_classifier,
            BitcoinCoreSyncConfig {
                limit: 3,
                near_tip_repair_window_heights: 4,
                follow_interval: Duration::from_secs(60),
                ..BitcoinCoreSyncConfig::default()
            },
        )
        .await?;

        assert_eq!(outcome, (true, false));
        let old_kind: String = client
            .query_one(
                "SELECT kind FROM block WHERE btc_header_hash = $1",
                &[&old_h2],
            )
            .await?
            .get(0);
        assert_eq!(old_kind, "stale");
        for height in [2, 3, 4, 5, 77, 78, 79, 80] {
            let row = client
                .query_one(
                    "SELECT btc_header_hash, btc_coinbase_status \
                     FROM block WHERE kind = 'canonical' AND btc_height = $1",
                    &[&height],
                )
                .await?;
            assert_eq!(
                row.get::<_, Vec<u8>>(0),
                header_hash_bytes(&active[&height])
            );
            assert_eq!(row.get::<_, String>(1), "complete");
        }
        let unsynced_h6: i64 = client
            .query_one(
                "SELECT count(*)::bigint FROM block WHERE kind = 'canonical' AND btc_height = 6",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(unsynced_h6, 0, "ordinary catch-up remains batch-bounded");
        let sync = client
            .query_one(
                "SELECT target_tip_height, target_tip_hash, contiguous_complete_height, \
                        last_error_code \
                 FROM bitcoin_core_sync_state",
                &[],
            )
            .await?;
        assert_eq!(sync.get::<_, Option<i32>>(0), Some(80));
        assert_eq!(
            sync.get::<_, Option<Vec<u8>>>(1).as_deref(),
            Some(active[&80].block_hash().to_byte_array().as_slice())
        );
        assert_eq!(sync.get::<_, i32>(2), 5);
        assert_eq!(sync.get::<_, Option<String>>(3), None);
        assert_bounded_cursor_recovery_rpc(&active_source);
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
        drain_core_reconcile_disabled(&mut restart_client, bitcoin).await?;
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
