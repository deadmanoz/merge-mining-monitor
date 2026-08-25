use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::serialize;
use bitcoin::hashes::{Hash as _, sha256, sha256d};
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, FakeParentClassifier, FakeParentClassifierGate,
};
use mmm_producers::{
    HistoricalImportAllConfig, HistoricalImportConfig, enqueue_published_stale_branches_for_test,
    run_historical_import, run_historical_import_configs_for_test,
    run_manifest_historical_import_for_test,
};
use mmm_read_model::{
    clear_authoritative_historical_provenance_in_transaction, drain_historical_reconcile_queue,
    drain_historical_reconcile_queue_with_budget_for_test, enqueue_historical_parent_reconcile,
    rebuild_source_health, reconcile_authoritative_historical_source_in_transaction,
};
use mmm_store::get_source_id;

use crate::support::scenario::{
    canonical_verdict, stale_verdict_with_competitor_header, unknown_verdict,
};
use crate::support::seed::insert_block;
use crate::support::{
    absent_classifier, btc_400000_coinbase_script, btc_400000_header, btc_400000_orphan_fixture,
    header_meeting_bits,
};

const NORMALIZED_HEADER: &str = "chain,source_kind,source_path,source_row_number,artifact_scope,provenance,child_height,child_block_hash,child_header_hex,child_block_time,child_nbits,btc_height,btc_header_hash,btc_prev_hash,btc_time,btc_bits,btc_nonce,btc_header_hex,coinbase_scriptsig_hex,coinbase_outputs,full_coinbase_hex,classification,validation_status,expected_nbits,rejection_reason,btc_stale_relevance,relevance_reason\n";
const GENESIS_COINBASE_SCRIPT: &str = "04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73";

fn pinned_publication_ref() -> &'static str {
    static PIN: LazyLock<String> = LazyLock::new(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/historical/historical-source-manifest.json");
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(path).expect("read committed historical source manifest"),
        )
        .expect("parse committed historical source manifest")["source_repo_commit"]
            .as_str()
            .expect("committed historical source manifest has source_repo_commit")
            .to_owned()
    });
    PIN.as_str()
}
const GENESIS_COINBASE: &str = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000";

#[tokio::test]
async fn manifest_backed_import_verifies_artifacts_before_writes() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_000, 1);
        let fixture = write_manifest_fixture(&header)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_000),
            ));
            let summary = run_manifest_historical_import_for_test(
                &mut client,
                &classifier,
                &fixture.config,
                "devcoin",
            )
            .await?;
            assert_eq!(summary.ingested, 1);

            let source_id = get_source_id(&client, "auxpow:devcoin").await?;
            assert_eq!(source_event_count(&client, source_id).await?, 1);

            let mut tampered = std::fs::read(&fixture.artifact_path)?;
            let chain_start = tampered
                .windows(b"devcoin".len())
                .position(|window| window == b"devcoin")
                .context("fixture contains devcoin row")?;
            tampered[chain_start] = b'D';
            std::fs::write(&fixture.artifact_path, tampered)?;
            let checksum_error = run_manifest_historical_import_for_test(
                &mut client,
                &classifier,
                &fixture.config,
                "devcoin",
            )
            .await
            .expect_err("tampered artifact must fail before mutation");
            assert!(checksum_error.to_string().contains("checksum mismatch"));
            assert_eq!(source_event_count(&client, source_id).await?, 1);

            std::fs::write(
                &fixture.artifact_path,
                "version https://git-lfs.github.com/spec/v1\noid sha256:00\nsize 1\n",
            )?;
            let lfs_error = run_manifest_historical_import_for_test(
                &mut client,
                &classifier,
                &fixture.config,
                "devcoin",
            )
            .await
            .expect_err("Git LFS pointer must fail before mutation");
            assert!(lfs_error.to_string().contains("Git LFS pointer"));
            assert_eq!(source_event_count(&client, source_id).await?, 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        result
    })
}

