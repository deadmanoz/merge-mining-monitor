use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::serialize;
use bitcoin::hashes::{Hash as _, sha256};
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::btc_orphan::{BtcOrphanVerdict, classify_btc_orphan};
use mmm_producers::{
    HistoricalImportAllConfig, HistoricalImportConfig, run_historical_import,
    run_historical_import_configs_for_test, run_manifest_historical_import_for_test,
};
use mmm_store::get_source_id;

use crate::support::scenario::{
    canonical_verdict, stale_verdict_with_competitor_header, unknown_verdict,
};
use crate::support::{
    absent_classifier, btc_400000_coinbase_script, btc_400000_header, header_meeting_bits,
};

const NORMALIZED_HEADER: &str = "chain,source_kind,source_path,source_row_number,artifact_scope,provenance,child_height,child_block_hash,child_header_hex,child_block_time,child_nbits,btc_height,btc_header_hash,btc_prev_hash,btc_time,btc_bits,btc_nonce,btc_header_hex,coinbase_scriptsig_hex,coinbase_outputs,full_coinbase_hex,classification,validation_status,expected_nbits,rejection_reason,btc_stale_relevance,relevance_reason\n";

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
async fn multi_chain_import_reuses_the_parent_classification_cache() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_036, 36);
        let ixcoin_path = write_normalized_csv_for_chain(
            "ixcoin",
            &header,
            "canonical",
            "",
            "canonical_parent",
            &[],
            700_036,
        )?;
        let devcoin_path = write_normalized_csv_for_chain(
            "devcoin",
            &header,
            "canonical",
            "",
            "canonical_parent",
            &[],
            700_036,
        )?;
        let result = async {
            let fake = FakeParentClassifier::new(canonical_verdict(&header, 700_036));
            let classifier = ConfiguredParentClassifier::Fake(fake.clone());
            let summary = run_historical_import_configs_for_test(
                &mut client,
                &classifier,
                two_chain_import_configs(&ixcoin_path, &devcoin_path),
            )
            .await?;

            assert_eq!(summary.chains.len(), 2);
            assert_eq!(summary.stale_branches_reconciled, 0);
            assert_eq!(
                fake.call_count().await,
                1,
                "the repeated parent must be classified once across both chains"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&ixcoin_path, &devcoin_path])
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
        let header = btc_400000_header()?;
        let coinbase_script = btc_400000_coinbase_script()?;
        let verdict = classify_btc_orphan(
            i64::from(header.time),
            header.bits,
            parse_bip34_height(&coinbase_script),
        )
        .0;
        let relevance = match verdict {
            BtcOrphanVerdict::Strict => "strict_btc_orphan",
            BtcOrphanVerdict::Weak => "weak_btc_orphan",
            other => panic!("fixture must be strict/weak, got {other:?}"),
        };
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
        let csv_path =
            write_normalized_csv(&header, "canonical", "", "canonical_parent", &[], 700_010)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                canonical_verdict(&header, 700_010),
            ));
            let config = devcoin_import_config(&csv_path);
            run_historical_import(&mut client, &classifier, &config).await?;
            seed_unpublished_event(&client, "auxpow:devcoin", 99, vec![0x44; 32]).await?;

            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_eq!(summary.updated, 1);
            assert_eq!(summary.removed, 1);
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
        let csv_path = write_contradictory_exact_rows(&header, 700_012)?;
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
    std::fs::write(&artifact_path, format!("{NORMALIZED_HEADER}{row}"))?;
    let artifact_bytes = std::fs::read(&artifact_path)?;

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
    artifacts[devcoin_index]["row_count"] = serde_json::json!(1);
    artifacts[devcoin_index]["size_bytes"] = serde_json::json!(artifact_bytes.len());
    artifacts[devcoin_index]["sha256"] =
        serde_json::json!(sha256::Hash::hash(&artifact_bytes).to_string());
    artifacts[devcoin_index]["counts"] = serde_json::json!({
        "canonical": 1,
        "stale": 0,
        "stale_descendant": 0,
        "strict_btc_orphan": 0,
        "weak_btc_orphan": 0
    });

    let donor = artifacts
        .iter_mut()
        .find(|artifact| artifact["chain"] == "elastos" && artifact["role"] == "event")
        .context("elastos event artifact")?;
    let transferred_rows = prior_devcoin_rows
        .checked_sub(1)
        .context("committed devcoin artifact is non-empty")?;
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
            allow_unclassified: false,
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
    let validation_status = if row.classification == "canonical" {
        "VALID (canonical Bitcoin block)"
    } else if row.classification == "stale" {
        "VALID"
    } else {
        ""
    };
    let bits = format!("{:08x}", header.bits.to_consensus());
    let child_hash = row.child_hash.map(hex::encode).unwrap_or_default();
    format!(
        "{},full_inventory,<fixture>,{},full_classifier_inventory,test,\
         {},{child_hash},,,,{},{},{},{},{bits},{},{},{},,,{},\
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
        row.classification,
        row.relevance,
        relevance_reason = row.relevance_reason,
    )
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

fn write_contradictory_exact_rows(header: &Header, btc_height: i32) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let bits = format!("{:08x}", header.bits.to_consensus());
    let child_hash = "66".repeat(32);
    let row = |source_row: i32, child_height: i32| {
        format!(
            "devcoin,full_inventory,<fixture>,{source_row},full_classifier_inventory,test,\
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
) -> Result<()> {
    let source_id = get_source_id(client, source_code).await?;
    client
        .execute(
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
             LIMIT 1",
            &[&source_id, &child_height, &child_hash],
        )
        .await
        .context("seed unpublished event")?;
    Ok(())
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
