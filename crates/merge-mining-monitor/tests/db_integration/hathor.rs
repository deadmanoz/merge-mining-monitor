use std::str::FromStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Result;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification};
use mmm_capture::nbits_table::daa_epoch_start;
use mmm_producers::RescanOutcome;
use mmm_producers::chains::hathor::{
    ChainObservation, HathorBlockMeta, HathorCaptureContext, HathorHeightOutcome, HathorRpc,
    HathorTransaction, forge_with_weight, process_hathor_height, reconstruct_from_blobs,
    rescan_hathor_height,
};
use mmm_store::upsert_merge_mining_event;
use tokio_postgres::Client;

use crate::support::db::{advisory_locks_held, displacement_at};
use crate::support::exact_observation;

/// A `HathorRpc` that always returns one fixed block + transaction, so a committed
/// Hathor block fixture can drive `process_hathor_height` end to end.
struct FixtureHathorRpc {
    meta: HathorBlockMeta,
    tx: HathorTransaction,
    /// `/transaction` fetches served: the call a rescan of an unchanged
    /// height must not make.
    tx_calls: AtomicUsize,
}

impl HathorRpc for FixtureHathorRpc {
    async fn get_block_at_height(&self, _height: i32) -> Result<Option<HathorBlockMeta>> {
        Ok(Some(self.meta.clone()))
    }

    async fn get_transaction(&self, _tx_id: &str) -> Result<Option<HathorTransaction>> {
        self.tx_calls.fetch_add(1, Ordering::SeqCst);
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

/// A fake Core classifier whose fresh mainnet tip is `tip`.
fn fake_classifier_synced_to(tip: i32) -> ConfiguredParentClassifier {
    ConfiguredParentClassifier::Fake(
        FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(tip),
    )
}

/// A live context over the default 20-block rescan window.
async fn live_context(
    client: &Client,
    classifier: ConfiguredParentClassifier,
) -> Result<HathorCaptureContext> {
    HathorCaptureContext::new_with_classifier(
        client,
        classifier,
        ChainObservation::Live { fork_window: 20 },
    )
    .await
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
    (
        height,
        FixtureHathorRpc {
            meta,
            tx,
            tx_calls: AtomicUsize::new(0),
        },
    )
}

/// The event count at `(source, height)`: a parent the lineage gate refuses
/// leaves none, whatever an earlier capture stored.
async fn hathor_events_at(client: &Client, source_id: i64, height: i32) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT COUNT(*)::int8 FROM merge_mining_event \
             WHERE source_id = $1 AND child_height = $2",
            &[&source_id, &height],
        )
        .await?
        .get(0))
}

/// Make the Core cache disagree with the fixture parent's epoch bits, so the
/// parent at 710,969 no longer looks like a Bitcoin header.
async fn contradict_fixture_parent_epoch(client: &Client) -> Result<()> {
    client
        .execute(
            "UPDATE bitcoin_core_header SET bits = $1 WHERE height = $2",
            &[&i64::from(0x170c_69ea_u32 ^ 1), &daa_epoch_start(710_969)],
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn a_parent_that_turns_out_foreign_retracts_its_event() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();
        let context = live_context(&client, fake_classifier_synced_to(955_609)).await?;
        assert_eq!(
            process_hathor_height(&mut client, &rpc, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );

        contradict_fixture_parent_epoch(&client).await?;
        assert_eq!(
            process_hathor_height(&mut client, &rpc, &context, height).await?,
            HathorHeightOutcome::NonBitcoinParent
        );
        assert_eq!(
            hathor_events_at(&client, context.source_id(), height).await?,
            0
        );
        Ok(())
    })
}

