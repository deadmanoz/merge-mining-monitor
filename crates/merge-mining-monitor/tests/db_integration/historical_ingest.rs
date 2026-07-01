use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::btc_orphan::{BtcOrphanVerdict, classify_btc_orphan};
use mmm_producers::{HistoricalImportConfig, HistoricalImportSummary, run_historical_import};
use mmm_store::get_source_id;

use crate::support::header_meeting_bits;
use crate::support::scenario::{
    canonical_verdict, orphan_candidate_verdict, stale_verdict_with_competitor_header,
    unknown_verdict,
};

const BTC_400000_HEADER_HEX: &str = "0400000039fa821848781f027a2e6dfabbf6bda920d9ae61b63400030000000000000000ecae536a304042e3154be0e3e9a8220e5568c3433a9ab49ac4cbb74f8df8e8b0cc2acf569fb9061806652c27";
const BTC_400000_COINBASE_SCRIPTSIG_HEX: &str = "03801a060004cc2acf560433c30f37085d4a39ad543b0c000a425720537570706f727420384d200a666973686572206a696e78696e092f425720506f6f6c2f";

fn assert_single_row_import(summary: &HistoricalImportSummary, candidates: u64, ingested: u64) {
    assert_eq!(summary.rows_seen, 1);
    assert_eq!(summary.candidates, candidates);
    assert_eq!(summary.ingested, ingested);
}

#[tokio::test]
async fn import_dataset_persists_core_classified_rows_and_skips_unattested_unknowns() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        let skipped_header = header_meeting_bits(0x207f_ffff, 1_700_000_000, 1);
        let imported_header = header_meeting_bits(0x207f_ffff, 1_700_000_001, 2);
        let csv_path = write_historical_csv(&skipped_header, &imported_header)?;

        let import_result = async {
            let classifier =
                ConfiguredParentClassifier::Fake(FakeParentClassifier::new_sequence([
                    unknown_verdict(&skipped_header),
                    canonical_verdict(&imported_header, 700_001),
                    unknown_verdict(&skipped_header),
                    canonical_verdict(&imported_header, 700_001),
                ]));
            let config = devcoin_import_config(&csv_path, None);

            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_eq!(summary.rows_seen, 2);
            assert_eq!(summary.candidates, 1);
            assert_eq!(summary.ingested, 1);
            assert_eq!(summary.canonical, 1);
            assert_eq!(summary.skipped.get("unclassified"), Some(&1));
            let replay_summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_eq!(replay_summary.ingested, 1);
            assert_eq!(replay_summary.canonical, 1);

            let devcoin = get_source_id(&client, "auxpow:devcoin").await?;
            let event_count: i64 = client
                .query_one(
                    "SELECT COUNT(*) FROM merge_mining_event WHERE source_id = $1",
                    &[&devcoin],
                )
                .await?
                .get(0);
            assert_eq!(event_count, 1);

            let block = client
                .query_one(
                    "SELECT kind, btc_height, core_attested \
                     FROM block WHERE btc_header_hash = $1",
                    &[&imported_header.block_hash().to_byte_array().to_vec()],
                )
                .await?;
            assert_eq!(block.get::<_, String>(0), "canonical");
            assert_eq!(block.get::<_, Option<i32>>(1), Some(700_001));
            assert!(block.get::<_, bool>(2));

            let canonical_parents: i64 = client
                .query_one(
                    "SELECT canonical_parents FROM source_health WHERE source_id = $1",
                    &[&devcoin],
                )
                .await?
                .get(0);
            assert_eq!(canonical_parents, 1);

            Ok::<_, anyhow::Error>(())
        }
        .await;

        finish_import_with_cleanup(import_result, &[&csv_path])
    })
}

