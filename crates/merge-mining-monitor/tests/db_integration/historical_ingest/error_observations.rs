use std::path::PathBuf;

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, FakeParentClassifier, HeightSource, ParentClassification,
    TIME_BELOW_MTP,
};
use mmm_producers::run_error_observation_import_for_test;

use super::{
    NORMALIZED_HEADER, NormalizedCsvRow, active_source_event_count, finish_import_with_cleanup,
    normalized_csv_line, temp_csv_path,
};
use crate::support::{db::seed_bitcoin_core_header_cache_through, header_meeting_bits};

const LEGACY_MTP_REJECTION_REASON: &str = "median_time_past_violation";

#[tokio::test]
async fn import_requires_catalogue_match_before_writing() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let unsupported = header_meeting_bits(0x207f_ffff, 1_700_000_045, 45);
        let unsupported_path = write_csv(&unsupported, "devcoin", 45, 700_045, TIME_BELOW_MTP)?;
        let accepted = catalogued_header()?;
        let expected_parent_hashes = [accepted.block_hash().to_byte_array()];
        let accepted_path = write_csv(&accepted, "devcoin", 46, 946_213, TIME_BELOW_MTP)?;
        let result = async {
            let skipped_classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                ParentClassification::unknown(&unsupported),
            ));
            let error = run_error_observation_import_for_test(
                &mut client,
                &skipped_classifier,
                &unsupported_path,
                &expected_parent_hashes,
            )
            .await
            .expect_err("a non-catalogued error observation must fail before writes");
            assert!(
                error
                    .to_string()
                    .contains("would be skipped as unsupported_classification")
            );
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                0
            );

            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                ParentClassification::error_block(
                    &accepted,
                    946_213,
                    HeightSource::ErrorBlockCatalog,
                    None,
                    TIME_BELOW_MTP,
                ),
            ));
            let summary = run_error_observation_import_for_test(
                &mut client,
                &classifier,
                &accepted_path,
                &expected_parent_hashes,
            )
            .await?;
            assert_eq!(summary.expected_rows, 1);
            assert_eq!(summary.rows_seen, 1);
            assert_eq!(summary.ingested, 1);
            assert_eq!(summary.error_blocks, 1);
            assert_eq!(summary.error_parents, 1);

            let row = client
                .query_one(
                    "SELECT p.classification, p.artifact_scope, b.kind, b.btc_height, \
                            b.error_block_reason \
                     FROM historical_event_provenance p \
                     JOIN merge_mining_event e ON e.id = p.event_id \
                     JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash",
                    &[],
                )
                .await?;
            assert_eq!(row.get::<_, String>(0), "error_block");
            assert_eq!(row.get::<_, String>(1), "error-block-observations");
            assert_eq!(row.get::<_, String>(2), "error_block");
            assert_eq!(row.get::<_, i32>(3), 946_213);
            assert_eq!(
                row.get::<_, Option<String>>(4).as_deref(),
                Some(TIME_BELOW_MTP)
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&unsupported_path, &accepted_path])
    })
}

#[tokio::test]
async fn import_accepts_live_mtp_verdict_for_legacy_catalogue_token() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = legacy_mtp_header()?;
        let parent_hash = header.block_hash().to_byte_array();
        let path = write_csv(
            &header,
            "namecoin",
            255_293,
            380_992,
            LEGACY_MTP_REJECTION_REASON,
        )?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                ParentClassification::error_block(
                    &header,
                    380_992,
                    HeightSource::PrevCanonical,
                    Some(true),
                    TIME_BELOW_MTP,
                ),
            ));

            let summary = run_error_observation_import_for_test(
                &mut client,
                &classifier,
                &path,
                &[parent_hash],
            )
            .await?;
            assert_eq!(summary.ingested, 1);

            let row = client
                .query_one(
                    "SELECT btc_height_source, error_block_reason \
                     FROM block \
                     WHERE btc_header_hash = $1",
                    &[&parent_hash.as_slice()],
                )
                .await?;
            assert_eq!(row.get::<_, String>(0), "prev-canonical");
            assert_eq!(
                row.get::<_, Option<String>>(1).as_deref(),
                Some(LEGACY_MTP_REJECTION_REASON)
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&path])
    })
}

#[tokio::test]
async fn error_observation_coordinate_cannot_change_across_publications() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let accepted = catalogued_header()?;
        let expected_parent_hashes = [accepted.block_hash().to_byte_array()];
        let original_path = write_csv(&accepted, "devcoin", 46, 946_213, TIME_BELOW_MTP)?;
        let changed_path = write_csv(&accepted, "devcoin", 47, 946_213, TIME_BELOW_MTP)?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                ParentClassification::error_block(
                    &accepted,
                    946_213,
                    HeightSource::ErrorBlockCatalog,
                    None,
                    TIME_BELOW_MTP,
                ),
            ));
            run_error_observation_import_for_test(
                &mut client,
                &classifier,
                &original_path,
                &expected_parent_hashes,
            )
            .await?;
            client
                .execute(
                    "UPDATE historical_event_provenance \
                     SET publication_ref = $1 \
                     WHERE artifact_scope = 'error-block-observations'",
                    &[&"08da16532a55240e54c4051d5d324a0484b80b1c"],
                )
                .await?;

            let error = run_error_observation_import_for_test(
                &mut client,
                &classifier,
                &changed_path,
                &expected_parent_hashes,
            )
            .await
            .expect_err("a changed error-observation coordinate must fail");
            assert!(
                format!("{error:#}").contains("error-observation coordinate"),
                "unexpected error: {error:#}"
            );
            assert_eq!(
                active_source_event_count(&client, "auxpow:devcoin").await?,
                1
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&original_path, &changed_path])
    })
}

