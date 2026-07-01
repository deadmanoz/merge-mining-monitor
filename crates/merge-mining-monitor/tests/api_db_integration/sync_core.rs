use std::collections::BTreeSet;

use anyhow::Result;
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use mmm_capture::source_registry::{BITCOIN_SOURCE_CODE, NAMECOIN_SOURCE_CODE};
use mmm_producers::{BitcoinCoreSyncConfig, initialize_follow_state, run_sync_bitcoin_core};
use mmm_store::get_source_id;
use serde_json::json;
use time::Month;
use tokio_postgres::types::Json;

use crate::support::seed::{
    EventSeed, day_epoch, hash_bytes, header_hash_and_prev, header_hash_bytes, insert_block,
    insert_event, test_header_chain,
};

use crate::helpers::FakeBitcoinCoreBackboneSource;

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