#[tokio::test]
async fn zero_import_limit_fails_before_any_database_write() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_060, 60);
        let csv_path =
            write_normalized_csv(&header, "canonical", "", "canonical_parent", &[], 700_060)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_060),
            ));
            let mut config = devcoin_import_config(&csv_path);
            config.limit = Some(0);

            let error = run_historical_import(&mut client, &classifier, &config)
                .await
                .expect_err("zero limit must be rejected before mutation");
            assert!(
                error
                    .to_string()
                    .contains("--limit must be greater than zero")
            );
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                0
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn import_dataset_persists_and_replays_a_normalized_canonical_row() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_001, 2);
        let csv_path =
            write_normalized_csv(&header, "canonical", "", "canonical_parent", &[], 700_001)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_001),
            ));
            let config = devcoin_import_config(&csv_path);

            for _ in 0..2 {
                let summary = run_historical_import(&mut client, &classifier, &config).await?;
                assert_eq!(summary.expected_rows, 1);
                assert_eq!(summary.rows_seen, 1);
                assert_eq!(summary.candidates, 1);
                assert_eq!(summary.ingested, 1);
                assert_eq!(summary.canonical, 1);
                assert!(summary.skipped.is_empty());
            }

            let source_id = get_source_id(&client, "auxpow:devcoin").await?;
            let event_count: i64 = client
                .query_one(
                    "SELECT count(*) FROM merge_mining_event WHERE source_id = $1",
                    &[&source_id],
                )
                .await?
                .get(0);
            assert_eq!(event_count, 1);
            let provenance = client
                .query_one(
                    "SELECT classification, btc_height, relevance_reason \
                     FROM historical_event_provenance",
                    &[],
                )
                .await?;
            assert_eq!(provenance.get::<_, String>(0), "canonical");
            assert_eq!(provenance.get::<_, Option<i32>>(1), Some(700_001));
            assert_eq!(
                provenance.get::<_, Option<String>>(2).as_deref(),
                Some("canonical_parent")
            );
            let queued: i64 = client
                .query_one("SELECT count(*) FROM historical_reconcile_queue", &[])
                .await?
                .get(0);
            assert_eq!(queued, 0, "successful imports fully drain durable work");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn operator_csv_retains_per_row_skip_accounting() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_037, 37);
        let csv_path = temp_csv_path()?;
        let child_header = serialize(&header);
        let valid = normalized_csv_line_with_child_header(
            &header,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 1,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_037,
                child_height: 12,
                child_hash: None,
            },
            &child_header,
        );
        let mut invalid_fields = valid
            .trim_end()
            .split(',')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        invalid_fields[3] = "2".to_owned();
        invalid_fields[7] = "11".repeat(32);
        invalid_fields[8] = hex::encode(serialize(&header));
        let invalid = format!("{}\n", invalid_fields.join(","));
        let mut near_fields = valid
            .trim_end()
            .split(',')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        near_fields[3] = "3".to_owned();
        near_fields[6] = "13".to_owned();
        near_fields[21] = "near".to_owned();
        near_fields[22].clear();
        near_fields[26].clear();
        let near = format!("{}\n", near_fields.join(","));
        std::fs::write(
            &csv_path,
            format!("{NORMALIZED_HEADER}{valid}{invalid}{near}"),
        )?;

        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_037),
            ));
            let summary =
                run_historical_import(&mut client, &classifier, &devcoin_import_config(&csv_path))
                    .await?;

            assert_eq!(summary.rows_seen, 3);
            assert_eq!(summary.ingested, 1);
            assert_eq!(summary.skipped.get("hash_mismatch"), Some(&1));
            assert_eq!(summary.skipped.get("near"), Some(&1));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn import_persists_lossless_parent_coinbase_evidence() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_042, 42);
        let coinbase: bitcoin::Transaction =
            bitcoin::consensus::deserialize(&hex::decode(GENESIS_COINBASE)?)?;
        let genesis_output = coinbase.output.first().context("genesis output")?;
        let output_text = format!(
            "{}:{}",
            hex::encode(genesis_output.script_pubkey.as_bytes()),
            genesis_output.value.to_sat()
        );
        let csv_path = write_normalized_csv_with_parent_coinbase(
            &header,
            GENESIS_COINBASE_SCRIPT,
            &output_text,
            GENESIS_COINBASE,
            700_042,
        )?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_042),
            ));
            run_historical_import(&mut client, &classifier, &devcoin_import_config(&csv_path))
                .await?;

            let row = client
                .query_one(
                    "SELECT btc_parent_coinbase_script, btc_parent_coinbase_outputs, \
                            btc_parent_coinbase_outputs_text, btc_parent_coinbase_tx_bytes \
                     FROM merge_mining_event",
                    &[],
                )
                .await?;
            assert_eq!(
                row.get::<_, Option<Vec<u8>>>(0),
                Some(hex::decode(GENESIS_COINBASE_SCRIPT)?)
            );
            assert!(row.get::<_, Option<Vec<u8>>>(1).is_some());
            assert_eq!(
                row.get::<_, Option<String>>(2).as_deref(),
                Some(output_text.as_str())
            );
            assert_eq!(
                row.get::<_, Option<Vec<u8>>>(3),
                Some(hex::decode(GENESIS_COINBASE)?)
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn import_summary_reports_partial_to_exact_promotion() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_031, 31);
        let child_hash = vec![0x77; 32];
        let csv_path = write_normalized_csv_row(
            &header,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 1,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_031,
                child_height: 12,
                child_hash: Some(&child_hash),
            },
        )?;
        let result = async {
            seed_identity_event(&client, "auxpow:devcoin", &header, 12, None).await?;
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_031),
            ));

            let summary =
                run_historical_import(&mut client, &classifier, &devcoin_import_config(&csv_path))
                    .await?;

            assert_eq!(summary.promoted, 1);
            assert_eq!(summary.inserted, 0);
            assert_eq!(summary.updated, 0);
            assert_eq!(summary.satisfied_by_existing_exact, 0);
            let stored_hash: Option<Vec<u8>> = client
                .query_one(
                    "SELECT child_block_hash FROM merge_mining_event \
                     WHERE source_id = (SELECT id FROM source WHERE code = 'auxpow:devcoin')",
                    &[],
                )
                .await?
                .get(0);
            assert_eq!(stored_hash, Some(child_hash));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn import_summary_reports_partial_satisfied_by_existing_exact() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_032, 32);
        let csv_path =
            write_normalized_csv(&header, "canonical", "", "canonical_parent", &[], 700_032)?;
        let result = async {
            seed_identity_event(&client, "auxpow:devcoin", &header, 12, Some(vec![0x78; 32]))
                .await?;
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_032),
            ));

            let summary =
                run_historical_import(&mut client, &classifier, &devcoin_import_config(&csv_path))
                    .await?;

            assert_eq!(summary.satisfied_by_existing_exact, 1);
            assert_eq!(summary.inserted, 0);
            assert_eq!(summary.updated, 0);
            assert_eq!(summary.promoted, 0);
            let event_count: i64 = client
                .query_one(
                    "SELECT count(*) FROM merge_mining_event \
                     WHERE source_id = (SELECT id FROM source WHERE code = 'auxpow:devcoin')",
                    &[],
                )
                .await?
                .get(0);
            assert_eq!(event_count, 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn live_child_hash_byte_order_matches_historical_identity() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_039, 39);
        let child_header = serialize(&header);
        let child_hash = sha256d::Hash::hash(&child_header).to_byte_array().to_vec();
        let csv_path = temp_csv_path()?;
        let row = normalized_csv_line_with_child_header(
            &header,
            &NormalizedCsvRow {
                chain: "namecoin",
                source_row_number: 1,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_039,
                child_height: 12,
                child_hash: Some(&child_hash),
            },
            &child_header,
        );
        std::fs::write(&csv_path, format!("{NORMALIZED_HEADER}{row}"))?;

        let result = async {
            let live_event = seed_identity_event(
                &client,
                "auxpow:namecoin",
                &header,
                12,
                Some(child_hash.clone()),
            )
            .await?;
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_039),
            ));
            let mut config = HistoricalImportConfig::for_csv("namecoin", &csv_path);
            config.allow_empty_known_stales = true;

            let summary = run_historical_import(&mut client, &classifier, &config).await?;

            assert_eq!(summary.updated, 1);
            assert_eq!(summary.inserted, 0);
            assert_eq!(
                active_source_event_count(&client, "auxpow:namecoin").await?,
                1
            );
            let stored = client
                .query_one(
                    "SELECT id, child_block_hash, child_header_bytes \
                     FROM merge_mining_event \
                     WHERE source_id = (SELECT id FROM source WHERE code = 'auxpow:namecoin')",
                    &[],
                )
                .await?;
            assert_eq!(stored.get::<_, i64>(0), live_event);
            assert_eq!(stored.get::<_, Option<Vec<u8>>>(1), Some(child_hash));
            assert_eq!(stored.get::<_, Option<Vec<u8>>>(2), Some(child_header));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn multi_chain_import_orders_sources_and_reconciles_stale_branch() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_033, 33);
        let competitor = header_meeting_bits(0x207f_ffff, 1_700_000_034, 34);
        let competitor_hash = competitor.block_hash().to_byte_array().to_vec();
        let ixcoin_path = write_normalized_csv_for_chain(
            "ixcoin",
            &header,
            "unknown",
            "",
            "valid_direct_stale",
            &[],
            700_033,
        )?;
        let devcoin_path = write_normalized_csv_for_chain(
            "devcoin",
            &header,
            "unknown",
            "",
            "valid_direct_stale",
            &[],
            700_033,
        )?;
        let result = async {
            let classifier =
                ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
                    unknown_verdict(&header),
                    stale_verdict_with_competitor_header(
                        &header,
                        700_033,
                        competitor,
                        competitor_hash,
                    ),
                ]));
            let summary = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                two_chain_import_configs(&ixcoin_path, &devcoin_path),
            )
            .await?;

            assert_eq!(
                summary
                    .chains
                    .iter()
                    .map(|(chain, _)| chain.as_str())
                    .collect::<Vec<_>>(),
                vec!["devcoin", "ixcoin"]
            );
            assert_eq!(summary.stale_branches_reconciled, 1);
            let kind: String = client
                .query_one(
                    "SELECT kind FROM block WHERE btc_header_hash = $1",
                    &[&header.block_hash().to_byte_array().to_vec()],
                )
                .await?
                .get(0);
            assert_eq!(kind, "stale");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&ixcoin_path, &devcoin_path])
    })
}

