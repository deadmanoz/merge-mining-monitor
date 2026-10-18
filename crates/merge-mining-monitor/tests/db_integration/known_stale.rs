//! Known-stale membership exclusion gate and retroactive `reclassify-known-stales` demotion.

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

        let again = run_reclassify_known_stales(
            &mut client,
            ReclassifyKnownStalesConfig { batch_size: 10 },
        )
        .await?;
        assert_eq!(again.demoted, 0);

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
async fn import_known_stales_repairs_contaminated_orphan_rows() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (parent, _, _) = btc_400000_orphan_fixture(&client).await?;
        let parent_hash = parent.block_hash().to_byte_array().to_vec();
        let display_hash = parent.block_hash();
        capture_child_event(
            &mut client,
            ChildEvidence::new(
                "known_stale_import_repair",
                SYSCOIN_SOURCE_CODE,
                2_248_410,
                0x5c,
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

        let csv_path = std::env::temp_dir().join(format!(
            "known-stale-import-repair-{}-{display_hash}.csv",
            std::process::id()
        ));
        let result = async {
            std::fs::write(
                &csv_path,
                format!("height,hash,header\n400000,{display_hash},\n"),
            )?;
            let summary = mmm_producers::run_import_known_stales(
                &mut client,
                &mmm_producers::KnownStaleImportConfig {
                    csv_path: csv_path.clone(),
                    source_label: "import-repair-test".to_string(),
                    batch_size: 100,
                    skip_malformed: false,
                },
            )
            .await?;
            assert_eq!(summary.inserted, 1);
            assert_eq!(
                block_kind_and_class(&client, &parent_hash).await?,
                ("unknown".to_string(), Some("excluded".to_string())),
                "membership import repairs classifications made before the membership existed"
            );
            assert_eq!(
                weak_orphan_parents(&client, source_id).await?,
                0,
                "the import's in-transaction repair maintains source health"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = std::fs::remove_file(&csv_path);
        result
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
