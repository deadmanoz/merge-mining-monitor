use std::str::FromStr;

use anyhow::{Context, Result};
use bitcoin::block::{Header, Version};
use bitcoin::consensus::serialize;
use bitcoin::hashes::{Hash as _, sha256d};
use bitcoin::{BlockHash, CompactTarget};
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification};
use mmm_capture::auxpow::{parse_bip34_height, parse_elastos_auxpow};
use mmm_capture::capture::CHILD_PAYOUT_REGISTRY_SOURCE;
use mmm_capture::nbits_table::daa_epoch_start;
use mmm_capture::source_registry::ELASTOS_SOURCE_CODE;
use mmm_producers::chains::elastos::{
    ELASTOS_MINERINFO_NAMESPACE, ELASTOS_REWARD_ADDRESS_NAMESPACE, ELASTOS_RPC_MINERINFO_SOURCE,
    ELASTOS_RPC_REWARD_ADDRESS_SOURCE, ElastosBlock, ElastosCaptureContext, ElastosHeightOutcome,
    ElastosRpc, process_elastos_height,
};
use mmm_producers::{ReclassifyPoolsConfig, run_reclassify_pools};
use mmm_store::{get_source_id, sync_event_observation_attribution};
use tokio_postgres::Client;

use crate::support::db::{
    advisory_locks_held, displacement_at, observation_prev_hash, observations_at,
};

use crate::support::default_pool_snapshot;
use crate::support::seed::{
    EventSeed, hash_bytes, insert_event, insert_pool_identity, pool_id_for_slug,
};

const BINANCE_MINERINFO: &str = "binance";
const F2POOL_REWARD_ADDRESS: &str = "EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR";

#[derive(Clone)]
struct FixtureElastosRpc {
    block: ElastosBlock,
}

impl ElastosRpc for FixtureElastosRpc {
    async fn get_block_by_height(&self, height: i32) -> Result<ElastosBlock> {
        assert_eq!(height, self.block.height);
        Ok(self.block.clone())
    }
}

async fn capture_unknown_elastos_block(
    client: &mut Client,
    block: ElastosBlock,
) -> Result<(i64, i64)> {
    let context =
        ElastosCaptureContext::new_with_classifier(&*client, unknown_classifier()).await?;
    let source_id = context.source_id();
    let height = block.height;
    let rpc = FixtureElastosRpc { block };
    let outcome = process_elastos_height(client, &rpc, &context, height).await?;
    assert_eq!(outcome, ElastosHeightOutcome::AuxpowWritten);
    let event_id = event_id_at_height(client, source_id, height).await?;
    Ok((source_id, event_id))
}

#[tokio::test]
async fn core_cache_match_writes_the_event_end_to_end() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // Real stuck block: Elastos child 2,243,660, parent BIP34 955,585.
        let block: ElastosBlock = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-2243660.json"
        )))
        .expect("deserialize Elastos 2243660 fixture");
        let height = block.height;
        let context = ElastosCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        let rpc = FixtureElastosRpc { block };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::AuxpowWritten);
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
            "a Core-cache-valid parent must write one active event"
        );
        Ok(())
    })
}

#[tokio::test]
async fn a_parent_missing_bitcoins_target_is_captured_as_a_near() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // Elastos 1,500,000: the parent meets only Elastos's easier target.
        let block: ElastosBlock = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-1500000.json"
        )))?;
        let height = block.height;
        let hash = block.reconstruct()?.block_hash.to_byte_array().to_vec();
        let prev = BlockHash::from_str(&block.previousblockhash)?
            .to_byte_array()
            .to_vec();
        let bits = block.bits;
        let context =
            ElastosCaptureContext::new_with_classifier(&client, unknown_classifier()).await?;
        let rpc = FixtureElastosRpc { block };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::AuxpowWritten);
        assert_eq!(
            observations_at(&client, context.source_id(), height).await?,
            vec![(hash.clone(), "near".to_owned(), true, true)]
        );
        assert_eq!(
            observation_prev_hash(&client, context.source_id(), &hash).await?,
            Some(prev)
        );
        let nbits: Option<i64> = client
            .query_one(
                "SELECT child_nbits FROM child_block_observation \
                 WHERE source_id = $1 AND child_block_hash = $2",
                &[&context.source_id(), &hash],
            )
            .await?
            .get(0);
        assert_eq!(nbits, Some(i64::from(bits)));
        Ok(())
    })
}