#[tokio::test]
async fn targeted_stale_reconcile_retains_committed_cascade_seeds() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_061, 61);
        let parent_hash = header.block_hash().to_byte_array().to_vec();
        let competitor = header_meeting_bits(0x207f_ffff, 1_700_000_062, 62);
        let competitor_hash = competitor.block_hash().to_byte_array().to_vec();
        let csv_path =
            write_normalized_csv(&header, "unknown", "", "valid_direct_stale", &[], 700_061)?;
        let result = async {
            let initial = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                unknown_verdict(&header),
            ));
            run_historical_import(&mut client, &initial, &devcoin_import_config(&csv_path)).await?;

            let dependent_hash = vec![0x63; 32];
            insert_block(
                &client,
                &dependent_hash,
                &parent_hash,
                None,
                "unknown",
                1_700_000_063,
                None,
            )
            .await?;
            assert_eq!(
                enqueue_published_stale_branches_for_test(&mut client).await?,
                1
            );

            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                stale_verdict_with_competitor_header(&header, 700_061, competitor, competitor_hash),
            ));
            let classifications = std::collections::HashMap::new();
            drain_historical_reconcile_queue_with_budget_for_test(
                &mut client,
                &classifier,
                &classifications,
                0,
            )
            .await
            .expect_err("zero cascade budget must interrupt targeted dependent work");

            let queued: (bool, Vec<Vec<u8>>) = client
                .query_one(
                    "SELECT primary_pending, changed_hashes \
                     FROM historical_reconcile_queue \
                     WHERE btc_parent_header_hash = $1",
                    &[&parent_hash],
                )
                .await
                .map(|row| (row.get(0), row.get(1)))?;
            assert!(
                !queued.0,
                "the primary promotion committed before the failure"
            );
            assert!(queued.1.contains(&parent_hash));
            assert_eq!(
                block_kind_and_orphan_class(&client, &header).await?.0,
                "stale"
            );

            drain_historical_reconcile_queue(&mut client, &classifier, &classifications).await?;
            let remaining: i64 = client
                .query_one("SELECT count(*) FROM historical_reconcile_queue", &[])
                .await?
                .get(0);
            assert_eq!(remaining, 0);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[path = "historical_ingest/queue_concurrency.rs"]
mod queue_concurrency;