#[tokio::test]
async fn rescan_of_an_unchanged_hathor_height_skips_the_transaction_fetch() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();
        let context = live_context(&client, fake_classifier_synced_to(955_609)).await?;
        let outcome = process_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, HathorHeightOutcome::AuxpowWritten);
        let fetched = rpc.tx_calls.load(Ordering::SeqCst);
        assert!(fetched >= 1);

        // The chain still carries the captured block: the block metadata is
        // enough to know that, so the transaction is not fetched again and
        // the event is left as it is.
        let outcome = rescan_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, RescanOutcome::Unchanged);
        assert_eq!(rpc.tx_calls.load(Ordering::SeqCst), fetched);
        let active: i64 = client
            .query_one(
                "SELECT count(*) FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2 AND revoked_at IS NULL",
                &[&context.source_id(), &height],
            )
            .await?
            .get(0);
        assert_eq!(active, 1);
        assert_eq!(advisory_locks_held(&client).await?, 0);

        // A sidecar added at the height since the record (a historical import,
        // or the cache ingest enriching an imported event) changes the
        // evidence the block's work was held against: the rescan takes the
        // full capture, which re-runs the work-floor check, and records the
        // block again against the evidence now, after which the fast path
        // applies again. An event without a sidecar is not part of that
        // evidence.
        let imported = exact_observation("500001-near-parent", height, [0x5b; 32], 2_030)?;
        let imported_id = upsert_merge_mining_event(&client, context.source_id(), &imported)
            .await?
            .event_id;
        let outcome = rescan_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, RescanOutcome::Unchanged);
        assert_eq!(rpc.tx_calls.load(Ordering::SeqCst), fetched);
        client
            .execute(
                "INSERT INTO hathor_merge_mining_evidence \
                     (event_id, hathor_block_hash, hathor_height, aux_pow, funds_graph, \
                      funds_graph_split, proof_format) \
                 SELECT $1, hathor_block_hash, hathor_height, aux_pow, funds_graph, \
                        funds_graph_split, proof_format \
                   FROM hathor_merge_mining_evidence h \
                   JOIN merge_mining_event e ON e.id = h.event_id \
                  WHERE e.source_id = $2 AND e.child_height = $3 AND e.id <> $1",
                &[&imported_id, &context.source_id(), &height],
            )
            .await?;
        let outcome = rescan_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(
            outcome,
            RescanOutcome::Captured(HathorHeightOutcome::AuxpowWritten)
        );
        let fetched = fetched + 1;
        assert_eq!(rpc.tx_calls.load(Ordering::SeqCst), fetched);
        let outcome = rescan_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, RescanOutcome::Unchanged);
        assert_eq!(rpc.tx_calls.load(Ordering::SeqCst), fetched);

        // A replay that rewrites the sidecar's graph head (the bytes the
        // work floor reads) changes the evidence just as a new sidecar does.
        client
            .execute(
                "UPDATE hathor_merge_mining_evidence SET funds_graph = \
                     overlay(funds_graph PLACING '\\x00'::bytea FROM funds_graph_split + 1) \
                 WHERE event_id = $1",
                &[&imported_id],
            )
            .await?;
        let outcome = rescan_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(
            outcome,
            RescanOutcome::Captured(HathorHeightOutcome::AuxpowWritten)
        );
        let fetched = fetched + 1;
        assert_eq!(rpc.tx_calls.load(Ordering::SeqCst), fetched);
        client
            .execute(
                "DELETE FROM merge_mining_event WHERE child_block_hash = $1",
                &[&[0x5b_u8; 32].as_slice()],
            )
            .await?;

        // A verdict came from the Core header cache. When the cache replaces a
        // boundary that can change verdicts (its generation moves) the head is
        // no longer final: the rescan captures the height again, and with the
        // epoch's nBits changed underneath it the event is retracted.
        contradict_fixture_parent_epoch(&client).await?;
        client
            .execute(
                "UPDATE bitcoin_core_header_cache_state \
                 SET core_cache_generation = core_cache_generation + 1 WHERE singleton",
                &[],
            )
            .await?;
        let outcome = rescan_hathor_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(
            outcome,
            RescanOutcome::Captured(HathorHeightOutcome::NonBitcoinParent)
        );
        assert_eq!(rpc.tx_calls.load(Ordering::SeqCst), fetched + 1);
        assert_eq!(
            hathor_events_at(&client, context.source_id(), height).await?,
            0
        );
        Ok(())
    })
}

#[tokio::test]
async fn live_capture_promotes_a_hashless_historical_row_without_revoking_it() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();
        let context = live_context(&client, fake_classifier_synced_to(955_609)).await?;
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

/// A copy of a fixture RPC with its response edited: what the endpoint would
/// answer if it misplaced, voided or corrupted that block.
fn variant_of(
    rpc: &FixtureHathorRpc,
    edit: impl FnOnce(&mut HathorBlockMeta, &mut HathorTransaction),
) -> FixtureHathorRpc {
    let mut meta = rpc.meta.clone();
    let mut tx = rpc.tx.clone();
    edit(&mut meta, &mut tx);
    FixtureHathorRpc {
        meta,
        tx,
        tx_calls: AtomicUsize::new(0),
    }
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

        let context = live_context(&client, fake_classifier_synced_to(955_609)).await?;
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
        let context = live_context(&client, fake_classifier_synced_to(955_609)).await?;
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
        let forged = variant_of(&rpc_a, |meta, tx| {
            meta.tx_id = forged_hash.to_string();
            tx.raw = hex::encode(&forged_raw);
            tx.hash = forged_hash.to_string();
        });
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
