use std::str::FromStr;

use anyhow::Result;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification};
use mmm_capture::capture::{
    ClassificationProof, HATHOR_REVOKE_NON_BTC, MergeMiningEventPayload, ResolvedPoolAttributions,
    build_event_payload,
};
use mmm_capture::nbits_table::daa_epoch_start;
use mmm_capture::source_registry::{HATHOR_SOURCE_CODE, NAMECOIN_SOURCE_CODE};
use mmm_producers::chains::hathor::{
    HathorBlockMeta, HathorCaptureContext, HathorHeightOutcome, HathorRpc, HathorTransaction,
    forge_with_weight, process_hathor_height, reconstruct_from_blobs,
};
use mmm_store::{get_source_id, upsert_merge_mining_event};
use tokio_postgres::Client;

use crate::support::parse_auxpow_fixture;

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
    hathor_fixture(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/hathor/1971823.json"
    )))
}

/// A committed Hathor block fixture as `(height, rpc)`. The fixture RPC answers
/// every height with its block, so a block from one height can be served at
/// another to stand in for a child-chain replacement.
fn hathor_fixture(json: &str) -> (i32, FixtureHathorRpc) {
    let j: serde_json::Value = serde_json::from_str(json).expect("deserialize Hathor fixture");
    let height = j["hathor_height"].as_i64().unwrap() as i32;
    let meta = HathorBlockMeta {
        tx_id: j["tx_id"].as_str().unwrap().to_owned(),
        version: j["version"].as_i64().unwrap() as i32,
        height,
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

/// A display-order Hathor block hash in internal byte order, as stored.
fn internal_hash(tx_id: &str) -> Vec<u8> {
    bitcoin::BlockHash::from_str(tx_id)
        .unwrap()
        .to_byte_array()
        .to_vec()
}

/// `(child_block_hash, child_displaced_by, revoked_at)` of one event.
type DisplacementRow = (Vec<u8>, Option<Vec<u8>>, Option<i64>);

/// Every event at the height, ordered by hash.
async fn displacement_at(
    client: &Client,
    source_id: i64,
    height: i32,
) -> Result<Vec<DisplacementRow>> {
    let rows = client
        .query(
            "SELECT child_block_hash, child_displaced_by, revoked_at \
             FROM merge_mining_event \
             WHERE source_id = $1 AND child_height = $2 \
             ORDER BY child_block_hash",
            &[&source_id, &height],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect())
}

async fn advisory_locks_held(client: &Client) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
            &[],
        )
        .await?
        .get(0))
}

/// Seed the Core cache so the parents of both fixtures classify Valid: the
/// cache reaches B's parent (751,763) with B's nBits, and A's epoch keeps A's.
async fn seed_core_cache_for_both_fixtures(client: &Client) -> Result<()> {
    crate::support::db::seed_bitcoin_core_header_cache_through(
        client,
        751_763,
        i64::MAX,
        0x1709_ed88,
    )
    .await?;
    client
        .execute(
            "UPDATE bitcoin_core_header SET bits = $1 WHERE height = $2",
            &[&i64::from(0x170c_69ea_u32), &daa_epoch_start(710_969)],
        )
        .await?;
    Ok(())
}

/// A copy of a fixture RPC with its response edited: what the endpoint would
/// answer if it misplaced, voided or corrupted that block.
fn variant_of(
    rpc: &FixtureHathorRpc,
    edit: impl FnOnce(&mut HathorBlockMeta, &mut HathorTransaction),
) -> FixtureHathorRpc {
    let mut meta = rpc.meta.clone();
    let mut tx = rpc.tx.clone();
    edit(&mut meta, &mut tx);
    FixtureHathorRpc { meta, tx }
}