#[tokio::test]
async fn historical_preflight_refills_classifier_concurrency() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let first = header_meeting_bits(0x207f_ffff, 1_700_000_037, 37);
        let second = header_meeting_bits(0x207f_ffff, 1_700_000_038, 38);
        let third = header_meeting_bits(0x207f_ffff, 1_700_000_039, 39);
        let csv_path = write_repeated_then_unique_canonical_rows(&first, &second, &third, 700_037)?;
        let result = async {
            let gate = FakeParentClassifierGate::new();
            let fake = FakeParentClassifier::new(canonical_verdict(&first, 700_037))
                .with_first_call_gate(gate.clone())
                .with_max_concurrency(2);
            let classifier = ConfiguredParentClassifier::Fake(fake.clone());
            let config = devcoin_import_config(&csv_path);
            let mut import = Box::pin(run_historical_import(&mut client, &classifier, &config));

            tokio::select! {
                result = &mut import => {
                    result.context("import completed before the classifier gate")?;
                    anyhow::bail!("import completed before the classifier gate");
                }
                result = tokio::time::timeout(Duration::from_secs(5), gate.wait_started()) => {
                    result.context("first classification did not reach the gate")?;
                }
            }

            let later_completed = async {
                loop {
                    if fake.call_count().await == 2 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            };
            tokio::select! {
                result = &mut import => {
                    result.context("import completed while the first classification was gated")?;
                    anyhow::bail!("import completed while the first classification was gated");
                }
                result = tokio::time::timeout(Duration::from_secs(5), later_completed) => {
                    result.context("classification slots were not refilled while the first was gated")?;
                }
            }

            gate.proceed();
            let summary = import.await?;
            assert_eq!(summary.ingested, 4);
            assert_eq!(
                fake.call_count().await,
                5,
                "three preflight classifications plus two barrier-protected rechecks of the \
                 fake's incompatible same-height canonical verdicts"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn historical_preflight_stops_at_first_invalid_publication_row() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let headers = (0..20)
            .map(|offset| header_meeting_bits(0x207f_ffff, 1_700_000_100 + offset, 100 + offset))
            .collect::<Vec<_>>();
        let rows = headers
            .iter()
            .enumerate()
            .map(|(index, header)| {
                normalized_csv_line(
                    header,
                    &NormalizedCsvRow {
                        chain: "devcoin",
                        source_row_number: i64::try_from(index + 1).expect("fixture row fits i64"),
                        classification: "canonical",
                        relevance: "",
                        relevance_reason: "canonical_parent",
                        coinbase_script: &[],
                        btc_height: 700_100 + i32::try_from(index).expect("fixture index fits i32"),
                        child_height: 100 + i32::try_from(index).expect("fixture index fits i32"),
                        child_hash: None,
                    },
                )
            })
            .collect::<Vec<_>>();
        let fixture = write_manifest_fixture_rows(&rows)?;
        let result = async {
            let fake =
                FakeParentClassifier::new(unknown_verdict(&headers[0])).with_max_concurrency(2);
            let classifier = ConfiguredParentClassifier::Fake(fake.clone());
            let error = run_manifest_historical_import_for_test(
                &mut client,
                &classifier,
                &fixture.config,
                "devcoin",
            )
            .await
            .expect_err("an invalid canonical publication row must fail preflight");
            assert!(error.to_string().contains("row 2 would be skipped"));
            assert!(
                fake.call_count().await < u64::try_from(rows.len())?,
                "preflight must not classify the complete artifact before rejecting its first row"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        result
    })
}

#[tokio::test]
async fn refining_publication_rows_share_one_event_and_keep_both_provenance_rows() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_035, 35);
        let csv_path = write_refining_rows(&header, 700_035)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_035),
            ));

            let summary =
                run_historical_import(&mut client, &classifier, &devcoin_import_config(&csv_path))
                    .await?;

            assert_eq!(summary.ingested, 2);
            assert_eq!(summary.inserted, 1);
            assert_eq!(summary.promoted, 1);
            let event_count: i64 = client
                .query_one("SELECT count(*) FROM merge_mining_event", &[])
                .await?
                .get(0);
            let provenance_rows: Vec<i64> = client
                .query(
                    "SELECT source_row_number FROM historical_event_provenance \
                     ORDER BY source_row_number",
                    &[],
                )
                .await?
                .into_iter()
                .map(|row| row.get(0))
                .collect();
            assert_eq!(event_count, 1);
            assert_eq!(provenance_rows, vec![1, 2]);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn import_refuses_without_known_stale_membership() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_000, 1);
        let csv_path = write_normalized_csv(&header, "canonical", "", "canonical_parent", &[], 1)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 1),
            ));
            let mut config = devcoin_import_config(&csv_path);
            config.allow_empty_known_stales = false;
            let error = run_historical_import(&mut client, &classifier, &config)
                .await
                .expect_err("empty known-stale membership must fail closed");
            assert!(error.to_string().contains("known_stale_block is empty"));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn import_dataset_persists_core_attested_strict_or_weak_unknown() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (header, coinbase_script, relevance) = btc_400000_orphan_fixture(&client).await?;
        let csv_path = write_normalized_csv(
            &header,
            "unknown",
            relevance,
            "published_orphan_verdict",
            &coinbase_script,
            400_000,
        )?;
        let result = async {
            let config = devcoin_import_config(&csv_path);
            let summary =
                run_historical_import(&mut client, &absent_classifier(&header), &config).await?;
            assert_eq!(summary.ingested, 1);
            assert_eq!(summary.strict_orphans + summary.weak_orphans, 1);

            let (kind, orphan_class) = block_kind_and_orphan_class(&client, &header).await?;
            assert_eq!(kind, "unknown");
            assert_eq!(orphan_class.as_deref(), Some(relevance));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn raw_unknown_descendant_attestation_becomes_stale_only_when_core_places_it() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let stale_header = header_meeting_bits(0x207f_ffff, 1_700_000_002, 12);
        let competitor = header_meeting_bits(0x207f_ffff, 1_700_000_003, 13);
        let competitor_hash = competitor.block_hash().to_byte_array().to_vec();
        let csv_path = write_normalized_csv(
            &stale_header,
            "unknown",
            "",
            "valid_stale_descendant",
            &[],
            700_002,
        )?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                stale_verdict_with_competitor_header(
                    &stale_header,
                    700_002,
                    competitor,
                    competitor_hash.clone(),
                ),
            ));
            let config = devcoin_import_config(&csv_path);
            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_eq!(summary.ingested, 1);
            assert_eq!(summary.stale, 1);
            assert_eq!(summary.known_descendant_branch_attestations, 1);

            let block = client
                .query_one(
                    "SELECT kind, canonical_competitor_hash FROM block \
                     WHERE btc_header_hash = $1",
                    &[&stale_header.block_hash().to_byte_array().to_vec()],
                )
                .await?;
            assert_eq!(block.get::<_, String>(0), "stale");
            assert_eq!(block.get::<_, Option<Vec<u8>>>(1), Some(competitor_hash));
            let source_classification: String = client
                .query_one(
                    "SELECT classification FROM historical_event_provenance",
                    &[],
                )
                .await?
                .get(0);
            assert_eq!(source_classification, "unknown");
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn known_branch_attestation_is_not_reinterpreted_as_an_orphan() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = btc_400000_header()?;
        let csv_path = write_normalized_csv(
            &header,
            "unknown",
            "",
            "valid_direct_stale",
            &btc_400000_coinbase_script()?,
            400_000,
        )?;
        let result = async {
            let config = devcoin_import_config(&csv_path);
            let summary =
                run_historical_import(&mut client, &absent_classifier(&header), &config).await?;
            assert_eq!(summary.ingested, 1);
            assert!(summary.skipped.is_empty());
            let (kind, orphan_class) = block_kind_and_orphan_class(&client, &header).await?;
            assert_eq!(kind, "unknown");
            assert_eq!(orphan_class.as_deref(), Some("excluded"));
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn authoritative_import_removes_rows_absent_from_the_snapshot() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_010, 21);
        let row = normalized_csv_line(
            &header,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 1,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_010,
                child_height: 12,
                child_hash: None,
            },
        );
        let fixture = write_manifest_fixture_rows(&[row])?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_010),
            ));
            run_manifest_historical_import_for_test(
                &mut client,
                &classifier,
                &fixture.config,
                "devcoin",
            )
            .await?;
            let omitted_event =
                seed_unpublished_event(&client, "auxpow:devcoin", 99, vec![0x44; 32]).await?;
            client
                .execute(
                    "INSERT INTO event_pool_attribution ( \
                        event_id, side, namespace, match_kind, matched_value, source, \
                        confidence, first_seen_at, last_seen_at \
                     ) VALUES ($1, 'btc_parent', 'btc_coinbase_tag', 'test_seed', \
                        'omitted-attribution', 'test_seed', 'high', 1, 1)",
                    &[&omitted_event],
                )
                .await?;

            let summary = run_manifest_historical_import_for_test(
                &mut client,
                &classifier,
                &fixture.config,
                "devcoin",
            )
            .await?;
            assert_eq!(summary.updated, 1);
            assert_eq!(summary.removed, 1);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            let attribution_count: i64 = client
                .query_one(
                    "SELECT count(*) FROM event_pool_attribution WHERE event_id = $1",
                    &[&omitted_event],
                )
                .await?
                .get(0);
            assert_eq!(attribution_count, 0);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        result
    })
}

