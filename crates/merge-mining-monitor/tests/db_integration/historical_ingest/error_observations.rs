use std::path::PathBuf;

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, FakeParentClassifier, HeightSource, ParentClassification,
    TIME_BELOW_MTP,
};
use mmm_producers::run_error_observation_import_for_test;

use super::{
    NORMALIZED_HEADER, NormalizedCsvRow, active_source_event_count, finish_import_with_cleanup,
    normalized_csv_line, temp_csv_path,
};
use crate::support::header_meeting_bits;

#[tokio::test]
async fn import_requires_catalogue_match_before_writing() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let unsupported = header_meeting_bits(0x207f_ffff, 1_700_000_045, 45);
        let unsupported_path = write_csv(&unsupported, "devcoin", 45, 700_045, TIME_BELOW_MTP)?;
        let accepted = catalogued_header()?;
        let accepted_path = write_csv(&accepted, "devcoin", 46, 946_213, TIME_BELOW_MTP)?;
        let result = async {
            let skipped_classifier = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
                ParentClassification::unknown(&unsupported),
            ));
            let error = run_error_observation_import_for_test(
                &mut client,
                &skipped_classifier,
                &unsupported_path,
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
            let summary =
                run_error_observation_import_for_test(&mut client, &classifier, &accepted_path)
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

fn catalogued_header() -> Result<Header> {
    deserialize(&hex::decode(
        "00a0032bb223f1aad55892df75d0ff4712f0543959c5065ab89d000000000000000000005eba715327fc82c765fa651bd6226c4b4a6a846cd60197bcd76d47ada0611cfce335df696913021725806e70",
    )?)
    .context("decode catalogued time_below_mtp error-block header")
}

fn write_csv(
    header: &Header,
    chain: &str,
    child_height: i32,
    btc_height: i32,
    rejection_reason: &str,
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
    let line = normalized_csv_line(header, &row)
        .replacen("full_classifier_inventory", "error-block-observations", 1)
        .replacen(
            &format!(",error_block,,{bits},,,"),
            &format!(",error_block,VALID_ERROR_BLOCK,{bits},{rejection_reason},,"),
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
