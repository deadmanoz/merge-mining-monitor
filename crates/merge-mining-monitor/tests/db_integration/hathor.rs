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

use crate::support::db::{
    advisory_locks_held, displacement_at, observation_displacement_at, observation_prev_hash,
    observations_at,
};
use crate::support::exact_observation;

struct FixtureHathorRpc {
    meta: HathorBlockMeta,
    tx: HathorTransaction,
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

fn unknown_genesis_parent() -> ParentClassification {
    ParentClassification::unknown(
        &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
    )
}

fn fake_classifier_synced_to(tip: i32) -> ConfiguredParentClassifier {
    ConfiguredParentClassifier::Fake(
        FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(tip),
    )
}

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

fn hathor_fixture(json: &str) -> (i32, FixtureHathorRpc) {
    let j: serde_json::Value = serde_json::from_str(json).expect("deserialize Hathor fixture");
    let height = j["hathor_height"].as_i64().unwrap() as i32;
    let meta = HathorBlockMeta {
        tx_id: j["tx_id"].as_str().unwrap().to_owned(),
        version: j["version"].as_i64().unwrap() as i32,
        height,
        is_voided: j["is_voided"].as_bool().unwrap_or(false),
        timestamp: j["timestamp"].as_i64(),
        weight: j["weight"].as_f64(),
        parents: Vec::new(),
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
        // A hashless observation of the block beside the exact one carries an nBits the
        // live payload lacks: promotion folds it into the exact row.
        client
            .execute(
                "INSERT INTO child_block_observation \
                     (source_id, child_height, observation_kind, btc_parent_header_hash, \
                      child_nbits, recorded_at) \
                 SELECT source_id, child_height, observation_kind, btc_parent_header_hash, \
                        486604799, recorded_at \
                   FROM child_block_observation \
                  WHERE source_id = $1 AND child_height = $2",
                &[&context.source_id(), &height],
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
        let folded = client
            .query_one(
                "SELECT count(*), max(child_nbits), bool_and(child_block_hash IS NOT NULL) \
                 FROM child_block_observation WHERE source_id = $1 AND child_height = $2",
                &[&context.source_id(), &height],
            )
            .await?;
        assert_eq!(
            folded.get::<_, i64>(0),
            1,
            "the hashless observation is folded away"
        );
        assert_eq!(folded.get::<_, Option<i64>>(1), Some(486_604_799));
        assert!(folded.get::<_, bool>(2));
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

fn internal_hash(tx_id: &str) -> Vec<u8> {
    bitcoin::BlockHash::from_str(tx_id)
        .unwrap()
        .to_byte_array()
        .to_vec()
}

type DisplacementRow = (Vec<u8>, Option<Vec<u8>>, Option<i64>);

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

        assert_eq!(
            process_hathor_height(&mut client, &rpc_a, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            vec![(hash_a.clone(), None, None)]
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);

        assert_eq!(
            process_hathor_height(&mut client, &rpc_b, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let after_b = sorted(vec![
            (hash_a.clone(), Some(hash_b.clone()), None),
            (hash_b.clone(), None, None),
        ]);
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

        let misrouted = variant_of(&rpc_b, |meta, _| meta.height = 2_773_476);
        assert_eq!(
            process_hathor_height(&mut client, &misrouted, &context, height).await?,
            HathorHeightOutcome::TransientHold
        );
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

        let tampered = variant_of(&rpc_b, |_, tx| {
            let last = tx.raw.pop().unwrap();
            tx.raw.push(if last == '0' { '1' } else { '0' });
        });
        assert_eq!(
            process_hathor_height(&mut client, &tampered, &context, height).await?,
            HathorHeightOutcome::MalformedSkipped
        );
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

        let voided = variant_of(&rpc_b, |meta, _| meta.is_voided = true);
        assert_eq!(
            process_hathor_height(&mut client, &voided, &context, height).await?,
            HathorHeightOutcome::VoidedSkipped
        );
        assert_eq!(displacement_at(&client, source_id, height).await?, after_b);

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

/// The fixture block reweighted to a trivial `weight`: a self-consistent block
/// whose Bitcoin parent misses Bitcoin's target.
fn trivial_work_near(rpc: &FixtureHathorRpc, weight: f64) -> Result<FixtureHathorRpc> {
    let raw = hex::decode(&rpc.tx.raw)?;
    let aux_pow = hex::decode(rpc.tx.aux_pow.as_deref().unwrap())?;
    let recon = reconstruct_from_blobs(
        &raw,
        &aux_pow,
        bitcoin::BlockHash::from_str(&rpc.meta.tx_id)?,
    )?;
    let (forged_raw, forged_hash) =
        forge_with_weight(&raw, &aux_pow, recon.funds_graph_split, weight)?;
    Ok(variant_of(rpc, |meta, tx| {
        meta.tx_id = forged_hash.to_string();
        tx.raw = hex::encode(&forged_raw);
        tx.hash = forged_hash.to_string();
    }))
}

async fn hathor_sidecars_for(client: &Client, child_block_hash: &[u8]) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM hathor_merge_mining_evidence WHERE hathor_block_hash = $1",
            &[&child_block_hash],
        )
        .await?
        .get(0))
}

fn displaced_by(rows: &[(Vec<u8>, Option<Vec<u8>>)], hash: &[u8]) -> Option<Vec<u8>> {
    rows.iter()
        .find(|(row_hash, _)| row_hash.as_slice() == hash)
        .and_then(|(_, by)| by.clone())
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

        let forged = trivial_work_near(&rpc_a, 1e-6)?;
        let near_hash = internal_hash(&forged.meta.tx_id);
        assert_eq!(
            process_hathor_height(&mut client, &forged, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let mut undisplaced = vec![(hash_a.clone(), None), (near_hash.clone(), None)];
        undisplaced.sort();
        assert_eq!(
            observation_displacement_at(&client, source_id, height).await?,
            undisplaced,
            "A stays the chain's block, undisplaced, and the near is recorded beside it"
        );
        assert!(
            displacement_at(&client, source_id, height)
                .await?
                .contains(&(hash_a.clone(), None, None))
        );

        // An event with no sidecar to hold work against (an imported row) keeps
        // the REST answer from taking the height too.
        client
            .execute(
                "DELETE FROM hathor_merge_mining_evidence WHERE hathor_block_hash = $1",
                &[&hash_a],
            )
            .await?;
        let another = trivial_work_near(&rpc_a, 2e-6)?;
        assert_eq!(
            process_hathor_height(&mut client, &another, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let rows = observation_displacement_at(&client, source_id, height).await?;
        assert_eq!(displaced_by(&rows, &hash_a), None);
        assert_eq!(displaced_by(&rows, &near_hash), None);
        assert_eq!(advisory_locks_held(&client).await?, 0);
        Ok(())
    })
}

#[tokio::test]
async fn a_height_without_an_event_follows_hathor_across_nears_and_unmerged_blocks() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();
        let context = live_context(&client, fake_classifier_synced_to(955_609)).await?;
        let source_id = context.source_id();

        let first = trivial_work_near(&rpc, 1e-6)?;
        let first_hash = internal_hash(&first.meta.tx_id);
        assert_eq!(
            process_hathor_height(&mut client, &first, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        assert_eq!(
            observations_at(&client, source_id, height).await?,
            vec![(first_hash.clone(), "near".to_owned(), true, true)]
        );
        assert_eq!(hathor_sidecars_for(&client, &first_hash).await?, 0);
        // The parent block Hathor's /block_at_height lists first for 1,971,823.
        assert_eq!(
            observation_prev_hash(&client, source_id, &first_hash).await?,
            Some(internal_hash(
                "00000000000000000e1b2a0daba36e5fd7a29573412005f52fcd950c227fd981"
            ))
        );

        let second = trivial_work_near(&rpc, 2e-6)?;
        let second_hash = internal_hash(&second.meta.tx_id);
        assert_eq!(
            process_hathor_height(&mut client, &second, &context, height).await?,
            HathorHeightOutcome::AuxpowWritten
        );
        let rows = observation_displacement_at(&client, source_id, height).await?;
        assert_eq!(displaced_by(&rows, &first_hash), Some(second_hash.clone()));
        assert_eq!(displaced_by(&rows, &second_hash), None);

        let parent_display = rpc.meta.tx_id.clone();
        let unmerged = variant_of(&rpc, |meta, _| {
            meta.version = 0;
            meta.tx_id = "00000000000000000000000000000000000000000000000000000000000000aa".into();
            meta.parents = vec![parent_display.clone()];
        });
        let unmerged_hash = internal_hash(&unmerged.meta.tx_id);
        assert_eq!(
            process_hathor_height(&mut client, &unmerged, &context, height).await?,
            HathorHeightOutcome::NonAuxpowSkipped
        );
        assert_eq!(unmerged.tx_calls.load(Ordering::SeqCst), 0);
        let rows = observation_displacement_at(&client, source_id, height).await?;
        assert_eq!(
            displaced_by(&rows, &second_hash),
            Some(unmerged_hash.clone())
        );
        assert_eq!(displaced_by(&rows, &unmerged_hash), None);
        let time: Option<i64> = client
            .query_one(
                "SELECT child_block_time FROM child_block_observation \
                 WHERE source_id = $1 AND child_block_hash = $2 \
                   AND observation_kind = 'not_merge_mined'",
                &[&source_id, &unmerged_hash],
            )
            .await?
            .get(0);
        assert_eq!(time, rpc.meta.timestamp);
        assert_eq!(
            observation_prev_hash(&client, source_id, &unmerged_hash).await?,
            Some(internal_hash(&rpc.meta.tx_id))
        );

        assert_eq!(
            rescan_hathor_height(&mut client, &unmerged, &context, height).await?,
            RescanOutcome::Unchanged,
            "an unchanged block without AuxPoW skips the full path on rescan"
        );

        assert_eq!(advisory_locks_held(&client).await?, 0);
        Ok(())
    })
}

#[tokio::test]
async fn a_malformed_hathor_answer_is_kept_once_its_weight_checks_out() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (height, rpc) = hathor_1971823_fixture();
        let context = live_context(&client, fake_classifier_synced_to(955_609)).await?;
        let source_id = context.source_id();

        let misses_its_weight = variant_of(&rpc, |meta, _| {
            meta.version = 0;
            meta.tx_id = "00000000000000000000000000000000000000000000000000000000000000aa".into();
            meta.weight = Some(250.0);
        });
        assert_eq!(
            process_hathor_height(&mut client, &misses_its_weight, &context, height).await?,
            HathorHeightOutcome::MalformedSkipped
        );

        // A version-3 answer whose reconstruction fails keeps the block, parent unverified,
        // once its hash is checked against the weight the answer declares.
        let tampered = variant_of(&rpc, |_, tx| {
            let last = tx.raw.pop().unwrap();
            tx.raw.push(if last == '0' { '1' } else { '0' });
        });
        let weightless = variant_of(&tampered, |meta, _| meta.weight = None);
        assert_eq!(
            process_hathor_height(&mut client, &weightless, &context, height).await?,
            HathorHeightOutcome::MalformedSkipped
        );
        assert_eq!(observations_at(&client, source_id, height).await?, vec![]);
        assert_eq!(
            process_hathor_height(&mut client, &tampered, &context, height).await?,
            HathorHeightOutcome::MalformedSkipped
        );
        assert_eq!(
            observations_at(&client, source_id, height).await?,
            vec![(
                internal_hash(&rpc.meta.tx_id),
                "unverified_parent".to_owned(),
                false,
                false
            )]
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);
        Ok(())
    })
}