#[tokio::test]
async fn authoritative_reimport_replaces_superseded_publication_provenance() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let retained = header_meeting_bits(0x207f_ffff, 1_700_000_040, 40);
        let omitted = header_meeting_bits(0x207f_ffff, 1_700_000_041, 41);
        let retained_row = normalized_csv_line(
            &retained,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 1,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_040,
                child_height: 40,
                child_hash: None,
            },
        );
        let omitted_row = normalized_csv_line(
            &omitted,
            &NormalizedCsvRow {
                chain: "devcoin",
                source_row_number: 2,
                classification: "canonical",
                relevance: "",
                relevance_reason: "canonical_parent",
                coinbase_script: &[],
                btc_height: 700_041,
                child_height: 41,
                child_hash: None,
            },
        );
        let first_fixture = write_manifest_fixture_rows(&[retained_row.clone(), omitted_row])?;
        let second_fixture = write_manifest_fixture_rows(&[retained_row])?;
        let result = async {
            let first_classifier =
                ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
                    canonical_verdict(&retained, 700_040),
                    canonical_verdict(&omitted, 700_041),
                ]));
            run_manifest_historical_import_for_test(
                &mut client,
                &first_classifier,
                &first_fixture.config,
                "devcoin",
            )
            .await?;
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                2
            );
            client
                .execute(
                    "UPDATE historical_event_provenance \
                     SET publication_ref = '08da16532a55240e54c4051d5d324a0484b80b1c' \
                     WHERE chain = 'devcoin' \
                       AND publication_ref <> 'operator-csv'",
                    &[],
                )
                .await?;

            let second_classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&retained, 700_040),
            ));
            let summary = run_manifest_historical_import_for_test(
                &mut client,
                &second_classifier,
                &second_fixture.config,
                "devcoin",
            )
            .await?;

            assert_eq!(summary.removed, 1);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            let publication_ref: String = client
                .query_one(
                    "SELECT publication_ref \
                     FROM historical_event_provenance WHERE chain = 'devcoin'",
                    &[],
                )
                .await?
                .get(0);
            assert_eq!(publication_ref, pinned_publication_ref());
            Ok::<_, anyhow::Error>(())
        }
        .await;
        for fixture in [&first_fixture, &second_fixture] {
            std::fs::remove_dir_all(&fixture.root)
                .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        }
        result
    })
}

#[tokio::test]
async fn authoritative_snapshot_reconciliation_preserves_error_observation_provenance() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_044, 44);
        let csv_path =
            write_normalized_csv(&header, "canonical", "", "canonical_parent", &[], 700_044)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_044),
            ));
            run_historical_import(&mut client, &classifier, &devcoin_import_config(&csv_path))
                .await?;
            client
                .execute(
                    "UPDATE historical_event_provenance \
                     SET publication_ref = $1, \
                         artifact_scope = 'error-block-observations', \
                         classification = 'error_block' \
                     WHERE chain = 'devcoin'",
                    &[&pinned_publication_ref()],
                )
                .await?;

            let transaction = client.transaction().await?;
            clear_authoritative_historical_provenance_in_transaction(&transaction, "devcoin")
                .await?;
            transaction.commit().await?;
            let remaining: i64 = client
                .query_one(
                    "SELECT count(*) FROM historical_event_provenance \
                     WHERE artifact_scope = 'error-block-observations'",
                    &[],
                )
                .await?
                .get(0);
            assert_eq!(remaining, 1);

            let source_id = get_source_id(&client, "auxpow:devcoin").await?;
            let transaction = client.transaction().await?;
            let removed = reconcile_authoritative_historical_source_in_transaction(
                &transaction,
                source_id,
                pinned_publication_ref(),
                "devcoin",
            )
            .await?;
            transaction.commit().await?;
            assert_eq!(removed, 0);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn operator_csv_is_additive_for_historical_sources() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_043, 43);
        let csv_path =
            write_normalized_csv(&header, "canonical", "", "canonical_parent", &[], 700_000)?;
        let fixture = write_manifest_fixture(&header)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_000),
            ));
            let config = devcoin_import_config(&csv_path);
            run_historical_import(&mut client, &classifier, &config).await?;
            seed_unpublished_event(&client, "auxpow:devcoin", 99, vec![0x45; 32]).await?;

            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_eq!(summary.removed, 0);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                2
            );
            let publication_ref: String = client
                .query_one(
                    "SELECT publication_ref FROM historical_event_provenance \
                     WHERE chain = 'devcoin'",
                    &[],
                )
                .await?
                .get(0);
            assert_eq!(publication_ref, "operator-csv");

            run_manifest_historical_import_for_test(
                &mut client,
                &classifier,
                &fixture.config,
                "devcoin",
            )
            .await?;
            let publication_refs = client
                .query_one(
                    "SELECT array_agg(publication_ref ORDER BY publication_ref) \
                     FROM historical_event_provenance WHERE chain = 'devcoin'",
                    &[],
                )
                .await?
                .get::<_, Vec<String>>(0);
            assert_eq!(
                publication_refs,
                vec![
                    pinned_publication_ref().to_owned(),
                    "operator-csv".to_owned(),
                ]
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&fixture.root)
            .with_context(|| format!("remove fixture root {}", fixture.root.display()))?;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn live_import_is_additive_and_never_removes_live_events() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_011, 22);
        let csv_path = write_normalized_csv_for_chain(
            "namecoin",
            &header,
            "canonical",
            "",
            "canonical_parent",
            &[],
            700_011,
        )?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_011),
            ));
            let mut config = HistoricalImportConfig::for_csv("namecoin", &csv_path);
            config.allow_empty_known_stales = true;
            run_historical_import(&mut client, &classifier, &config).await?;
            seed_unpublished_event(&client, "auxpow:namecoin", 99, vec![0x55; 32]).await?;

            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_eq!(summary.removed, 0);
            assert_eq!(
                active_source_event_count(&client, "auxpow:namecoin").await?,
                2
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn failed_chain_import_rolls_back_every_row() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_012, 23);
        let csv_path = write_contradictory_exact_rows("devcoin", &header, 700_012)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_012),
            ));
            let config = devcoin_import_config(&csv_path);
            let error = run_historical_import(&mut client, &classifier, &config)
                .await
                .expect_err("contradictory second row must fail the chain");
            assert!(error.to_string().contains("capture historical parent"));
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                0
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