#[tokio::test]
async fn a_foreign_parent_retracts_only_its_own_blocks_event() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = writable_elastos_block_without_identity();
        let height = block.height;
        let context = ElastosCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        let rpc = FixtureElastosRpc {
            block: block.clone(),
        };
        assert_eq!(
            process_elastos_height(&mut client, &rpc, &context, height).await?,
            ElastosHeightOutcome::AuxpowWritten
        );
        seed_hashless_copy_of_block(&client, context.source_id(), height).await?;
        let sibling_hash = hash_bytes(0x5a01);
        insert_event(
            &client,
            EventSeed {
                source_id: context.source_id(),
                child_height: height,
                child_hash: sibling_hash.clone(),
                parent_hash: hash_bytes(0x5b01),
                prev_hash: hash_bytes(0x5b00),
                parent_time: 1_800_000_000,
                kind: "near",
                pow_validates_btc_target: false,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;

        let reconstructed = block.reconstruct()?;
        let auxpow = reconstructed
            .auxpow
            .as_deref()
            .context("Elastos fixture must carry AuxPoW")?;
        let parsed = parse_elastos_auxpow(reconstructed.prefix_header, auxpow)?;
        let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
            .context("Elastos fixture must carry a BIP34 parent height")?;
        let mismatching_bits = parsed.parent_header.bits().to_consensus() ^ 1;
        client
            .execute(
                "UPDATE bitcoin_core_header SET bits = $1 WHERE height = $2",
                &[
                    &i64::from(mismatching_bits),
                    &daa_epoch_start(parent_height),
                ],
            )
            .await?;

        assert_eq!(
            process_elastos_height(&mut client, &rpc, &context, height).await?,
            ElastosHeightOutcome::NonBitcoinParent
        );
        let block_hash = block.reconstruct()?.block_hash.to_byte_array().to_vec();
        let mut expected = vec![
            (block_hash, "non_bitcoin_parent".to_owned(), false, false),
            (sibling_hash.clone(), "near".to_owned(), true, true),
        ];
        expected.sort();
        assert_eq!(
            observations_at(&client, context.source_id(), height).await?,
            expected,
            "the refused block keeps only its child-side observation; the sibling is untouched"
        );
        let remaining: Vec<Vec<u8>> = client
            .query(
                "SELECT child_block_hash FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2",
                &[&context.source_id(), &height],
            )
            .await?
            .iter()
            .map(|row| row.get(0))
            .collect();
        assert_eq!(
            remaining,
            std::slice::from_ref(&sibling_hash),
            "a foreign parent retracts this block's event and no other"
        );
        let sibling = client
            .query_one(
                "SELECT revoked_at, child_displaced_by FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2 AND child_block_hash = $3",
                &[&context.source_id(), &height, &sibling_hash],
            )
            .await?;
        assert_eq!(sibling.get::<_, Option<i64>>(0), None);
        assert_eq!(
            sibling.get::<_, Option<Vec<u8>>>(1),
            Some(reconstructed.block_hash.to_byte_array().to_vec()),
            "the sibling is displaced by the block the chain carries, not removed"
        );
        Ok(())
    })
}

fn unknown_genesis_parent() -> ParentClassification {
    ParentClassification::unknown(
        &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
    )
}

