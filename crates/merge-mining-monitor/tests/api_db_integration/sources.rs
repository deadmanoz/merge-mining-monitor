use std::collections::BTreeSet;

use anyhow::Result;
use mmm_api::projection::{self, SourceEndpointRecord, SourcesPayload};
use mmm_capture::source_registry::{
    BITCOIN_SOURCE_CODE, NAMECOIN_SOURCE_CODE, RSK_SOURCE_CODE, SYSCOIN_SOURCE_CODE,
};
use mmm_capture::source_registry::{SOURCE_REGISTRY, SourceLifecycle};
use mmm_read_model::rebuild_source_health;
use mmm_store::get_source_id;
use time::Month;
use tokio_postgres::Client;

use crate::support::seed::{
    EventSeed, day_epoch, hash_bytes, insert_attestation_proof, insert_block, insert_event,
    insert_orphan,
};

use crate::helpers::format_projection_error;

const ADD_ELCASH_MIGRATION: &str =
    include_str!("../../../../migrations/0003_add_elcash_source.sql");
const REMOVE_MAZACOIN_MIGRATION: &str =
    include_str!("../../../../migrations/0004_remove_mazacoin_source.sql");

#[tokio::test]
async fn sources_counts_events_without_classifier_observation_counts() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (ts, stale_ts) = seed_source_status_projection_rows(&client).await?;

        // Raw insert_event seeds bypass source_health maintenance, and /sources
        // now fails closed until a rebuild. Rebuild from base so the read serves.
        rebuild_source_health(&mut client).await?;
        let payload = projection::sources(&client, (ts + 60) as u64)
            .await
            .map_err(format_projection_error)?;
        let namecoin_row = source_row(&payload, NAMECOIN_SOURCE_CODE);
        assert_eq!(namecoin_row.status, "fresh");
        assert_source_sync(
            namecoin_row,
            "live",
            "stale",
            Some(141),
            Some(stale_ts),
            Some(150),
        );
        assert_eq!(namecoin_row.sync.latest_evidence_at, Some(ts));
        assert_eq!(namecoin_row.counts.events, 2);
        assert_eq!(namecoin_row.counts.near, 1);
        let rsk_row = source_row(&payload, RSK_SOURCE_CODE);
        assert_eq!(rsk_row.status, "stale");
        assert_source_sync(
            rsk_row,
            "live",
            "catching_up",
            Some(142),
            Some(ts),
            Some(150),
        );
        assert_eq!(rsk_row.sync.latest_evidence_at, Some(stale_ts));
        assert_eq!(rsk_row.counts.events, 1);
        assert_eq!(rsk_row.counts.near, 1);
        let bitcoin_row = source_row(&payload, BITCOIN_SOURCE_CODE);
        assert_eq!(bitcoin_row.status, "not_started");
        assert_eq!(bitcoin_row.last_seen_at, None);
        assert_source_sync(
            bitcoin_row,
            "bitcoin-core-backbone",
            "catching_up",
            Some(198),
            Some(ts),
            Some(200),
        );
        assert_eq!(bitcoin_row.sync.latest_evidence_at, None);
        assert_eq!(bitcoin_row.sync.error_code, None);
        assert_eq!(bitcoin_row.sync.error_height, None);
        assert_eq!(bitcoin_row.counts.events, 0);
        assert_eq!(bitcoin_row.counts.canonical, 0);
        let syscoin_row = source_row(&payload, SYSCOIN_SOURCE_CODE);
        assert_eq!(syscoin_row.status, "not_started");
        assert_source_sync(syscoin_row, "live", "not_started", None, None, None);
        assert_eq!(syscoin_row.counts.events, 0);

        insert_event(
            &client,
            EventSeed {
                source_id: get_source_id(&client, NAMECOIN_SOURCE_CODE).await?,
                child_height: 999,
                child_hash: hash_bytes(0x9001),
                parent_hash: hash_bytes(0x9101),
                prev_hash: hash_bytes(0x9100),
                parent_time: ts,
                kind: "unknown",
                pow_validates_btc_target: false,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;
        rebuild_source_health(&mut client).await?;
        assert!(
            projection::sources(&client, (ts + 60) as u64)
                .await
                .is_err(),
            "an invalid-unknown parent must fail the /sources guard"
        );

        Ok::<_, anyhow::Error>(())
    })
}