#[tokio::test]
async fn a_replaced_hathor_block_is_displaced_and_restored_when_it_returns() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc_a) = hathor_1971823_fixture();
        // A real block from another height, which the endpoint now places at
        // this one: the chain's replacement for A. The proof cannot bind a
        // block to a height; the position is the endpoint's assertion, as it is
        // for every captured event.
        let (_, mut rpc_b) = hathor_fixture(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hathor/2773476.json"
        )));
        rpc_b.meta.height = height;
        let hash_a = internal_hash(&rpc_a.meta.tx_id);
        let hash_b = internal_hash(&rpc_b.meta.tx_id);

        seed_core_cache_for_both_fixtures(&client).await?;
        let context = HathorCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        let source_id = context.source_id();
        let sorted = |mut rows: Vec<DisplacementRow>| {
            rows.sort();
            rows
        };

        // Tick 1: A is captured and is the chain's block.
        assert_eq!(
            process_hathor_height(&mut client, &rpc_a, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            vec![(hash_a.clone(), None, None)]
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);

        // Tick 2: the chain now carries B. B is captured, A is displaced by it
        // and stays active: it is still valid Bitcoin-side evidence.
        assert_eq!(
            process_hathor_height(&mut client, &rpc_b, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let after_b = sorted(vec![
            (hash_a.clone(), Some(hash_b.clone()), None),
            (hash_b.clone(), None, None),
        ]);
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

        // Tick 3: B answered for its own height rather than the one asked for
        // holds the height for a retry; nothing changes.
        let misrouted = variant_of(&rpc_b, |meta, _| meta.height = 2_773_476);
        assert_eq!(
            process_hathor_height(&mut client, &misrouted, &context, height).await?,
            HathorHeightOutcome::TransientHold
        );
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

        // Tick 4: a response whose reconstruction identity is broken proves
        // nothing, so it changes nothing.
        let tampered = variant_of(&rpc_b, |_, tx| {
            let last = tx.raw.pop().unwrap();
            tx.raw.push(if last == '0' { '1' } else { '0' });
        });
        assert_eq!(
            process_hathor_height(&mut client, &tampered, &context, height).await?,
            HathorHeightOutcome::MalformedSkipped
        );
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

        // Tick 5: B reported voided names no replacement: nothing changes and
        // nothing is revoked.
        let voided = variant_of(&rpc_b, |meta, _| meta.is_voided = true);
        assert_eq!(
            process_hathor_height(&mut client, &voided, &context, height).await?,
            HathorHeightOutcome::VoidedSkipped
        );
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

        // Tick 6: A again. A is restored and B displaced by it; nothing was
        // revoked, and no lock or held height is left behind.
        assert_eq!(
            process_hathor_height(&mut client, &rpc_a, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            sorted(vec![
                (hash_a.clone(), None, None),
                (hash_b.clone(), Some(hash_a.clone()), None),
            ])
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);
        let pending: i64 = client
            .query_one(
                "SELECT count(*) FROM poll_pending_reconcile WHERE source_id = $1",
                &[&source_id],
            )
            .await?
            .get(0);
        assert_eq!(pending, 0);
        Ok(())
    })
}

#[tokio::test]
async fn a_block_declaring_trivial_work_does_not_displace_the_captured_block() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc_a) = hathor_1971823_fixture();
        let context = hathor_context(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        let source_id = context.source_id();
        let hash_a = internal_hash(&rpc_a.meta.tx_id);
        assert_eq!(
            process_hathor_height(&mut client, &rpc_a, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );

        // A self-consistent response built from A's own bytes that declares
        // almost no work: the reconstruction identity holds and the trivial
        // target is met, but the block captured at the height declares about
        // 2^68 hashes, and a block that far below it is not the chain's block.
        let raw = hex::decode(&rpc_a.tx.raw)?;
        let aux_pow = hex::decode(rpc_a.tx.aux_pow.as_deref().unwrap())?;
        let (_aux, recon) = reconstruct_from_blobs(
            &raw,
            &aux_pow,
            bitcoin::BlockHash::from_str(&rpc_a.meta.tx_id)?,
        )?;
        let (forged_raw, forged_hash) =
            forge_with_weight(&raw, &aux_pow, recon.funds_graph_split, 1e-6)?;
        let forged = FixtureHathorRpc {
            meta: HathorBlockMeta {
                tx_id: forged_hash.to_string(),
                version: 3,
                height,
                is_voided: false,
            },
            tx: HathorTransaction {
                raw: hex::encode(&forged_raw),
                aux_pow: rpc_a.tx.aux_pow.clone(),
                hash: forged_hash.to_string(),
                timestamp: rpc_a.tx.timestamp,
            },
        };
        assert_eq!(
            process_hathor_height(&mut client, &forged, &context, height).await?,
            HathorHeightOutcome::NearSkipped
        );
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            vec![(hash_a, None, None)],
            "A stays the chain's block, undisplaced"
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);
        Ok(())
    })
}