#[tokio::test]
async fn import_dataset_requires_source_label_then_persists_core_attested_orphan() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header: Header = deserialize(&hex::decode(BTC_400000_HEADER_HEX)?)?;
        let coinbase_script = hex::decode(BTC_400000_COINBASE_SCRIPTSIG_HEX)?;
        let display_hash = header.block_hash().to_string();
        let verdict = classify_btc_orphan(
            header.time as i64,
            header.bits,
            parse_bip34_height(&coinbase_script),
        )
        .0;
        assert!(
            matches!(verdict, BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak),
            "fixture must be strict/weak for importer coverage, got {verdict:?}"
        );
        let relevance = match verdict {
            BtcOrphanVerdict::Strict => "strict_btc_orphan",
            BtcOrphanVerdict::Weak => "weak_btc_orphan",
            BtcOrphanVerdict::Excluded | BtcOrphanVerdict::Pending => unreachable!(),
        };
        let skip_csv_path = write_classified_csv(&header, &coinbase_script, "stale")?;
        let csv_path = write_orphan_csv(&header, &coinbase_script)?;
        let relevance_path = write_relevance_csv(&display_hash, relevance)?;

        let import_result = async {
            let classifier = absent_classifier(&header);
            let skip_config = devcoin_import_config(&skip_csv_path, None);

            let skipped = run_historical_import(&mut client, &classifier, &skip_config).await?;
            assert_single_row_import(&skipped, 0, 0);
            assert_eq!(skipped.skipped.get("unclassified"), Some(&1));

            let config = devcoin_import_config(&csv_path, Some(&relevance_path));

            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_single_row_import(&summary, 1, 1);
            assert_eq!(summary.strict_orphans + summary.weak_orphans, 1);

            let block = client
                .query_one(
                    "SELECT kind, btc_orphan_class \
                     FROM block WHERE btc_header_hash = $1",
                    &[&header.block_hash().to_byte_array().to_vec()],
                )
                .await?;
            assert_eq!(block.get::<_, String>(0), "unknown");
            assert_eq!(
                block.get::<_, Option<String>>(1).as_deref(),
                Some(relevance)
            );

            Ok::<_, anyhow::Error>(())
        }
        .await;

        finish_import_with_cleanup(import_result, &[&skip_csv_path, &csv_path, &relevance_path])
    })
}

#[tokio::test]
async fn import_dataset_persists_known_branch_attestation_when_core_classifies_stale() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        let stale_header = header_meeting_bits(0x207f_ffff, 1_700_000_002, 12);
        let competitor_header = header_meeting_bits(0x207f_ffff, 1_700_000_003, 13);
        let competitor_hash = competitor_header.block_hash().to_byte_array().to_vec();
        let coinbase_script = hex::decode("04ffff001d0104")?;
        let csv_path = write_orphan_csv(&stale_header, &coinbase_script)?;
        let relevance_path = write_relevance_csv_with_reason(
            &stale_header.block_hash().to_string(),
            "btc_stale_excluded",
            "known_stale_descendant_hash",
        )?;
        let height = 700_002;

        let import_result = async {
            let stale_classification = stale_verdict_with_competitor_header(
                &stale_header,
                height,
                competitor_header,
                competitor_hash.clone(),
            );
            let classifier =
                ConfiguredParentClassifier::Fake(FakeParentClassifier::new(stale_classification));
            let config = devcoin_import_config(&csv_path, Some(&relevance_path));

            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_single_row_import(&summary, 1, 1);
            assert_eq!(summary.stale, 1);
            assert_eq!(summary.known_descendant_branch_attestations, 1);
            assert_eq!(summary.known_direct_branch_attestations, 0);

            let stale_hash = stale_header.block_hash().to_byte_array().to_vec();
            let block = client
                .query_one(
                    "SELECT kind, btc_height, canonical_competitor_hash \
                     FROM block WHERE btc_header_hash = $1",
                    &[&stale_hash],
                )
                .await?;
            assert_eq!(block.get::<_, String>(0), "stale");
            assert_eq!(block.get::<_, Option<i32>>(1), Some(height));
            assert_eq!(
                block.get::<_, Option<Vec<u8>>>(2),
                Some(competitor_hash.clone())
            );

            let derivable_competition: bool = client
                .query_one(
                    "SELECT EXISTS ( \
                        SELECT 1 \
                        FROM block stale \
                        JOIN block canonical \
                          ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                        WHERE stale.btc_header_hash = $1 \
                          AND stale.kind = 'stale' \
                          AND canonical.kind = 'canonical' \
                    )",
                    &[&stale_hash],
                )
                .await?
                .get(0);
            assert!(derivable_competition);

            Ok::<_, anyhow::Error>(())
        }
        .await;

        finish_import_with_cleanup(import_result, &[&csv_path, &relevance_path])
    })
}