#[tokio::test]
async fn interrupted_multi_chain_import_resumes_safely() -> Result<()> {
    crate::run_mut_db_test!(client, {
        rebuild_source_health(&mut client).await?;
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_038, 38);
        let devcoin_path = write_normalized_csv_for_chain(
            "devcoin",
            &header,
            "canonical",
            "",
            "canonical_parent",
            &[],
            700_038,
        )?;
        let ixcoin_path = write_contradictory_exact_rows("ixcoin", &header, 700_038)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_038),
            ));
            let first_error = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                two_chain_import_configs(&ixcoin_path, &devcoin_path),
            )
            .await
            .expect_err("the second chain must roll back");
            assert!(
                first_error
                    .to_string()
                    .contains("capture historical parent")
            );
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            assert_eq!(
                active_source_event_count(&client, "auxpow:ixcoin").await?,
                0
            );
            let source_health_ready: bool = client
                .query_one(
                    "SELECT source_health_ready FROM read_model_invariant WHERE id = TRUE",
                    &[],
                )
                .await?
                .get(0);
            assert!(
                !source_health_ready,
                "a partial multi-chain import must fail closed"
            );

            let corrected = normalized_csv_line(
                &header,
                &NormalizedCsvRow {
                    chain: "ixcoin",
                    source_row_number: 1,
                    classification: "canonical",
                    relevance: "",
                    relevance_reason: "canonical_parent",
                    coinbase_script: &[],
                    btc_height: 700_038,
                    child_height: 12,
                    child_hash: None,
                },
            );
            std::fs::write(&ixcoin_path, format!("{NORMALIZED_HEADER}{corrected}"))?;

            let summary = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                two_chain_import_configs(&ixcoin_path, &devcoin_path),
            )
            .await?;
            assert_eq!(summary.chains.len(), 2);
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            assert_eq!(
                active_source_event_count(&client, "auxpow:ixcoin").await?,
                1
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&ixcoin_path, &devcoin_path])
    })
}

#[tokio::test]
async fn surveyed_zero_row_artifact_is_a_database_noop() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let csv_path = temp_csv_path()?;
        std::fs::write(&csv_path, NORMALIZED_HEADER)?;
        let result = async {
            let config = HistoricalImportConfig::for_csv("doichain", &csv_path);
            let summary =
                run_historical_import(&mut client, &ConfiguredParentClassifier::Disabled, &config)
                    .await?;
            assert_eq!(summary.expected_rows, 0);
            assert_eq!(summary.ingested, 0);
            assert_eq!(
                active_source_event_count(&client, "auxpow:doichain").await?,
                0
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&csv_path])
    })
}

fn devcoin_import_config(csv_path: &Path) -> HistoricalImportConfig {
    let mut config = HistoricalImportConfig::for_csv("devcoin", csv_path);
    config.batch_size = 10;
    config.allow_empty_known_stales = true;
    config
}

struct ManifestFixture {
    root: PathBuf,
    artifact_path: PathBuf,
    config: HistoricalImportAllConfig,
}

fn write_manifest_fixture(header: &Header) -> Result<ManifestFixture> {
    let row = normalized_csv_line(
        header,
        &NormalizedCsvRow {
            chain: "devcoin",
            source_row_number: 1,
            classification: "canonical",
            relevance: "",
            relevance_reason: "canonical_parent",
            coinbase_script: &[],
            btc_height: 700_000,
            child_height: 12,
            child_hash: None,
        },
    );
    write_manifest_fixture_rows(&[row])
}

fn write_manifest_fixture_rows(rows: &[String]) -> Result<ManifestFixture> {
    let row_count = u64::try_from(rows.len()).context("fixture row count exceeds u64")?;
    write_manifest_fixture_rows_with_counts(
        rows,
        serde_json::json!({
            "canonical": row_count,
            "stale": 0,
            "stale_descendant": 0,
            "strict_btc_orphan": 0,
            "weak_btc_orphan": 0
        }),
    )
}

fn write_manifest_fixture_rows_with_counts(
    rows: &[String],
    counts: serde_json::Value,
) -> Result<ManifestFixture> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("clock before epoch")?
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "mmm-manifest-import-{}-{suffix}",
        std::process::id()
    ));
    let publication_dir = root.join("results/monitor-evidence");
    std::fs::create_dir_all(&publication_dir)?;

    let artifact_path = publication_dir.join("devcoin_monitor_evidence.csv");
    std::fs::write(
        &artifact_path,
        format!("{NORMALIZED_HEADER}{}", rows.concat()),
    )?;
    let artifact_bytes = std::fs::read(&artifact_path)?;
    let row_count = u64::try_from(rows.len()).context("fixture row count exceeds u64")?;

    let research_manifest_path = publication_dir.join("monitor-evidence-manifest.json");
    let research_manifest_bytes = b"{\"fixture\":\"manifest-backed import\"}\n";
    std::fs::write(&research_manifest_path, research_manifest_bytes)?;

    let committed_manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../data/historical/historical-source-manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&committed_manifest)?)?;
    manifest["publication_manifest_sha256"] =
        serde_json::Value::String(sha256::Hash::hash(research_manifest_bytes).to_string());

    let artifacts = manifest["artifacts"]
        .as_array_mut()
        .context("manifest artifacts array")?;
    let devcoin_index = artifacts
        .iter()
        .position(|artifact| artifact["chain"] == "devcoin" && artifact["role"] == "event")
        .context("devcoin event artifact")?;
    let prior_devcoin_rows = artifacts[devcoin_index]["row_count"]
        .as_u64()
        .context("devcoin row_count")?;
    artifacts[devcoin_index]["row_count"] = serde_json::json!(row_count);
    artifacts[devcoin_index]["size_bytes"] = serde_json::json!(artifact_bytes.len());
    artifacts[devcoin_index]["sha256"] =
        serde_json::json!(sha256::Hash::hash(&artifact_bytes).to_string());
    artifacts[devcoin_index]["counts"] = counts;

    let donor = artifacts
        .iter_mut()
        .find(|artifact| artifact["chain"] == "elastos" && artifact["role"] == "event")
        .context("elastos event artifact")?;
    let transferred_rows = prior_devcoin_rows
        .checked_sub(row_count)
        .context("fixture cannot exceed committed devcoin row count")?;
    donor["row_count"] = serde_json::json!(
        donor["row_count"].as_u64().context("elastos row_count")? + transferred_rows
    );
    donor["counts"]["canonical"] = serde_json::json!(
        donor["counts"]["canonical"]
            .as_u64()
            .context("elastos canonical count")?
            + transferred_rows
    );

    let manifest_path = root.join("monitor-manifest.json");
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(ManifestFixture {
        artifact_path,
        config: HistoricalImportAllConfig {
            manifest_path,
            artifact_root: root.clone(),
            require_pinned_checkout: false,
            batch_size: 10,
            allow_empty_known_stales: true,
        },
        root,
    })
}

