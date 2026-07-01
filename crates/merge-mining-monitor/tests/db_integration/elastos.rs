use anyhow::Result;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification};
use mmm_capture::capture::{CHILD_PAYOUT_REGISTRY_SOURCE, ELASTOS_REVOKE_NON_BTC};
use mmm_capture::source_registry::ELASTOS_SOURCE_CODE;
use mmm_producers::chains::elastos::{
    ELASTOS_MINERINFO_NAMESPACE, ELASTOS_REWARD_ADDRESS_NAMESPACE, ELASTOS_RPC_MINERINFO_SOURCE,
    ELASTOS_RPC_REWARD_ADDRESS_SOURCE, ElastosBlock, ElastosCaptureContext, ElastosHeightOutcome,
    ElastosRpc, process_elastos_height, process_elastos_table_horizon_for_test,
};
use mmm_producers::{ReclassifyPoolsConfig, run_reclassify_pools};
use mmm_store::get_source_id;
use tokio_postgres::Client;

use crate::support::default_pool_snapshot;
use crate::support::seed::{EventSeed, hash_bytes, insert_event, pool_id_for_slug};

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
async fn far_future_table_horizon_parent_revokes_active_event_and_advances() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = writable_elastos_block_without_identity();
        let height = block.height;
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(953_305),
        );
        let context = ElastosCaptureContext::new_with_classifier(&client, classifier).await?;
        let rpc = FixtureElastosRpc { block };

        let write_outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(write_outcome, ElastosHeightOutcome::AuxpowWritten);

        // A claimed BIP34 height 954,814 is far beyond the synced tip 953,305
        // (> tolerance 144), so it is a fabricated far-future height regardless of
        // the parent's nBits: revoke as non-BTC.
        let outcome = process_elastos_table_horizon_for_test(
            &mut client,
            &context,
            height,
            954_814,
            bitcoin::CompactTarget::from_consensus(0x1702_40c3),
        )
        .await?;

        assert_eq!(outcome, ElastosHeightOutcome::NonBtcParentSkipped);
        let row = client
            .query_one(
                "SELECT revoked_at IS NOT NULL, revocation_reason \
                 FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2",
                &[&context.source_id(), &height],
            )
            .await?;
        let revoked: bool = row.get(0);
        let reason: Option<String> = row.get(1);
        assert!(
            revoked,
            "far-future table-horizon downgrade must revoke the active event"
        );
        assert_eq!(reason.as_deref(), Some(ELASTOS_REVOKE_NON_BTC));
        Ok(())
    })
}

#[tokio::test]
async fn above_horizon_core_match_writes_the_event_end_to_end() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // The real stuck block: Elastos child 2,243,660, parent BIP34 955,585 (epoch
        // 955,584, beyond the embedded table horizon). A fresh Core tip resolves the
        // epoch nBits to the parent's own (17021a42) -> Valid -> the event is WRITTEN
        // through the full production path (`write_valid_capture`), not just decided.
        let block: ElastosBlock = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-2243660.json"
        )))
        .expect("deserialize Elastos 2243660 fixture");
        let height = block.height;
        let context = ElastosCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent())
                    .with_synced_tip_height(955_609)
                    .with_epoch_nbits(955_584, 0x1702_1a42, 1_782_525_607),
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
            "an above-horizon Core-Valid parent must write one active event"
        );
        Ok(())
    })
}

#[tokio::test]
async fn in_table_valid_far_future_height_is_revoked_against_fresh_tip() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = writable_elastos_block_without_identity();
        let height = block.height;
        // The fixture's parent is a real in-table BTC block (height 572,333, nBits
        // match -> in-table Valid). A FRESH Core tip far below that claimed height
        // proves it fabricated-far-future, so it must be revoked, not written, even
        // though its nBits matched a covered epoch (the in-table bypass this closes).
        let context = ElastosCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(500_000),
            ),
        )
        .await?;
        let rpc = FixtureElastosRpc { block };
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        assert_eq!(outcome, ElastosHeightOutcome::NonBtcParentSkipped);
        Ok(())
    })
}

/// An `unknown` parent classification over the BTC genesis header, for fake
/// classifiers whose horizon outcome is driven by `synced_tip_height` /
/// `epoch_nbits`, not `classify_parent`.
fn unknown_genesis_parent() -> ParentClassification {
    ParentClassification::unknown(
        &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
    )
}

async fn event_revoked_non_btc(client: &Client, source_id: i64, height: i32) -> Result<bool> {
    let row = client
        .query_one(
            "SELECT revoked_at IS NOT NULL AND revocation_reason = $3 \
             FROM merge_mining_event WHERE source_id = $1 AND child_height = $2",
            &[&source_id, &height, &ELASTOS_REVOKE_NON_BTC],
        )
        .await?;
    Ok(row.get(0))
}

#[tokio::test]
async fn beyond_horizon_core_mismatch_revokes_non_btc() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let block = writable_elastos_block_without_identity();
        let height = block.height;
        // Write a valid in-table event first, so we can observe the later revoke.
        let context = ElastosCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent()).with_synced_tip_height(955_609),
            ),
        )
        .await?;
        let rpc = FixtureElastosRpc { block };
        assert_eq!(
            process_elastos_height(&mut client, &rpc, &context, height).await?,
            ElastosHeightOutcome::AuxpowWritten
        );

        // The Core-resolved epoch nBits (170240c3) differs from the parent's
        // BCH-shaped nBits (1a0fffff): contaminant -> revoke as non-BTC.
        let context = ElastosCaptureContext::new_with_classifier(
            &client,
            ConfiguredParentClassifier::Fake(
                FakeParentClassifier::new(unknown_genesis_parent())
                    .with_synced_tip_height(955_609)
                    .with_epoch_nbits(955_584, 0x1702_40c3, 1_782_525_607),
            ),
        )
        .await?;
        let outcome = process_elastos_table_horizon_for_test(
            &mut client,
            &context,
            height,
            955_585,
            bitcoin::CompactTarget::from_consensus(0x1a0f_ffff),
        )
        .await?;
        assert_eq!(outcome, ElastosHeightOutcome::NonBtcParentSkipped);
        assert!(event_revoked_non_btc(&client, context.source_id(), height).await?);
        Ok(())
    })
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
        Ok(())
    })
}

#[tokio::test]
async fn live_capture_maps_known_reward_and_minerinfo_identities() -> Result<()> {
    crate::run_mut_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let f2pool_id = pool_id_for_slug(&client, "f2pool").await?;
        let reward_identity_id = insert_elastos_identity(
            &client,
            f2pool_id,
            ELASTOS_REWARD_ADDRESS_NAMESPACE,
            F2POOL_REWARD_ADDRESS,
        )
        .await?;
        let minerinfo_identity_id = insert_elastos_identity(
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

// The embedded minerinfo registry is seeded before the capture resolver is
// loaded, so first capture must already map the minerinfo value to a pool.
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

// Older rows may contain only an unmapped stored minerinfo attribution. The
// reclassification path must resolve that value without fetching the child
// block again through RPC.
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
        // Simulate a pre-registry row: no pool mapping yet and no child
        // outputs to rederive from.
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

async fn insert_elastos_identity(
    client: &Client,
    pool_id: i64,
    namespace: &str,
    identifier: &str,
) -> Result<i64> {
    Ok(client
        .query_one(
            "INSERT INTO pool_identity (pool_id, namespace, identifier) \
             VALUES ($1, $2, $3) \
             RETURNING id",
            &[&pool_id, &namespace, &identifier],
        )
        .await?
        .get(0))
}