#[tokio::test]
async fn live_capture_writes_unmapped_elastos_reward_and_minerinfo() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = writable_binance_elastos_block();
        let (_, event_id) = capture_unknown_elastos_block(&mut client, block).await?;

        assert_eq!(
            elastos_identity_rows(&client, event_id).await?,
            [
                (
                    ELASTOS_MINERINFO_NAMESPACE.to_owned(),
                    ELASTOS_RPC_MINERINFO_SOURCE.to_owned(),
                    BINANCE_MINERINFO.to_owned(),
                    None,
                    None,
                ),
                (
                    ELASTOS_REWARD_ADDRESS_NAMESPACE.to_owned(),
                    ELASTOS_RPC_REWARD_ADDRESS_SOURCE.to_owned(),
                    F2POOL_REWARD_ADDRESS.to_owned(),
                    None,
                    None,
                ),
            ]
        );

        // An identity the event no longer carries leaves its observation on the next sync.
        let identities = "SELECT cardinality(o.child_identity_ids) FROM child_block_observation o \
                          JOIN merge_mining_event e ON e.observation_id = o.id WHERE e.id = $1";
        assert_eq!(
            client
                .query_one(identities, &[&event_id])
                .await?
                .get::<_, i32>(0),
            2
        );
        client
            .execute(
                "DELETE FROM event_pool_attribution WHERE event_id = $1 AND namespace = $2",
                &[&event_id, &ELASTOS_MINERINFO_NAMESPACE],
            )
            .await?;
        sync_event_observation_attribution(&client, event_id).await?;
        assert_eq!(
            client
                .query_one(identities, &[&event_id])
                .await?
                .get::<_, i32>(0),
            1
        );
        Ok(())
    })
}

#[tokio::test]
async fn live_capture_maps_known_reward_and_minerinfo_identities() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;
        let reward_identity_id = insert_pool_identity(
            &client,
            f2pool_id,
            ELASTOS_REWARD_ADDRESS_NAMESPACE,
            F2POOL_REWARD_ADDRESS,
        )
        .await?;
        let minerinfo_identity_id = insert_pool_identity(
            &client,
            f2pool_id,
            ELASTOS_MINERINFO_NAMESPACE,
            BINANCE_MINERINFO,
        )
        .await?;

        let block = writable_binance_elastos_block();
        let (_, event_id) = capture_unknown_elastos_block(&mut client, block).await?;

        assert_eq!(
            elastos_identity_rows(&client, event_id).await?,
            [
                (
                    ELASTOS_MINERINFO_NAMESPACE.to_owned(),
                    CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                    BINANCE_MINERINFO.to_owned(),
                    Some(f2pool_id),
                    Some(minerinfo_identity_id),
                ),
                (
                    ELASTOS_REWARD_ADDRESS_NAMESPACE.to_owned(),
                    CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
                    F2POOL_REWARD_ADDRESS.to_owned(),
                    Some(f2pool_id),
                    Some(reward_identity_id),
                ),
            ]
        );

        Ok(())
    })
}

#[tokio::test]
async fn capture_resolves_embedded_minerinfo_registry_end_to_end() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = writable_elastos_block_with_identity(Some("🐟"));
        let (_, event_id) = capture_unknown_elastos_block(&mut client, block).await?;

        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;
        let viabtc_id = pool_id_for_slug(&client, "viabtc").await?;
        let (fish_pool_id, fish_identity_id) = elastos_minerinfo_identity(&client, "🐟").await?;
        assert_eq!(fish_pool_id, f2pool_id);
        let (viabtc_pool_id, _) = elastos_minerinfo_identity(&client, "Mined by ViaBTC").await?;
        assert_eq!(viabtc_pool_id, viabtc_id);

        assert!(elastos_identity_rows(&client, event_id).await?.contains(&(
            ELASTOS_MINERINFO_NAMESPACE.to_owned(),
            CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
            "🐟".to_owned(),
            Some(f2pool_id),
            Some(fish_identity_id),
        )));

        Ok(())
    })
}