fn two_chain_import_configs(
    ixcoin_path: &Path,
    devcoin_path: &Path,
) -> Vec<HistoricalImportConfig> {
    [("ixcoin", ixcoin_path), ("devcoin", devcoin_path)]
        .into_iter()
        .map(|(chain, path)| {
            let mut config = HistoricalImportConfig::for_csv(chain, path);
            config.allow_empty_known_stales = true;
            config
        })
        .collect()
}

async fn block_kind_and_orphan_class(
    client: &tokio_postgres::Client,
    header: &Header,
) -> Result<(String, Option<String>)> {
    let row = client
        .query_one(
            "SELECT kind, btc_orphan_class FROM block WHERE btc_header_hash = $1",
            &[&header.block_hash().to_byte_array().to_vec()],
        )
        .await?;
    Ok((row.get(0), row.get(1)))
}

async fn source_event_count(client: &tokio_postgres::Client, source_id: i64) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM merge_mining_event WHERE source_id = $1",
            &[&source_id],
        )
        .await?
        .get(0))
}

fn write_normalized_csv(
    header: &Header,
    classification: &str,
    relevance: &str,
    relevance_reason: &str,
    coinbase_script: &[u8],
    btc_height: i32,
) -> Result<PathBuf> {
    write_normalized_csv_for_chain(
        "devcoin",
        header,
        classification,
        relevance,
        relevance_reason,
        coinbase_script,
        btc_height,
    )
}

fn write_normalized_csv_for_chain(
    chain: &str,
    header: &Header,
    classification: &str,
    relevance: &str,
    relevance_reason: &str,
    coinbase_script: &[u8],
    btc_height: i32,
) -> Result<PathBuf> {
    write_normalized_csv_row(
        header,
        &NormalizedCsvRow {
            chain,
            source_row_number: 1,
            classification,
            relevance,
            relevance_reason,
            coinbase_script,
            btc_height,
            child_height: 12,
            child_hash: None,
        },
    )
}

struct NormalizedCsvRow<'a> {
    chain: &'a str,
    source_row_number: i64,
    classification: &'a str,
    relevance: &'a str,
    relevance_reason: &'a str,
    coinbase_script: &'a [u8],
    btc_height: i32,
    child_height: i32,
    child_hash: Option<&'a [u8]>,
}

fn write_normalized_csv_row(header: &Header, row: &NormalizedCsvRow<'_>) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let csv_row = normalized_csv_line(header, row);
    std::fs::write(&path, format!("{NORMALIZED_HEADER}{csv_row}"))
        .with_context(|| format!("write temp CSV {}", path.display()))?;
    Ok(path)
}

fn normalized_csv_line(header: &Header, row: &NormalizedCsvRow<'_>) -> String {
    normalized_csv_line_with_parent_coinbase(header, row, "", "")
}

fn normalized_csv_line_with_child_header(
    header: &Header,
    row: &NormalizedCsvRow<'_>,
    child_header: &[u8],
) -> String {
    let line = normalized_csv_line(header, row);
    let mut fields = line
        .trim_end()
        .split(',')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    fields[8] = hex::encode(child_header);
    format!("{}\n", fields.join(","))
}

fn normalized_csv_line_with_parent_coinbase(
    header: &Header,
    row: &NormalizedCsvRow<'_>,
    coinbase_outputs: &str,
    full_coinbase: &str,
) -> String {
    let validation_status = match (row.classification, row.relevance_reason) {
        ("canonical", _) => "VALID (canonical Bitcoin block)",
        ("stale", _) | ("unknown", "valid_direct_stale") => "VALID",
        ("stale_descendant", _) | ("unknown", "valid_stale_descendant") => "VALID_STALE_DESCENDANT",
        _ => "",
    };
    let bits = format!("{:08x}", header.bits.to_consensus());
    let child_hash = row.child_hash.map(hex::encode).unwrap_or_default();
    format!(
        "{},full_inventory,<fixture>,{},full_classifier_inventory,test,\
         {},{child_hash},,,,{},{},{},{},{bits},{},{},{},{},{},{},\
         {validation_status},{bits},,{},{relevance_reason}\n",
        row.chain,
        row.source_row_number,
        row.child_height,
        row.btc_height,
        header.block_hash(),
        header.prev_blockhash,
        header.time,
        header.nonce,
        hex::encode(serialize(header)),
        hex::encode(row.coinbase_script),
        coinbase_outputs,
        full_coinbase,
        row.classification,
        row.relevance,
        relevance_reason = row.relevance_reason,
    )
}

fn write_normalized_csv_with_parent_coinbase(
    header: &Header,
    coinbase_script: &str,
    coinbase_outputs: &str,
    full_coinbase: &str,
    btc_height: i32,
) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let script = hex::decode(coinbase_script)?;
    let row = NormalizedCsvRow {
        chain: "devcoin",
        source_row_number: 1,
        classification: "canonical",
        relevance: "",
        relevance_reason: "canonical_parent",
        coinbase_script: &script,
        btc_height,
        child_height: 12,
        child_hash: None,
    };
    let csv_row =
        normalized_csv_line_with_parent_coinbase(header, &row, coinbase_outputs, full_coinbase);
    std::fs::write(&path, format!("{NORMALIZED_HEADER}{csv_row}"))
        .with_context(|| format!("write temp CSV {}", path.display()))?;
    Ok(path)
}

