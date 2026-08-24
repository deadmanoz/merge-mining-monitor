//! Known-stale membership: the `compute_block_orphan_class` exclusion gate and
//! the retroactive `reclassify-known-stales` demotion pass.
//!
//! A known stale is absent from Core's active chain by definition, so it passes
//! the reconciler's Core-absence gate and the offline classifier would refine it
//! into strict/weak. These tests prove the membership gate excludes it instead,
//! and that the demotion pass fixes rows contaminated before the membership was
//! imported (the production bug: a catalogued stale served as strict_btc_orphan).

use anyhow::Result;
use bitcoin::hashes::Hash as _;
use mmm_capture::source_registry::SYSCOIN_SOURCE_CODE;
use mmm_read_model::{
    ReclassifyKnownStalesConfig, compute_source_health_from_base,
    reconcile_from_merge_mining_event, run_reclassify_known_stales,
};
use mmm_store::{get_source_id, upsert_known_stale_block};
use tokio_postgres::Client;

use crate::support::scenario::{ChildEvidence, capture_child_event, orphan_candidate_verdict};
use crate::support::{absent_classifier, btc_400000_header, btc_400000_orphan_fixture};

// Real Bitcoin block 400000 header + coinbase scriptSig: genuine BTC PoW and era
// nBits, so the offline classifier renders a real strict/weak verdict (a crafted
// regtest-bits header would classify excluded). The coinbase carries the BIP34
// height, so a syscoin (strict-eligible) parent with it lands strict, and
// without it lands weak.

async fn block_kind_and_class(client: &Client, hash: &[u8]) -> Result<(String, Option<String>)> {
    let row = client
        .query_one(
            "SELECT kind, btc_orphan_class FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await?;
    Ok((row.get(0), row.get(1)))
}

async fn weak_orphan_parents(client: &Client, source_id: i64) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT weak_orphan_parents FROM source_health WHERE source_id = $1",
            &[&source_id],
        )
        .await?
        .get(0))
}