#[tokio::test]
async fn reclassify_pools_reresolves_existing_elastos_minerinfo_without_rpc() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;
        let source_id = get_source_id(&client, ELASTOS_SOURCE_CODE).await?;

        let event_id = insert_event(
            &client,
            EventSeed {
                source_id,
                child_height: 360_062,
                child_hash: hash_bytes(0x51),
                parent_hash: hash_bytes(0x52),
                prev_hash: hash_bytes(0x53),
                parent_time: 1_700_000_000,
                kind: "unknown",
                pow_validates_btc_target: true,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;
        client
            .execute(
                "INSERT INTO event_pool_attribution ( \
                    event_id, side, namespace, match_kind, matched_value, pool_id, \
                    source, confidence, details, first_seen_at, last_seen_at \
                 ) VALUES ($1, 'child_block', $2, 'minerinfo', '🐟', NULL, $3, 'medium', \
                    '{}'::jsonb, $4, $4)",
                &[
                    &event_id,
                    &ELASTOS_MINERINFO_NAMESPACE,
                    &ELASTOS_RPC_MINERINFO_SOURCE,
                    &1_700_000_000_i64,
                ],
            )
            .await?;

        let stats = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert!(
            stats.elastos_identity_updates >= 1,
            "reclassify-pools should promote the matchable Elastos minerinfo row"
        );

        let row = client
            .query_one(
                "SELECT a.pool_id, a.pool_identity_id, a.source, e.child_miner_pool_id \
                 FROM event_pool_attribution a JOIN merge_mining_event e ON e.id = a.event_id \
                 WHERE a.event_id = $1 AND a.namespace = $2 AND a.matched_value = '🐟'",
                &[&event_id, &ELASTOS_MINERINFO_NAMESPACE],
            )
            .await?;
        assert_eq!(row.get::<_, Option<i64>>(0), Some(f2pool_id));
        assert!(row.get::<_, Option<i64>>(1).is_some());
        assert_eq!(row.get::<_, String>(2), CHILD_PAYOUT_REGISTRY_SOURCE);
        assert_eq!(row.get::<_, Option<i64>>(3), Some(f2pool_id));

        let again = run_reclassify_pools(&mut client, ReclassifyPoolsConfig::default()).await?;
        assert_eq!(again.elastos_identity_updates, 0);

        Ok(())
    })
}

async fn elastos_minerinfo_identity(client: &Client, minerinfo: &str) -> Result<(i64, i64)> {
    let row = client
        .query_one(
            "SELECT pool_id, id FROM pool_identity WHERE namespace = $1 AND identifier = $2",
            &[&ELASTOS_MINERINFO_NAMESPACE, &minerinfo],
        )
        .await?;
    Ok((row.get(0), row.get(1)))
}

fn unknown_classifier() -> ConfiguredParentClassifier {
    ConfiguredParentClassifier::Fake(FakeParentClassifier::new(ParentClassification::unknown(
        &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
    )))
}

fn writable_elastos_block_without_identity() -> ElastosBlock {
    serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/elastos/ela-360062.json"
    )))
    .expect("deserialize Elastos 360062 fixture")
}

fn writable_elastos_block_with_identity(minerinfo: Option<&str>) -> ElastosBlock {
    let mut block = writable_elastos_block_without_identity();
    let identity_block: ElastosBlock = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/elastos/ela-2232276.json"
    )))
    .expect("deserialize Elastos identity fixture");
    block.tx = identity_block.tx;
    block.minerinfo = minerinfo.map(str::to_owned);
    if let Some(first_tx) = block.tx.first_mut() {
        first_tx
            .payload
            .get_or_insert_with(Default::default)
            .coinbasedata = minerinfo.map(str::to_owned);
    }
    block
}

fn writable_binance_elastos_block() -> ElastosBlock {
    writable_elastos_block_with_identity(Some(BINANCE_MINERINFO))
}

/// A hashless historical row for the block captured at `height`, with its observation.
async fn seed_hashless_copy_of_block(client: &Client, source_id: i64, height: i32) -> Result<()> {
    client
        .execute(
            "WITH hashless AS ( \
                INSERT INTO merge_mining_event ( \
                    source_id, child_height, child_block_time, btc_parent_header_hash, \
                    btc_parent_prev_header_hash, btc_parent_header_bytes, btc_parent_header_time, \
                    btc_parent_height, btc_parent_kind, pow_validates_btc_target, \
                    discovered_at, confirmed_at \
                ) \
                SELECT source_id, child_height, child_block_time, btc_parent_header_hash, \
                       btc_parent_prev_header_hash, btc_parent_header_bytes, btc_parent_header_time, \
                       btc_parent_height, btc_parent_kind, pow_validates_btc_target, \
                       discovered_at, confirmed_at \
                FROM merge_mining_event \
                WHERE source_id = $1 AND child_height = $2 AND child_block_hash IS NOT NULL \
                RETURNING source_id, child_height, btc_parent_header_hash \
             ) \
             INSERT INTO child_block_observation ( \
                source_id, child_height, observation_kind, btc_parent_header_hash, recorded_at \
             ) \
             SELECT source_id, child_height, 'evidence', btc_parent_header_hash, 1 FROM hashless",
            &[&source_id, &height],
        )
        .await
        .context("seed a hashless copy of the captured block")?;
    Ok(())
}