async fn seed_source_status_projection_rows(client: &Client) -> Result<(i64, i64)> {
    let namecoin = get_source_id(client, NAMECOIN_SOURCE_CODE).await?;
    let rsk = get_source_id(client, RSK_SOURCE_CODE).await?;
    let bitcoin = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let ts = day_epoch(2026, Month::May, 10);
    let stale_ts = ts - 10 * 24 * 60 * 60;
    let parent = hash_bytes(0xb101);
    for child_height in [140, 141] {
        insert_event(
            client,
            EventSeed {
                source_id: namecoin,
                child_height,
                child_hash: hash_bytes(0xb200 + child_height as u32),
                parent_hash: parent.clone(),
                prev_hash: hash_bytes(0xb100),
                parent_time: ts,
                kind: "near",
                pow_validates_btc_target: false,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;
    }
    let canonical = hash_bytes(0xb301);
    insert_block(
        client,
        &canonical,
        &hash_bytes(0xb300),
        Some(30),
        "canonical",
        ts,
        None,
    )
    .await?;
    insert_event(
        client,
        EventSeed {
            source_id: rsk,
            child_height: 142,
            child_hash: hash_bytes(0xb401),
            parent_hash: hash_bytes(0xb501),
            prev_hash: hash_bytes(0xb500),
            parent_time: stale_ts,
            kind: "near",
            pow_validates_btc_target: false,
            btc_height: None,
            pool_id: None,
        },
    )
    .await?;
    insert_poll_cursor(client, namecoin, 141, Some(150), stale_ts).await?;
    insert_poll_cursor(client, rsk, 142, Some(150), ts).await?;
    insert_bitcoin_core_sync_state(client, bitcoin, Some(200), 198, ts, None, None).await?;
    Ok((ts, stale_ts))
}

#[tokio::test]
async fn sources_project_bitcoin_core_backbone_statuses() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let bitcoin = get_source_id(&client, BITCOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);
        let stale_updated_at = ts - 2 * 60 * 60;

        rebuild_source_health(&mut client).await?;
        let payload = projection::sources(&client, ts as u64)
            .await
            .map_err(format_projection_error)?;
        let bitcoin_row = source_row(&payload, BITCOIN_SOURCE_CODE);
        assert_source_sync(
            bitcoin_row,
            "bitcoin-core-backbone",
            "not_started",
            None,
            None,
            None,
        );

        insert_bitcoin_core_sync_state(&client, bitcoin, Some(200), 200, ts, None, None).await?;
        let payload = projection::sources(&client, ts as u64)
            .await
            .map_err(format_projection_error)?;
        let bitcoin_row = source_row(&payload, BITCOIN_SOURCE_CODE);
        assert_source_sync(
            bitcoin_row,
            "bitcoin-core-backbone",
            "live",
            Some(200),
            Some(ts),
            Some(200),
        );

        insert_bitcoin_core_sync_state(
            &client,
            bitcoin,
            Some(200),
            198,
            stale_updated_at,
            Some("coinbase_fetch_failed"),
            Some(199),
        )
        .await?;

        rebuild_source_health(&mut client).await?;
        let payload = projection::sources(&client, ts as u64)
            .await
            .map_err(format_projection_error)?;
        let bitcoin_row = source_row(&payload, BITCOIN_SOURCE_CODE);
        assert_source_sync(
            bitcoin_row,
            "bitcoin-core-backbone",
            "error",
            Some(198),
            Some(stale_updated_at),
            Some(200),
        );
        assert_eq!(
            bitcoin_row.sync.error_code.as_deref(),
            Some("coinbase_fetch_failed")
        );
        assert_eq!(bitcoin_row.sync.error_height, Some(199));

        Ok::<_, anyhow::Error>(())
    })
}

fn source_row<'a>(payload: &'a SourcesPayload, code: &str) -> &'a SourceEndpointRecord {
    payload
        .sources
        .iter()
        .find(|source| source.code == code)
        .unwrap_or_else(|| panic!("{code} source"))
}