#[tokio::test]
async fn retarget_observation_requires_core_epoch_nbits() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let header = retarget_header()?;
        let expected_parent_hashes = [header.block_hash().to_byte_array()];
        seed_bitcoin_core_header_cache_through(
            &client,
            717_696,
            i64::from(header.time),
            0x170b_8c8b,
        )
        .await?;
        let accepted_path = write_csv_with_expected_nbits(
            &header,
            "emercoin",
            50,
            717_696,
            "nbits_retarget_not_applied",
            0x170b_8c8b,
        )?;
        let mismatched_path = write_csv_with_expected_nbits(
            &header,
            "emercoin",
            51,
            717_696,
            "nbits_retarget_not_applied",
            0,
        )?;
        let result = async {
            let classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                ParentClassification::error_block(
                    &header,
                    717_696,
                    HeightSource::ErrorBlockCatalog,
                    None,
                    "nbits_retarget_not_applied",
                ),
            ));
            let error = run_error_observation_import_for_test(
                &mut client,
                &classifier,
                &mismatched_path,
                &expected_parent_hashes,
            )
            .await
            .expect_err("retarget observation with the wrong Core target must fail");
            assert!(
                format!("{error:#}").contains("failed evidence_mismatch"),
                "unexpected error: {error:#}"
            );
            assert_eq!(
                active_source_event_count(&client, "auxpow:emercoin").await?,
                0
            );

            let summary = run_error_observation_import_for_test(
                &mut client,
                &classifier,
                &accepted_path,
                &expected_parent_hashes,
            )
            .await?;
            assert_eq!(summary.ingested, 1);
            Ok::<_, anyhow::Error>(())
        }
        .await;
        finish_import_with_cleanup(result, &[&accepted_path, &mismatched_path])
    })
}

fn catalogued_header() -> Result<Header> {
    deserialize(&hex::decode(
        "00a0032bb223f1aad55892df75d0ff4712f0543959c5065ab89d000000000000000000005eba715327fc82c765fa651bd6226c4b4a6a846cd60197bcd76d47ada0611cfce335df696913021725806e70",
    )?)
    .context("decode catalogued time_below_mtp error-block header")
}

fn legacy_mtp_header() -> Result<Header> {
    deserialize(&hex::decode(
        "0300000092d98cb6018e9baa8dfe136fa81266dfa588c0ee23b26e030000000000000000af324cb995102e1d1c5e7d59459d5f651090881815f2a63f0eba667ce3db7538cf003156140f12182744ec68",
    )?)
    .context("decode catalogued median_time_past_violation error-block header")
}

fn retarget_header() -> Result<Header> {
    deserialize(&hex::decode(
        "0000ff3f9acaa5d26d392ace656c2428c991b0a3d3d773845a1300000000000000000000cd79ee4e64b40a2bbf130ca47d2d3cb0315d26cb96425bcb7dd82f375e8c6e739743d961ab980b17262f8aa5",
    )?)
    .context("decode catalogued nbits-retarget error-block header")
}

fn write_csv(
    header: &Header,
    chain: &str,
    child_height: i32,
    btc_height: i32,
    rejection_reason: &str,
) -> Result<PathBuf> {
    write_csv_with_expected_nbits(
        header,
        chain,
        child_height,
        btc_height,
        rejection_reason,
        header.bits.to_consensus(),
    )
}

fn write_csv_with_expected_nbits(
    header: &Header,
    chain: &str,
    child_height: i32,
    btc_height: i32,
    rejection_reason: &str,
    expected_nbits: u32,
) -> Result<PathBuf> {
    let path = temp_csv_path()?;
    let row = NormalizedCsvRow {
        chain,
        source_row_number: 1,
        classification: "error_block",
        relevance: "",
        relevance_reason: "",
        coinbase_script: &[],
        btc_height,
        child_height,
        child_hash: None,
    };
    let bits = format!("{:08x}", header.bits.to_consensus());
    let expected_nbits = format!("{expected_nbits:08x}");
    let line = normalized_csv_line(header, &row)
        .replacen("full_classifier_inventory", "error-block-observations", 1)
        .replacen(
            &format!(",error_block,,{bits},,,"),
            &format!(",error_block,VALID_ERROR_BLOCK,{expected_nbits},{rejection_reason},,"),
            1,
        );
    let header = format!(
        "{},rsk_miner,merge_mining_hash,is_uncle,uncle_index,uncle_parent_height,\
         rsk_merkle_proof,rsk_coinbase_tail\n",
        NORMALIZED_HEADER.trim_end()
    );
    let line = format!("{},,,,,,,\n", line.trim_end());
    std::fs::write(&path, format!("{header}{line}"))
        .with_context(|| format!("write error-observation fixture {}", path.display()))?;
    Ok(path)
}