async fn event_id_at_height(client: &Client, source_id: i64, height: i32) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT id FROM merge_mining_event WHERE source_id = $1 AND child_height = $2",
            &[&source_id, &height],
        )
        .await?
        .get(0))
}

async fn elastos_identity_rows(
    client: &Client,
    event_id: i64,
) -> Result<Vec<(String, String, String, Option<i64>, Option<i64>)>> {
    let rows = client
        .query(
            "SELECT namespace, source, matched_value, pool_id, pool_identity_id \
             FROM event_pool_attribution \
             WHERE event_id = $1 \
               AND side = 'child_block' \
               AND namespace IN ('elastos_reward_address', 'elastos_minerinfo') \
             ORDER BY namespace, matched_value",
            &[&event_id],
        )
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4)))
        .collect())
}

/// Recompute the RPC-reported hash (`sha256d(80-byte prefix || height LE)`).
fn rehash(block: &mut ElastosBlock) -> Result<()> {
    let prefix = Header {
        version: Version::from_consensus(block.version),
        prev_blockhash: block.previousblockhash.parse()?,
        merkle_root: block.merkleroot.parse()?,
        time: block.time,
        bits: CompactTarget::from_consensus(block.bits),
        nonce: block.nonce,
    };
    let mut bytes = serialize(&prefix);
    bytes.extend_from_slice(&block.height.to_le_bytes());
    block.hash =
        BlockHash::from_byte_array(sha256d::Hash::hash(&bytes).to_byte_array()).to_string();
    Ok(())
}

#[tokio::test]
async fn an_unproven_block_at_a_rescanned_height_leaves_the_record_alone() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block_a = writable_elastos_block_without_identity();
        let height = block_a.height;
        let context = ElastosCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        let source_id = context.source_id();
        let hash_a = block_a.reconstruct()?.block_hash.to_byte_array().to_vec();

        let mut block_n = block_a.clone();
        block_n.nonce = block_n.nonce.wrapping_add(1);
        block_n.auxpow = None;
        rehash(&mut block_n)?;
        let hash_n = block_n.reconstruct()?.block_hash.to_byte_array().to_vec();

        let rpc = FixtureElastosRpc {
            block: block_a.clone(),
        };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::AuxpowWritten);
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            vec![(hash_a.clone(), None, None)]
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);

        // Unproven non-AuxPoW block N does not displace A as the chain's block.
        let rpc = FixtureElastosRpc { block: block_n };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::NonAuxpowSkipped);
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            vec![(hash_a.clone(), None, None)]
        );
        assert_ne!(hash_a, hash_n);
        // Block M carries A's AuxPoW, which commits to A, not M: M is kept with an
        // unverified parent and does not displace A either.
        let mut block_m = block_a.clone();
        block_m.nonce = block_m.nonce.wrapping_add(2);
        rehash(&mut block_m)?;
        let hash_m = block_m.reconstruct()?.block_hash.to_byte_array().to_vec();
        let rpc = FixtureElastosRpc { block: block_m };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::MalformedSkipped);
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            vec![(hash_a.clone(), None, None)]
        );
        let mut expected = vec![
            (hash_a.clone(), "evidence".to_owned(), false, true),
            (hash_n.clone(), "not_merge_mined".to_owned(), false, false),
            (hash_m, "unverified_parent".to_owned(), false, false),
        ];
        expected.sort();
        assert_eq!(observations_at(&client, source_id, height).await?, expected);
        assert_eq!(advisory_locks_held(&client).await?, 0);

        // An answer that drops A's AuxPoW (the 84-byte hash does not commit to it) leaves
        // A's proven evidence in place.
        let mut stripped = block_a.clone();
        stripped.auxpow = None;
        let rpc = FixtureElastosRpc { block: stripped };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::NonAuxpowSkipped);
        assert_eq!(observations_at(&client, source_id, height).await?, expected);

        let rpc = FixtureElastosRpc { block: block_a };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::AuxpowWritten);
        assert_eq!(
            displacement_at(&client, source_id, height).await?,
            vec![(hash_a, None, None)]
        );
        Ok(())
    })
}