fn assert_source_sync(
    source: &SourceEndpointRecord,
    mode: &str,
    state: &str,
    progress_height: Option<i32>,
    progress_updated_at: Option<i64>,
    target_height: Option<i32>,
) {
    assert_eq!(source.sync.mode, mode);
    assert_eq!(source.sync.state, state);
    assert_eq!(source.sync.progress_height, progress_height);
    assert_eq!(source.sync.progress_updated_at, progress_updated_at);
    assert_eq!(source.sync.target_height, target_height);
}

fn assert_empty_registry_lifecycle(
    payload: &SourcesPayload,
    lifecycle: SourceLifecycle,
    mode: &str,
) {
    let code = SOURCE_REGISTRY
        .iter()
        .find(|definition| definition.lifecycle == lifecycle)
        .expect("lifecycle source in registry")
        .code;
    let source = source_row(payload, code);
    assert_source_sync(source, mode, mode, None, None, None);
    assert_eq!(source.sync.latest_evidence_at, None);
    assert_eq!(source.sync.error_code, None);
    assert_eq!(source.sync.error_height, None);
}

fn assert_registry_projection(payload: &SourcesPayload) {
    let projected: BTreeSet<&str> = payload.sources.iter().map(|s| s.code.as_str()).collect();
    let registry: BTreeSet<&str> = SOURCE_REGISTRY.iter().map(|source| source.code).collect();
    assert_eq!(
        projected, registry,
        "/sources codes must equal the registry"
    );
    for (lifecycle, mode) in [
        (SourceLifecycle::Historical, "historical"),
        (SourceLifecycle::Partial, "partial"),
        (SourceLifecycle::Surveyed, "surveyed"),
        (SourceLifecycle::Catalogued, "catalogued"),
    ] {
        assert_empty_registry_lifecycle(payload, lifecycle, mode);
    }
}

async fn insert_bitcoin_core_sync_state(
    client: &Client,
    source_id: i64,
    target_tip_height: Option<i32>,
    contiguous_complete_height: i32,
    updated_at: i64,
    last_error_code: Option<&str>,
    last_error_height: Option<i32>,
) -> Result<()> {
    let target_tip_hash = target_tip_height.map(|_| hash_bytes(0xb007));
    client
        .execute(
            "INSERT INTO bitcoin_core_sync_state ( \
                source_id, sync_mode, target_tip_height, target_tip_hash, \
                contiguous_complete_height, last_error_code, last_error_height, \
                created_at, updated_at \
             ) VALUES ($1, 'contiguous', $2, $3, $4, $5, $6, $7, $7) \
             ON CONFLICT (source_id, sync_mode) DO UPDATE SET \
                target_tip_height = EXCLUDED.target_tip_height, \
                target_tip_hash = EXCLUDED.target_tip_hash, \
                contiguous_complete_height = EXCLUDED.contiguous_complete_height, \
                last_error_code = EXCLUDED.last_error_code, \
                last_error_height = EXCLUDED.last_error_height, \
                updated_at = EXCLUDED.updated_at",
            &[
                &source_id,
                &target_tip_height,
                &target_tip_hash,
                &contiguous_complete_height,
                &last_error_code,
                &last_error_height,
                &updated_at,
            ],
        )
        .await?;
    Ok(())
}

async fn insert_poll_cursor(
    client: &Client,
    source_id: i64,
    cursor_height: i32,
    target_height: Option<i32>,
    updated_at: i64,
) -> Result<()> {
    let updated_at_f64 = updated_at as f64;
    client
        .execute(
            "INSERT INTO poll_cursor (source_id, cursor_height, target_height, updated_at) \
             VALUES ($1, $2, $3, to_timestamp($4::DOUBLE PRECISION))",
            &[&source_id, &cursor_height, &target_height, &updated_at_f64],
        )
        .await?;
    Ok(())
}