/// One exact Hathor-source observation at `height` with a synthetic child
/// hash, observed at `confirmed_at`. The child header is dropped so the hash
/// need not authenticate against it.
fn observation(height: i32, hash: u8, confirmed_at: i64) -> Result<MergeMiningEventPayload> {
    let parsed = parse_auxpow_fixture("500000-valid-parent")?;
    let mut payload = build_event_payload(
        &parsed,
        Some(height),
        ResolvedPoolAttributions::default(),
        ClassificationProof::default(),
        confirmed_at,
    )?;
    payload.child_block_hash = Some(vec![hash; 32]);
    payload.child_header_bytes = None;
    payload.child_nbits = None;
    payload.pow_validates_child_target = None;
    Ok(payload)
}

/// `(revoked_at, revocation_reason, child_displaced_at, child_displaced_by)`.
type EventState = (Option<i64>, Option<String>, Option<i64>, Option<Vec<u8>>);

async fn event_state(client: &Client, id: i64) -> Result<EventState> {
    let row = client
        .query_one(
            "SELECT revoked_at, revocation_reason, child_displaced_at, child_displaced_by \
             FROM merge_mining_event WHERE id = $1",
            &[&id],
        )
        .await?;
    Ok((row.get(0), row.get(1), row.get(2), row.get(3)))
}

/// The event ids of the pre-`0024` state the repair is checked against.
struct RepairScenario {
    /// A twice-replaced height: `a` replaced by `b`, then `b` by `c`.
    a: i64,
    b: i64,
    c: i64,
    /// A voided block nothing replaced.
    d: i64,
    /// An evidence (`hathor_non_btc`) revocation.
    e: i64,
    /// A supersession begun but not finished: the marker names `p` as
    /// superseded by `q`'s hash, and `p` was never revoked.
    p: i64,
    q: i64,
    /// A supersession interrupted before its capture committed: the marker
    /// names `interrupted` as superseded by a hash whose only row,
    /// `stale_replacement`, is a revoked one from before.
    interrupted: i64,
    stale_replacement: i64,
    /// Another source's event revoked with a Hathor reason by hand.
    other: i64,
}

/// Seed the rows the old producer would have left behind, on a schema that
/// still carries the `supersede` marker columns.
async fn seed_repair_scenario(client: &Client, source_id: i64) -> Result<RepairScenario> {
    let insert_for = async |source: i64, height: i32, hash: u8, confirmed_at: i64| -> Result<i64> {
        let payload = observation(height, hash, confirmed_at)?;
        Ok(upsert_merge_mining_event(client, source, &payload)
            .await?
            .event_id)
    };
    let insert = async |height: i32, hash: u8, confirmed_at: i64| -> Result<i64> {
        insert_for(source_id, height, hash, confirmed_at).await
    };
    let revoke = async |id: i64, at: i64, reason: &str| -> Result<()> {
        client
            .execute(
                "UPDATE merge_mining_event SET revoked_at = $2, revocation_reason = $3 \
                 WHERE id = $1",
                &[&id, &at, &reason],
            )
            .await?;
        Ok(())
    };

    let a = insert(5_001, 0xa1, 100).await?;
    let b = insert(5_001, 0xb1, 150).await?;
    let c = insert(5_001, 0xc1, 350).await?;
    revoke(a, 200, "hathor_superseded").await?;
    revoke(b, 400, "hathor_superseded").await?;
    let d = insert(5_002, 0xd2, 100).await?;
    revoke(d, 300, "hathor_voided").await?;
    let e = insert(5_003, 0xe3, 100).await?;
    revoke(e, 300, "hathor_non_btc").await?;
    let p = insert(5_004, 0xf4, 100).await?;
    let q = insert(5_004, 0xa4, 200).await?;
    let namecoin = get_source_id(client, NAMECOIN_SOURCE_CODE).await?;
    let other = insert_for(namecoin, 5_005, 0xb5, 100).await?;
    revoke(other, 300, "hathor_voided").await?;
    let interrupted = insert(5_006, 0xd6, 100).await?;
    let stale_replacement = insert(5_006, 0xc6, 200).await?;
    revoke(stale_replacement, 250, "hathor_non_btc").await?;
    client
        .execute(
            "INSERT INTO poll_pending_reconcile \
                 (source_id, height, kind, new_child_block_hash, superseded_event_ids, reason) \
             VALUES ($1, 5006, 'supersede', $2, $3, 'hathor_superseded')",
            &[&source_id, &vec![0xc6u8; 32], &vec![interrupted]],
        )
        .await?;
    client
        .execute(
            "INSERT INTO poll_pending_reconcile \
                 (source_id, height, kind, new_child_block_hash, superseded_event_ids, reason) \
             VALUES ($1, 5004, 'supersede', $2, $3, 'hathor_superseded')",
            &[&source_id, &vec![0xa4u8; 32], &vec![p]],
        )
        .await?;
    Ok(RepairScenario {
        a,
        b,
        c,
        d,
        e,
        p,
        q,
        interrupted,
        stale_replacement,
        other,
    })
}