fn write_repeated_then_unique_canonical_rows(
    first: &Header,
    second: &Header,
    third: &Header,
    btc_height: i32,
) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let first_row = NormalizedCsvRow {
        chain: "devcoin",
        source_row_number: 1,
        classification: "canonical",
        relevance: "",
        relevance_reason: "canonical_parent",
        coinbase_script: &[],
        btc_height,
        child_height: 12,
        child_hash: None,
    };
    let repeated_row = NormalizedCsvRow {
        source_row_number: 2,
        child_height: 13,
        ..first_row
    };
    let unique_row = NormalizedCsvRow {
        source_row_number: 3,
        child_height: 14,
        ..first_row
    };
    let third_unique_row = NormalizedCsvRow {
        source_row_number: 4,
        child_height: 15,
        ..first_row
    };
    std::fs::write(
        &path,
        format!(
            "{NORMALIZED_HEADER}{}{}{}{}",
            normalized_csv_line(first, &first_row),
            normalized_csv_line(first, &repeated_row),
            normalized_csv_line(second, &unique_row),
            normalized_csv_line(third, &third_unique_row)
        ),
    )
    .with_context(|| format!("write temp CSV {}", path.display()))?;
    Ok(path)
}

fn write_refining_rows(header: &Header, btc_height: i32) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let child_hash = vec![0x7a; 32];
    let mut row = NormalizedCsvRow {
        chain: "devcoin",
        source_row_number: 1,
        classification: "canonical",
        relevance: "",
        relevance_reason: "canonical_parent",
        coinbase_script: &[],
        btc_height,
        child_height: 12,
        child_hash: None,
    };
    let partial = normalized_csv_line(header, &row);
    row.source_row_number = 2;
    row.child_hash = Some(&child_hash);
    let exact = normalized_csv_line(header, &row);
    std::fs::write(&path, format!("{NORMALIZED_HEADER}{partial}{exact}"))
        .with_context(|| format!("write temp CSV {}", path.display()))?;
    Ok(path)
}

async fn seed_identity_event(
    client: &tokio_postgres::Client,
    source_code: &str,
    header: &Header,
    child_height: i32,
    child_hash: Option<Vec<u8>>,
) -> Result<i64> {
    let source_id = get_source_id(client, source_code).await?;
    let parent_hash = header.block_hash().to_byte_array().to_vec();
    let parent_prev_hash = header.prev_blockhash.to_byte_array().to_vec();
    let parent_header = serialize(header);
    let parent_time = i64::from(header.time);
    Ok(client
        .query_one(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_prev_header_hash, \
                btc_parent_header_bytes, btc_parent_header_time, btc_parent_height, \
                btc_parent_kind, pow_validates_btc_target, difficulty_epoch_ok, \
                discovered_at, confirmed_at \
             ) VALUES ($1, $2, $3, NULL, $4, $5, $6, $7, NULL, \
                       'unknown', TRUE, NULL, $8, $8) \
             RETURNING id",
            &[
                &source_id,
                &child_height,
                &child_hash,
                &parent_hash,
                &parent_prev_hash,
                &parent_header,
                &parent_time,
                &2_000_i64,
            ],
        )
        .await?
        .get(0))
}

fn write_contradictory_exact_rows(
    chain: &str,
    header: &Header,
    btc_height: i32,
) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let bits = format!("{:08x}", header.bits.to_consensus());
    let child_hash = "66".repeat(32);
    let row = |source_row: i32, child_height: i32| {
        format!(
            "{chain},full_inventory,<fixture>,{source_row},full_classifier_inventory,test,\
             {child_height},{child_hash},,,,{btc_height},{},{},{},{bits},{},{},,,,canonical,\
             VALID (canonical Bitcoin block),{bits},,,canonical_parent\n",
            header.block_hash(),
            header.prev_blockhash,
            header.time,
            header.nonce,
            hex::encode(serialize(header)),
        )
    };
    std::fs::write(
        &path,
        format!("{NORMALIZED_HEADER}{}{}", row(1, 12), row(2, 13)),
    )
    .with_context(|| format!("write temp CSV {}", path.display()))?;
    Ok(path)
}

async fn seed_unpublished_event(
    client: &tokio_postgres::Client,
    source_code: &str,
    child_height: i32,
    child_hash: Vec<u8>,
) -> Result<i64> {
    let source_id = get_source_id(client, source_code).await?;
    Ok(client
        .query_one(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_prev_header_hash, \
                btc_parent_header_bytes, btc_parent_header_time, \
                btc_parent_height, btc_parent_kind, pow_validates_btc_target, \
                pow_validates_child_target, difficulty_epoch_ok, discovered_at, confirmed_at \
             ) \
             SELECT $1, $2, $3, child_block_time, btc_parent_header_hash, \
                    btc_parent_prev_header_hash, btc_parent_header_bytes, \
                    btc_parent_header_time, btc_parent_height, btc_parent_kind, \
                    pow_validates_btc_target, pow_validates_child_target, \
                    difficulty_epoch_ok, discovered_at, confirmed_at \
             FROM merge_mining_event \
             WHERE source_id = $1 \
             ORDER BY id \
             LIMIT 1 \
             RETURNING id",
            &[&source_id, &child_height, &child_hash],
        )
        .await
        .context("seed unpublished event")?
        .get(0))
}

async fn active_source_event_count(
    client: &tokio_postgres::Client,
    source_code: &str,
) -> Result<i64> {
    let source_id = get_source_id(client, source_code).await?;
    Ok(client
        .query_one(
            "SELECT count(*) FROM merge_mining_event \
             WHERE source_id = $1 AND revoked_at IS NULL",
            &[&source_id],
        )
        .await?
        .get(0))
}

fn cleanup_temp_files(paths: &[&PathBuf]) -> Result<()> {
    for path in paths {
        std::fs::remove_file(path)
            .with_context(|| format!("remove temp CSV {}", path.display()))?;
    }
    Ok(())
}

fn finish_import_with_cleanup(import_result: Result<()>, paths: &[&PathBuf]) -> Result<()> {
    let cleanup_result = cleanup_temp_files(paths);
    import_result?;
    cleanup_result
}

fn temp_csv_path() -> Result<PathBuf> {
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock before unix epoch")?
        .as_nanos();
    Ok(std::env::temp_dir().join(format!(
        "merge-mining-monitor-historical-ingest-{}-{suffix}.csv",
        std::process::id()
    )))
}