/// The seeded `source` table EXACTLY equals the Source Lifecycle Registry,
/// including each explicit permanent id and any retired gaps. This is the
/// seed<->registry drift guard for all registered sources: it catches a typo, omission,
/// or extra row in the generated `0002_seed_sources.sql` relative to the Rust
/// `SOURCE_REGISTRY`, in either direction.
#[tokio::test]
async fn source_table_matches_registry() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let rows = client
            .query(
                "SELECT id, code, kind, chain, instance FROM source ORDER BY id",
                &[],
            )
            .await?;
        assert_eq!(
            rows.len(),
            SOURCE_REGISTRY.len(),
            "source row count vs registry"
        );
        for (def, row) in SOURCE_REGISTRY.iter().zip(rows.iter()) {
            let id: i64 = row.get(0);
            assert_eq!(id, def.id, "{} explicit id", def.code);
            assert_eq!(row.get::<_, String>(1), def.code, "{} code", def.code);
            assert_eq!(
                row.get::<_, String>(2),
                def.kind.as_str(),
                "{} kind",
                def.code
            );
            assert_eq!(
                row.get::<_, Option<String>>(3).as_deref(),
                Some(def.chain),
                "{} chain",
                def.code
            );
            assert_eq!(
                row.get::<_, Option<String>>(4).as_deref(),
                def.instance,
                "{} instance",
                def.code
            );
        }

        assert!(
            projection::sources(&client, 1_800_000_000).await.is_err(),
            "/sources must fail closed before the first source_health rebuild"
        );

        rebuild_source_health(&mut client).await?;
        let payload = projection::sources(&client, 1_800_000_000)
            .await
            .map_err(format_projection_error)?;
        assert_registry_projection(&payload);

        Ok(())
    })
}

async fn insert_legacy_mazacoin_source(client: &Client) -> Result<()> {
    client
        .execute(
            "INSERT INTO source (id, code, kind, chain, instance, created_at) \
             OVERRIDING SYSTEM VALUE \
             VALUES (32, 'auxpow:mazacoin', 'auxpow', 'mazacoin', NULL, 1)",
            &[],
        )
        .await?;
    Ok(())
}

async fn insert_legacy_mazacoin_state(client: &Client) -> Result<()> {
    client
        .batch_execute(
            "INSERT INTO source_health (source_id) VALUES (32); \
             INSERT INTO poll_cursor (source_id, cursor_height) VALUES (32, 7); \
             INSERT INTO poll_pending_reconcile (source_id, height, kind) \
                 VALUES (32, 8, 'reconcile'); \
             INSERT INTO bitcoin_core_sync_state ( \
                 source_id, sync_mode, created_at, updated_at \
             ) VALUES (32, 'contiguous', 1, 1);",
        )
        .await?;
    Ok(())
}

fn assert_migration_error(error: &tokio_postgres::Error, expected: &str) {
    let message = error
        .as_db_error()
        .map(|error| error.message())
        .unwrap_or_default();
    assert!(
        message.contains(expected),
        "expected migration error containing {expected:?}, got {message:?}"
    );
}

async fn assert_legacy_mazacoin_source_and_health_preserved(client: &Client) -> Result<()> {
    let row = client
        .query_one(
            "SELECT \
               (SELECT count(*) FROM source WHERE id = 32), \
               (SELECT count(*) FROM source_health WHERE source_id = 32)",
            &[],
        )
        .await?;
    assert_eq!(row.get::<_, i64>(0), 1);
    assert_eq!(row.get::<_, i64>(1), 1, "guard must run before cleanup");
    Ok(())
}