#[tokio::test]
async fn migration_0024_restores_replaced_hathor_events_as_displaced() -> Result<()> {
    let (client, schema) =
        crate::support::db::new_test_db_through("0023_validate_child_displacement").await?;
    let result = async {
        let source_id = get_source_id(&client, HATHOR_SOURCE_CODE).await?;
        let rows = seed_repair_scenario(&client, source_id).await?;

        client
            .batch_execute(include_str!(
                "../../../../migrations/0024_restore_hathor_displaced_events.sql"
            ))
            .await?;
        client
            .batch_execute(include_str!(
                "../../../../migrations/0025_retire_pending_supersede.sql"
            ))
            .await?;

        assert_eq!(
            event_state(&client, rows.a).await?,
            (None, None, Some(200), Some(vec![0xb1; 32])),
            "A is restored, displaced by B when B was written"
        );
        assert_eq!(
            event_state(&client, rows.b).await?,
            (None, None, Some(400), Some(vec![0xc1; 32])),
            "B is restored, displaced by C"
        );
        assert_eq!(
            event_state(&client, rows.c).await?,
            (None, None, None, None)
        );
        assert_eq!(
            event_state(&client, rows.d).await?,
            (None, None, None, None),
            "a voided block nothing replaced is restored without a displacement"
        );
        assert_eq!(
            event_state(&client, rows.e).await?,
            (Some(300), Some("hathor_non_btc".to_owned()), None, None),
            "an evidence revocation is untouched"
        );
        assert_eq!(
            event_state(&client, rows.p).await?,
            (None, None, Some(200), Some(vec![0xa4; 32])),
            "an unfinished supersession is completed as displacement"
        );
        assert_eq!(
            event_state(&client, rows.q).await?,
            (None, None, None, None)
        );
        assert_eq!(
            event_state(&client, rows.interrupted).await?,
            (None, None, None, None),
            "a marker whose replacement never became active completes nothing"
        );
        assert_eq!(
            event_state(&client, rows.stale_replacement).await?,
            (Some(250), Some("hathor_non_btc".to_owned()), None, None)
        );
        assert_eq!(
            event_state(&client, rows.other).await?,
            (Some(300), Some("hathor_voided".to_owned()), None, None),
            "another source's event is not touched"
        );

        let pending: i64 = client
            .query_one("SELECT count(*) FROM poll_pending_reconcile", &[])
            .await?
            .get(0);
        assert_eq!(pending, 0, "the marker is gone");
        let marker_columns: i64 = client
            .query_one(
                "SELECT count(*) FROM information_schema.columns \
                 WHERE table_schema = current_schema() \
                   AND table_name = 'poll_pending_reconcile' \
                   AND column_name IN ('kind', 'new_child_block_hash', 'superseded_event_ids')",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(marker_columns, 0, "the marker columns are gone");
        Ok(())
    }
    .await;
    crate::support::db::teardown_test_db(&client, &schema, result).await
}