#[tokio::test]
async fn import_dataset_skips_known_branch_attestation_when_core_cannot_classify() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = header_meeting_bits(0x207f_ffff, 1_700_000_004, 14);
        let coinbase_script = hex::decode("04ffff001d0104")?;
        let csv_path = write_orphan_csv(&header, &coinbase_script)?;
        let relevance_path = write_relevance_csv_with_reason(
            &header.block_hash().to_string(),
            "btc_stale_excluded",
            "known_direct_stale_hash",
        )?;

        let import_result = async {
            let classifier = absent_classifier(&header);
            let config = devcoin_import_config(&csv_path, Some(&relevance_path));

            let summary = run_historical_import(&mut client, &classifier, &config).await?;
            assert_single_row_import(&summary, 0, 0);
            assert_eq!(summary.skipped.get("known_branch_not_classified"), Some(&1));

            let devcoin = get_source_id(&client, "auxpow:devcoin").await?;
            let event_count: i64 = client
                .query_one(
                    "SELECT COUNT(*)::bigint FROM merge_mining_event WHERE source_id = $1",
                    &[&devcoin],
                )
                .await?
                .get(0);
            assert_eq!(event_count, 0);

            Ok::<_, anyhow::Error>(())
        }
        .await;

        finish_import_with_cleanup(import_result, &[&csv_path, &relevance_path])
    })
}

fn absent_classifier(header: &Header) -> ConfiguredParentClassifier {
    ConfiguredParentClassifier::Fake(FakeParentClassifier::new(orphan_candidate_verdict(header)))
}

fn devcoin_import_config(csv_path: &Path, relevance_path: Option<&Path>) -> HistoricalImportConfig {
    HistoricalImportConfig {
        chain: "devcoin".to_owned(),
        csv_path: csv_path.to_path_buf(),
        relevance_path: relevance_path.map(Path::to_path_buf),
        batch_size: 10,
        limit: None,
        allow_unclassified: false,
    }
}

fn write_historical_csv(skipped_header: &Header, imported_header: &Header) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let contents = format!(
        "dvc_height,btc_header_hex,coinbase_scriptsig_hex,classification\n\
         10,{},04ffff001d0104,stale\n\
         11,{},04ffff001d0104,stale\n",
        hex::encode(serialize(skipped_header)),
        hex::encode(serialize(imported_header))
    );
    std::fs::write(&path, contents)
        .with_context(|| format!("write temp CSV {}", path.display()))?;
    Ok(path)
}

fn write_orphan_csv(header: &Header, coinbase_script: &[u8]) -> Result<PathBuf> {
    write_classified_csv(header, coinbase_script, "orphan")
}

fn write_classified_csv(
    header: &Header,
    coinbase_script: &[u8],
    classification: &str,
) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let contents = format!(
        "dvc_height,btc_header_hex,coinbase_scriptsig_hex,classification,btc_header_hash\n\
         12,{},{},{classification},{}\n",
        hex::encode(serialize(header)),
        hex::encode(coinbase_script),
        header.block_hash()
    );
    std::fs::write(&path, contents)
        .with_context(|| format!("write temp CSV {}", path.display()))?;
    Ok(path)
}

fn write_relevance_csv(display_hash: &str, relevance: &str) -> Result<PathBuf> {
    write_relevance_csv_with_reason(display_hash, relevance, "")
}

fn write_relevance_csv_with_reason(
    display_hash: &str,
    relevance: &str,
    reason: &str,
) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let contents = format!(
        "chain,btc_stale_relevance,relevance_reason,btc_header_hash\n\
         devcoin,{relevance},{reason},{display_hash}\n"
    );
    std::fs::write(&path, contents)
        .with_context(|| format!("write temp relevance CSV {}", path.display()))?;
    Ok(path)
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
