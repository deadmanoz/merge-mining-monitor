use anyhow::Result;
use mmm_capture::capture::{
    EventPoolAttribution, PoolAttributionConfidence, PoolAttributionSide,
    RSK_MINER_REGISTRY_SOURCE, RSK_RPC_MINER_SOURCE,
};
use mmm_capture::pool_resolver::RSK_MINER_ADDRESS_NAMESPACE;
use mmm_capture::source_registry::RSK_SOURCE_CODE;
use mmm_producers::{ReclassifyPoolsConfig, ReclassifyPoolsStats, run_reclassify_pools};
use mmm_store::{get_source_id, upsert_event_pool_attributions};
use serde_json::json;
use tokio_postgres::Client;

use crate::support::default_pool_snapshot;
use crate::support::seed::pool_id_for_slug;

const KNOWN_MINER: &str = "12d3178a62ef1f520944534ed04504609f7307a1";
const PRIORITY_UNKNOWN_MINER: &str = "0fd9b9b567a459c6c9645ab0847785aef13dfe1b";

#[tokio::test]
async fn reclassify_pools_materializes_rsk_unresolved_and_late_fills_registry_match() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        let unresolved_event_id =
            insert_rsk_event(&client, 8_800_001, PRIORITY_UNKNOWN_MINER, 1_780_000_000).await?;
        let resolved_event_id =
            insert_rsk_event(&client, 8_800_002, KNOWN_MINER, 1_780_000_100).await?;

        let stats = reclassify_pools(&mut client).await?;
        assert_mixed_rsk_stats(&stats);

        let attribution = rsk_attribution(&client, unresolved_event_id).await?;
        assert_eq!(attribution.pool_id, None);
        assert_eq!(attribution.pool_identity_id, None);
        assert_eq!(attribution.source, RSK_RPC_MINER_SOURCE);
        assert_eq!(attribution.matched_value, PRIORITY_UNKNOWN_MINER);

        let sidecar_identity = late_filled_sidecar_identity(&client, resolved_event_id).await?;

        let attribution = rsk_attribution(&client, resolved_event_id).await?;
        assert!(attribution.pool_id.is_some());
        assert_eq!(attribution.pool_identity_id, Some(sidecar_identity));
        assert_eq!(attribution.source, RSK_MINER_REGISTRY_SOURCE);
        assert_eq!(attribution.identifier.as_deref(), Some(KNOWN_MINER));

        let again = reclassify_pools(&mut client).await?;
        assert_eq!(again.rsk_miner_attribution_updates, 0);
        assert_eq!(again.rsk_miner_sidecar_late_fills, 0);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_pools_requires_overwrite_before_remapping_existing_identity() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let f2pool_id = f2pool_id(&client).await?;
        let antpool_id = pool_id_for_slug(&client, "antpool").await?;
        assert_ne!(f2pool_id, antpool_id);
        let identity_id = insert_rsk_pool_identity(&client, KNOWN_MINER, antpool_id).await?;

        let err = reclassify_pools(&mut client)
            .await
            .expect_err("default reclassification must reject RSK identity remaps");
        assert!(format!("{err:#}").contains("--overwrite"));
        assert_eq!(
            pool_identity_pool_id(&client, identity_id).await?,
            antpool_id
        );

        let stats = reclassify_pools_with_overwrite(&mut client).await?;
        assert_eq!(stats.rsk_miner_rows_scanned, 0);
        assert_eq!(
            pool_identity_pool_id(&client, identity_id).await?,
            f2pool_id
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_pools_requires_overwrite_before_late_filling_conflicting_sidecar() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        let event_id = insert_rsk_event(&client, 8_800_004, KNOWN_MINER, 1_780_000_175).await?;
        let f2pool_id = f2pool_id(&client).await?;
        let antpool_id = pool_id_for_slug(&client, "antpool").await?;
        seed_child_miner_registry_attribution(&client, event_id, KNOWN_MINER, antpool_id, None)
            .await?;

        let err = reclassify_pools(&mut client)
            .await
            .expect_err("default reclassification must not late-fill conflicting sidecar identity");
        assert!(format!("{err:#}").contains("--overwrite"));
        assert_eq!(sidecar_identity(&client, event_id).await?, None);
        assert_eq!(
            event_attribution_pool_id(&client, event_id).await?,
            Some(antpool_id)
        );

        let stats = reclassify_pools_with_overwrite(&mut client).await?;
        assert_resolved_rsk_stats(&stats);
        assert!(sidecar_identity(&client, event_id).await?.is_some());
        assert_eq!(
            event_attribution_pool_id(&client, event_id).await?,
            Some(f2pool_id)
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn only_main_skips_the_rsk_pass() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // An RSK event the RSK pass would scan under the default config.
        insert_rsk_event(&client, 8_800_010, KNOWN_MINER, 1_780_000_300).await?;

        // `--only main` (run only the main candidate scan) must not execute the
        // expensive RSK miner-identity pass at all: zero rows scanned, no writes.
        let only_main = run_reclassify_pools(
            &mut client,
            ReclassifyPoolsConfig {
                run_rsk: false,
                run_hathor: false,
                run_elastos: false,
                ..ReclassifyPoolsConfig::default()
            },
        )
        .await?;
        assert_eq!(only_main.rsk_miner_rows_scanned, 0);
        assert_eq!(only_main.rsk_miner_attribution_updates, 0);

        // The default config still runs the RSK pass over the same event, proving
        // the gate, not a missing sidecar, is what suppressed the scan above.
        let full = reclassify_pools(&mut client).await?;
        assert_eq!(full.rsk_miner_rows_scanned, 1);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rsk_watermark_skips_unchanged_rerun() -> Result<()> {
    crate::run_mut_db_test!(client, {
        insert_rsk_event(&client, 8_800_020, KNOWN_MINER, 1_780_000_400).await?;
        // First run scans the corpus and records the watermark.
        let first = reclassify_pools(&mut client).await?;
        assert_eq!(first.rsk_miner_rows_scanned, 1);
        // An unchanged re-run (same registry, same active set) short-circuits.
        let second = reclassify_pools(&mut client).await?;
        assert_eq!(second.rsk_miner_rows_scanned, 0);
        assert_eq!(second.rsk_miner_attribution_updates, 0);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rsk_watermark_does_not_skip_after_a_new_event() -> Result<()> {
    crate::run_mut_db_test!(client, {
        insert_rsk_event(&client, 8_800_021, KNOWN_MINER, 1_780_000_410).await?;
        let first = reclassify_pools(&mut client).await?;
        assert_eq!(first.rsk_miner_rows_scanned, 1);
        // A newly captured RSK event advances the active-set fingerprint, so the
        // next run must rescan, not skip.
        insert_rsk_event(&client, 8_800_022, KNOWN_MINER, 1_780_000_411).await?;
        let second = reclassify_pools(&mut client).await?;
        assert_eq!(second.rsk_miner_rows_scanned, 2);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rsk_watermark_never_skips_an_overwrite_run() -> Result<()> {
    crate::run_mut_db_test!(client, {
        insert_rsk_event(&client, 8_800_023, KNOWN_MINER, 1_780_000_420).await?;
        // Prime the watermark with a default run.
        reclassify_pools(&mut client).await?;
        // --overwrite deliberately rewrites history, so it must ignore the
        // watermark and rescan.
        let overwrite = reclassify_pools_with_overwrite(&mut client).await?;
        assert_eq!(overwrite.rsk_miner_rows_scanned, 1);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rsk_watermark_skip_still_enforces_the_seed_remap_conflict() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // Prime the watermark over a clean default run (this also self-seeds the
        // KNOWN_MINER -> f2pool identity).
        insert_rsk_event(&client, 8_800_024, KNOWN_MINER, 1_780_000_430).await?;
        reclassify_pools(&mut client).await?;
        let f2pool = pool_id_for_slug(&client, "f2pool").await?;
        let antpool = pool_id_for_slug(&client, "antpool").await?;
        assert_ne!(f2pool, antpool);

        // Corrupt the identity to a conflicting pool. This does NOT change the
        // active-event set, so the watermark fingerprint is unchanged and the
        // scan would be skipped - but the seed/conflict pass runs BEFORE the
        // skip-check, so a default run must still bail.
        client
            .execute(
                "UPDATE pool_identity SET pool_id = $1 \
                 WHERE namespace = 'rsk_miner_address' AND identifier = $2",
                &[&antpool, &KNOWN_MINER],
            )
            .await?;
        let err = reclassify_pools(&mut client).await.expect_err(
            "seed must bail on an unauthorized remap even when the watermark would skip",
        );
        assert!(format!("{err:#}").contains("--overwrite"));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rsk_watermark_detects_balanced_revoke_restore() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let e1 = insert_rsk_event(&client, 8_800_025, KNOWN_MINER, 1_780_000_440).await?;
        let _e2 = insert_rsk_event(&client, 8_800_026, KNOWN_MINER, 1_780_000_441).await?;
        let e3 = insert_rsk_event(&client, 8_800_027, KNOWN_MINER, 1_780_000_442).await?;
        let _e4 = insert_rsk_event(&client, 8_800_028, KNOWN_MINER, 1_780_000_443).await?;
        // Start with e3 revoked: active set {e1, e2, e4}.
        set_revoked(&client, e3, true).await?;
        let first = reclassify_pools(&mut client).await?;
        assert_eq!(first.rsk_miner_rows_scanned, 3);

        // Balanced swap that preserves BOTH the active count and the max id:
        // revoke e1, restore e3 -> active {e2, e3, e4}. Only the order-independent
        // XOR digest distinguishes this from {e1, e2, e4}, so the run must rescan.
        set_revoked(&client, e1, true).await?;
        set_revoked(&client, e3, false).await?;
        let second = reclassify_pools(&mut client).await?;
        assert_eq!(second.rsk_miner_rows_scanned, 3);
        Ok::<_, anyhow::Error>(())
    })
}

async fn set_revoked(client: &Client, event_id: i64, revoked: bool) -> Result<()> {
    let revoked_at: Option<i64> = if revoked { Some(1_780_900_000) } else { None };
    client
        .execute(
            "UPDATE merge_mining_event SET revoked_at = $2 WHERE id = $1",
            &[&event_id, &revoked_at],
        )
        .await?;
    Ok(())
}

async fn insert_rsk_event(
    client: &Client,
    child_height: i32,
    miner_address: &str,
    confirmed_at: i64,
) -> Result<i64> {
    let source_id = get_source_id(client, RSK_SOURCE_CODE).await?;
    let child_block_hash = repeated_bytes(child_height, 0x10);
    let parent_hash = repeated_bytes(child_height, 0x20);
    let prev_hash = repeated_bytes(child_height, 0x30);
    let header_bytes = vec![0u8; 80];
    let event_id: i64 = client
        .query_one(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_prev_header_hash, \
                btc_parent_header_bytes, btc_parent_header_time, btc_parent_kind, \
                pow_validates_btc_target, pow_validates_child_target, \
                discovered_at, confirmed_at \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, $8, $9, true, NULL, $10, $11 \
             ) RETURNING id",
            &[
                &source_id,
                &child_height,
                &child_block_hash,
                &confirmed_at,
                &parent_hash,
                &prev_hash,
                &header_bytes,
                &confirmed_at,
                &"unknown",
                &confirmed_at,
                &confirmed_at,
            ],
        )
        .await?
        .get(0);
    let rsk_miner = hex::decode(miner_address)?;
    let rsk_block_hash = repeated_bytes(child_height, 0x40);
    let merge_mining_hash = repeated_bytes(child_height, 0x50);
    let coinbase_tail = Some(vec![0x52_u8, 0x53, 0x4b]);
    let pool_identity_id: Option<i64> = None;
    client
        .execute(
            "INSERT INTO rsk_merge_mining_evidence ( \
                event_id, rsk_block_hash, rsk_height, is_uncle, uncle_index, \
                uncle_parent_height, rsk_miner, pool_identity_id, merge_mining_hash, \
                coinbase_tail, proof_format \
             ) VALUES ( \
                $1, $2, $3, $4, NULL, NULL, $5, $6, $7, $8, 'rskj_rpc_opaque' \
             )",
            &[
                &event_id,
                &rsk_block_hash,
                &child_height,
                &false,
                &rsk_miner,
                &pool_identity_id,
                &merge_mining_hash,
                &coinbase_tail,
            ],
        )
        .await?;
    Ok(event_id)
}

fn repeated_bytes(seed: i32, prefix: u8) -> Vec<u8> {
    let mut bytes = vec![prefix; 32];
    bytes[0..4].copy_from_slice(&seed.to_be_bytes());
    bytes
}

fn assert_resolved_rsk_stats(stats: &ReclassifyPoolsStats) {
    assert_eq!(stats.rsk_miner_rows_scanned, 1);
    assert_eq!(stats.rsk_miner_registry_resolved_rows, 1);
    assert_eq!(stats.rsk_miner_unresolved_rows, 0);
    assert_eq!(stats.rsk_miner_attribution_updates, 1);
    assert_eq!(stats.rsk_miner_sidecar_late_fills, 1);
}

fn assert_mixed_rsk_stats(stats: &ReclassifyPoolsStats) {
    assert_eq!(stats.rsk_miner_rows_scanned, 2);
    assert_eq!(stats.rsk_miner_registry_resolved_rows, 1);
    assert_eq!(stats.rsk_miner_unresolved_rows, 1);
    assert_eq!(stats.rsk_miner_attribution_updates, 2);
    assert_eq!(stats.rsk_miner_sidecar_late_fills, 1);
}

async fn reclassify_pools(client: &mut Client) -> Result<ReclassifyPoolsStats> {
    run_reclassify_pools(client, ReclassifyPoolsConfig::default()).await
}

async fn reclassify_pools_with_overwrite(client: &mut Client) -> Result<ReclassifyPoolsStats> {
    run_reclassify_pools(
        client,
        ReclassifyPoolsConfig {
            overwrite: true,
            ..ReclassifyPoolsConfig::default()
        },
    )
    .await
}

struct RskAttribution {
    pool_id: Option<i64>,
    pool_identity_id: Option<i64>,
    source: String,
    matched_value: String,
    identifier: Option<String>,
}

async fn rsk_attribution(client: &Client, event_id: i64) -> Result<RskAttribution> {
    let row = client
        .query_one(
            "SELECT a.pool_id, a.pool_identity_id, a.source, a.matched_value, pi.identifier \
             FROM event_pool_attribution a \
             LEFT JOIN pool_identity pi ON pi.id = a.pool_identity_id \
             WHERE a.event_id = $1",
            &[&event_id],
        )
        .await?;
    Ok(RskAttribution {
        pool_id: row.get(0),
        pool_identity_id: row.get(1),
        source: row.get(2),
        matched_value: row.get(3),
        identifier: row.get(4),
    })
}

async fn late_filled_sidecar_identity(client: &Client, event_id: i64) -> Result<i64> {
    let identity_id = sidecar_identity(client, event_id).await?;
    identity_id.ok_or_else(|| anyhow::anyhow!("sidecar identity late-filled"))
}

async fn sidecar_identity(client: &Client, event_id: i64) -> Result<Option<i64>> {
    Ok(client
        .query_one(
            "SELECT pool_identity_id FROM rsk_merge_mining_evidence WHERE event_id = $1",
            &[&event_id],
        )
        .await?
        .get(0))
}

async fn f2pool_id(client: &Client) -> Result<i64> {
    let (_, pool_ids) = default_pool_snapshot(client).await?;
    pool_ids
        .get("f2pool")
        .copied()
        .ok_or_else(|| anyhow::anyhow!("f2pool in snapshot"))
}

async fn insert_rsk_pool_identity(
    client: &Client,
    miner_address: &str,
    pool_id: i64,
) -> Result<i64> {
    client
        .query_one(
            "INSERT INTO pool_identity (pool_id, namespace, identifier) \
             VALUES ($1, $2, $3) RETURNING id",
            &[&pool_id, &RSK_MINER_ADDRESS_NAMESPACE, &miner_address],
        )
        .await
        .map(|row| row.get(0))
        .map_err(Into::into)
}

async fn pool_identity_pool_id(client: &Client, identity_id: i64) -> Result<i64> {
    client
        .query_one(
            "SELECT pool_id FROM pool_identity WHERE id = $1",
            &[&identity_id],
        )
        .await
        .map(|row| row.get(0))
        .map_err(Into::into)
}

async fn event_attribution_pool_id(client: &Client, event_id: i64) -> Result<Option<i64>> {
    client
        .query_one(
            "SELECT pool_id FROM event_pool_attribution WHERE event_id = $1",
            &[&event_id],
        )
        .await
        .map(|row| row.get(0))
        .map_err(Into::into)
}

async fn seed_child_miner_registry_attribution(
    client: &Client,
    event_id: i64,
    miner_address: &str,
    pool_id: i64,
    pool_identity_id: Option<i64>,
) -> Result<()> {
    upsert_event_pool_attributions(
        client,
        event_id,
        &[EventPoolAttribution {
            side: PoolAttributionSide::ChildBlock,
            namespace: RSK_MINER_ADDRESS_NAMESPACE,
            match_kind: "miner_address",
            matched_value: miner_address.to_owned(),
            pool_id: Some(pool_id),
            pool_identity_id,
            source: RSK_MINER_REGISTRY_SOURCE,
            confidence: PoolAttributionConfidence::High,
            details: json!({}),
        }],
        1_780_000_200,
    )
    .await
}