#[tokio::test]
async fn membership_hash_is_excluded_never_strict_or_weak() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (parent, coinbase_script, _) = btc_400000_orphan_fixture(&client).await?;
        let parent_hash = parent.block_hash().to_byte_array().to_vec();

        // Baseline: a strict-eligible syscoin parent with its real BIP34 coinbase,
        // Core-absence attested, classifies as a STRICT orphan while the membership
        // is empty. This is exactly the contamination the production bug produces.
        let event_id = capture_child_event(
            &mut client,
            ChildEvidence::new(
                "known_stale_strict",
                SYSCOIN_SOURCE_CODE,
                2_248_408,
                0x5a,
                parent,
                orphan_candidate_verdict(&parent),
                1_000,
            )
            .with_parent_coinbase_script(coinbase_script),
        )
        .await?;
        assert_eq!(
            block_kind_and_class(&client, &parent_hash).await?,
            ("unknown".to_string(), Some("strict_btc_orphan".to_string())),
            "without membership the fixture is a strict orphan"
        );

        // Register the header as a known stale, then re-run classification: the
        // membership gate must exclude it, never leave it strict/weak.
        assert!(
            upsert_known_stale_block(
                &client,
                &parent_hash,
                Some(400_000),
                "test-stale-blocks@deadbeef",
                1_000,
            )
            .await?,
            "first insert records a new membership row"
        );
        reconcile_from_merge_mining_event(&mut client, event_id, &absent_classifier(&parent), None)
            .await?;
        assert_eq!(
            block_kind_and_class(&client, &parent_hash).await?,
            ("unknown".to_string(), Some("excluded".to_string())),
            "a known-stale hash is excluded, never strict/weak, and stays kind=unknown"
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn reclassify_known_stales_demotes_contaminated_row_and_keeps_source_health_consistent()
-> Result<()> {
    crate::run_mut_db_test!(client, {
        let (parent, _, _) = btc_400000_orphan_fixture(&client).await?;
        let parent_hash = parent.block_hash().to_byte_array().to_vec();

        // Contaminate: capture WITHOUT the coinbase so the syscoin parent lands a
        // WEAK orphan while the membership is still empty.
        capture_child_event(
            &mut client,
            ChildEvidence::new(
                "known_stale_weak",
                SYSCOIN_SOURCE_CODE,
                2_248_409,
                0x5b,
                parent,
                orphan_candidate_verdict(&parent),
                1_000,
            ),
        )
        .await?;
        assert_eq!(
            block_kind_and_class(&client, &parent_hash).await?,
            ("unknown".to_string(), Some("weak_btc_orphan".to_string()))
        );
        let source_id = get_source_id(&client, SYSCOIN_SOURCE_CODE).await?;
        assert_eq!(weak_orphan_parents(&client, source_id).await?, 1);

        // The upstream membership arrives after the contamination; the demotion
        // pass corrects the existing row.
        upsert_known_stale_block(&client, &parent_hash, Some(400_000), "test", 1_000).await?;
        let summary = run_reclassify_known_stales(
            &mut client,
            ReclassifyKnownStalesConfig { batch_size: 10 },
        )
        .await?;
        assert_eq!(summary.membership_size, 1);
        assert_eq!(
            summary.demoted, 1,
            "the contaminated weak row is demoted loudly"
        );
        assert_eq!(
            block_kind_and_class(&client, &parent_hash).await?,
            ("unknown".to_string(), Some("excluded".to_string()))
        );

        // Idempotent: a second pass finds nothing left to demote.
        let again = run_reclassify_known_stales(
            &mut client,
            ReclassifyKnownStalesConfig { batch_size: 10 },
        )
        .await?;
        assert_eq!(again.demoted, 0);

        // source_health stayed consistent: the maintained counter dropped to 0 and
        // matches a fresh recompute from base tables (proving the per-parent diff
        // maintenance, not a global rebuild, kept the counters correct).
        assert_eq!(weak_orphan_parents(&client, source_id).await?, 0);
        let recomputed = compute_source_health_from_base(&client).await?;
        let row = recomputed
            .rows
            .iter()
            .find(|row| row.source_id == source_id)
            .expect("syscoin source_health row");
        assert_eq!(row.weak_orphan_parents, 0);
        assert_eq!(row.strict_orphan_parents, 0);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn import_known_stales_serializes_with_parent_advisory_lock() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let header = btc_400000_header()?;
        let hash = header.block_hash().to_byte_array().to_vec();
        let csv_path = std::env::temp_dir().join(format!(
            "known-stale-lock-{}-{}.csv",
            std::process::id(),
            header.block_hash()
        ));
        std::fs::write(
            &csv_path,
            format!("height,hash,header\n400000,{},\n", header.block_hash()),
        )?;

        let result = async {
            // Simulate a producer holding this parent's advisory lock mid-
            // classification: the import of the same hash must block until the
            // producer's transaction ends, so the documented import-then-
            // reclassify sequence can never race an in-flight strict/weak
            // classification.
            let txn = client.transaction().await?;
            mmm_read_model::lock_block_hash(&txn, &hash).await?;

            let mut import_client = crate::support::db::connect_to_schema(&schema).await?;
            let config = mmm_producers::KnownStaleImportConfig {
                csv_path: csv_path.clone(),
                source_label: "race-test".to_string(),
                batch_size: 100,
                skip_malformed: false,
            };
            let import = tokio::spawn(async move {
                mmm_producers::run_import_known_stales(&mut import_client, &config).await
            });

            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            assert!(
                !import.is_finished(),
                "import must wait for the parent advisory lock held by a classification txn"
            );

            txn.rollback().await?;
            let summary = import.await??;
            assert_eq!(summary.inserted, 1);
            assert!(mmm_store::is_known_stale_hash(&client, &hash).await?);
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let _ = std::fs::remove_file(&csv_path);
        result
    })
}

#[tokio::test]
async fn import_known_stales_rejects_mixed_csv_unless_opted_in() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = btc_400000_header()?;
        let csv_path = std::env::temp_dir().join(format!(
            "known-stale-mixed-{}-{}.csv",
            std::process::id(),
            header.block_hash()
        ));
        std::fs::write(
            &csv_path,
            format!(
                "height,hash,header\n400000,{},\n400001,not-a-hash,\n",
                header.block_hash()
            ),
        )?;

        let result = async {
            let strict = mmm_producers::KnownStaleImportConfig {
                csv_path: csv_path.clone(),
                source_label: "mixed-test".to_string(),
                batch_size: 100,
                skip_malformed: false,
            };
            let err = mmm_producers::run_import_known_stales(&mut client, &strict)
                .await
                .expect_err("a malformed row must fail the import by default");
            assert!(
                err.to_string().contains("malformed"),
                "unexpected error: {err:#}"
            );
            let hash = header.block_hash().to_byte_array().to_vec();
            assert!(
                !mmm_store::is_known_stale_hash(&client, &hash).await?,
                "strict failure must record nothing"
            );

            let lenient = mmm_producers::KnownStaleImportConfig {
                skip_malformed: true,
                ..strict
            };
            let summary = mmm_producers::run_import_known_stales(&mut client, &lenient).await?;
            assert_eq!(summary.inserted, 1);
            assert_eq!(summary.skipped, 1);
            assert!(mmm_store::is_known_stale_hash(&client, &hash).await?);
            Ok::<_, anyhow::Error>(())
        }
        .await;

        let _ = std::fs::remove_file(&csv_path);
        result
    })
}