#[tokio::test]
async fn elcash_forward_migration_assigns_permanent_id_on_legacy_sequence() -> Result<()> {
    crate::run_db_test!(client, {
        client
            .batch_execute(
                "DELETE FROM source WHERE code = 'auxpow:elcash'; \
                 ALTER TABLE source ALTER COLUMN id RESTART WITH 34;",
            )
            .await?;

        client.batch_execute(ADD_ELCASH_MIGRATION).await?;
        client.batch_execute(ADD_ELCASH_MIGRATION).await?;
        let row = client
            .query_one(
                "SELECT id, count(*) OVER () FROM source WHERE code = 'auxpow:elcash'",
                &[],
            )
            .await?;
        assert_eq!(row.get::<_, i64>(0), 34);
        assert_eq!(row.get::<_, i64>(1), 1, "0003 must remain idempotent");

        client.batch_execute(REMOVE_MAZACOIN_MIGRATION).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn mazacoin_removal_migration_rejects_wrong_elcash_identity_before_cleanup() -> Result<()> {
    crate::run_db_test!(client, {
        client
            .batch_execute(
                "DELETE FROM source WHERE code = 'auxpow:elcash'; \
                 ALTER TABLE source ALTER COLUMN id RESTART WITH 35;",
            )
            .await?;
        client.batch_execute(ADD_ELCASH_MIGRATION).await?;
        let wrong_id: i64 = client
            .query_one("SELECT id FROM source WHERE code = 'auxpow:elcash'", &[])
            .await?
            .get(0);
        assert_eq!(wrong_id, 35, "test setup must exercise the bad 0003 path");

        insert_legacy_mazacoin_source(&client).await?;
        insert_legacy_mazacoin_state(&client).await?;
        let error = client
            .batch_execute(REMOVE_MAZACOIN_MIGRATION)
            .await
            .expect_err("wrong Elcash identity must block migration");
        assert_migration_error(&error, "expected auxpow:elcash at id 34");
        assert_legacy_mazacoin_source_and_health_preserved(&client).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn mazacoin_removal_migration_cleans_state_preserves_ids_and_is_idempotent() -> Result<()> {
    crate::run_db_test!(client, {
        insert_legacy_mazacoin_source(&client).await?;
        insert_legacy_mazacoin_state(&client).await?;

        client.batch_execute(REMOVE_MAZACOIN_MIGRATION).await?;
        client.batch_execute(REMOVE_MAZACOIN_MIGRATION).await?;

        let row = client
            .query_one(
                "SELECT \
                   (SELECT count(*) FROM source WHERE id = 32), \
                   (SELECT count(*) FROM source_health WHERE source_id = 32), \
                   (SELECT count(*) FROM poll_cursor WHERE source_id = 32), \
                   (SELECT count(*) FROM poll_pending_reconcile WHERE source_id = 32), \
                   (SELECT count(*) FROM bitcoin_core_sync_state WHERE source_id = 32)",
                &[],
            )
            .await?;
        for column in 0..5 {
            assert_eq!(row.get::<_, i64>(column), 0, "cleanup column {column}");
        }

        let rows = client
            .query(
                "SELECT id, code FROM source WHERE id IN (33, 34) ORDER BY id",
                &[],
            )
            .await?;
        let preserved: Vec<(i64, String)> =
            rows.iter().map(|row| (row.get(0), row.get(1))).collect();
        assert_eq!(
            preserved,
            vec![
                (33, "auxpow:bitcoin-stash".to_owned()),
                (34, "auxpow:elcash".to_owned()),
            ]
        );

        let next_id: i64 = client
            .query_one(
                "INSERT INTO source (code, kind, chain, instance, created_at) \
                 VALUES ('auxpow:identity-probe', 'auxpow', 'identity-probe', NULL, 1) \
                 RETURNING id",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(next_id, 35, "fresh-seed identity must resume after max id");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn mazacoin_removal_migration_refuses_event_evidence_before_cleanup() -> Result<()> {
    crate::run_db_test!(client, {
        insert_legacy_mazacoin_source(&client).await?;
        insert_legacy_mazacoin_state(&client).await?;
        insert_event(
            &client,
            EventSeed {
                source_id: 32,
                child_height: 1,
                child_hash: hash_bytes(0x3201),
                parent_hash: hash_bytes(0x3202),
                prev_hash: hash_bytes(0x3203),
                parent_time: 1,
                kind: "near",
                pow_validates_btc_target: false,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;

        let error = client
            .batch_execute(REMOVE_MAZACOIN_MIGRATION)
            .await
            .expect_err("event evidence must block source retirement");
        assert_migration_error(&error, "merge_mining_event evidence exists");
        assert_legacy_mazacoin_source_and_health_preserved(&client).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn mazacoin_removal_migration_refuses_attestation_evidence_before_cleanup() -> Result<()> {
    crate::run_db_test!(client, {
        insert_legacy_mazacoin_source(&client).await?;
        client
            .execute("INSERT INTO source_health (source_id) VALUES (32)", &[])
            .await?;
        let hash = hash_bytes(0x3211);
        insert_block(
            &client,
            &hash,
            &hash_bytes(0x3210),
            Some(1),
            "canonical",
            1,
            None,
        )
        .await?;
        insert_attestation_proof(&client, &hash, 32, &[1], 1).await?;

        let error = client
            .batch_execute(REMOVE_MAZACOIN_MIGRATION)
            .await
            .expect_err("attestation evidence must block source retirement");
        assert_migration_error(&error, "attestation_proof evidence exists");
        assert_legacy_mazacoin_source_and_health_preserved(&client).await?;

        Ok::<_, anyhow::Error>(())
    })
}

/// End-to-end projection mapping for the strict/weak orphan sub-counts: seed
/// orphan-classified parents, rebuild source_health, and assert
/// `projection::sources` carries the nonzero `counts.strict_orphan` /
/// `counts.weak_orphan` (and keeps `near`/`unknown` correct), catching an
/// index/field swap or default-zero leak in `load_source_count_aggregates` that
/// the from-base oracle and the field-presence fixture contract cannot. Also
/// covers the parent-level semantic: a weak-only chain (RSK) that shares a
/// strict-classified parent counts it as strict.
#[tokio::test]
async fn sources_projection_maps_strict_and_weak_orphan_counts() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let rsk = get_source_id(&client, RSK_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);

        // A strict and a weak orphan parent (PoW-valid kind='unknown' blocks), plus
        // a near parent (no block row).
        let strict_parent = hash_bytes(0xA001);
        let weak_parent = hash_bytes(0xA002);
        let near_parent = hash_bytes(0xA003);
        insert_orphan(
            &client,
            &strict_parent,
            &hash_bytes(0xA000),
            ts,
            "strict_btc_orphan",
        )
        .await?;
        insert_orphan(
            &client,
            &weak_parent,
            &hash_bytes(0xA000),
            ts,
            "weak_btc_orphan",
        )
        .await?;

        // namecoin attests all three; rsk shares the strict parent.
        for (i, (parent, kind, pow_ok)) in [
            (&strict_parent, "unknown", true),
            (&weak_parent, "unknown", true),
            (&near_parent, "near", false),
        ]
        .into_iter()
        .enumerate()
        {
            insert_event(
                &client,
                EventSeed {
                    source_id: namecoin,
                    child_height: 10 + i as i32,
                    child_hash: hash_bytes(0xB000 + i as u32),
                    parent_hash: parent.clone(),
                    prev_hash: hash_bytes(0xA000),
                    parent_time: ts,
                    kind,
                    pow_validates_btc_target: pow_ok,
                    btc_height: None,
                    pool_id: None,
                },
            )
            .await?;
        }
        insert_event(
            &client,
            EventSeed {
                source_id: rsk,
                child_height: 20,
                child_hash: hash_bytes(0xB100),
                parent_hash: strict_parent.clone(),
                prev_hash: hash_bytes(0xA000),
                parent_time: ts,
                kind: "unknown",
                pow_validates_btc_target: true,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;

        rebuild_source_health(&mut client).await?;
        let payload = projection::sources(&client, (ts + 60) as u64)
            .await
            .map_err(format_projection_error)?;

        let namecoin_row = source_row(&payload, NAMECOIN_SOURCE_CODE);
        assert_eq!(
            namecoin_row.counts.strict_orphan, 1,
            "namecoin strict orphan"
        );
        assert_eq!(namecoin_row.counts.weak_orphan, 1, "namecoin weak orphan");
        assert_eq!(
            namecoin_row.counts.unknown, 2,
            "two unknown (orphan) parents"
        );
        assert_eq!(namecoin_row.counts.near, 1, "one near parent");
        assert_eq!(namecoin_row.counts.canonical, 0);
        assert_eq!(namecoin_row.counts.stale, 0);

        // Parent-level: RSK shares the strict-classified parent, so it counts as a
        // strict orphan for RSK too even though RSK is weak-only eligible.
        let rsk_row = source_row(&payload, RSK_SOURCE_CODE);
        assert_eq!(
            rsk_row.counts.strict_orphan, 1,
            "RSK shares the strict-classified parent (parent-level count)"
        );
        assert_eq!(rsk_row.counts.weak_orphan, 0);
        assert_eq!(rsk_row.counts.unknown, 1);

        Ok::<_, anyhow::Error>(())
    })
}
