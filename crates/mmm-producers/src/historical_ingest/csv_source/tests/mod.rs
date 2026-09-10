use mmm_capture::nbits_table::{BitcoinEpochHeader, NbitsTable};

use super::super::config::{PINNED_RESEARCH_COMMIT, historical_chain_spec};
use super::*;

mod hathor_coinbase;
mod review_regressions;

const GENESIS_HEADER: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
const GENESIS_HASH: &str = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";
const GENESIS_COINBASE: &str = "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000";
const GENESIS_COINBASE_SCRIPT: &str = "04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73";
const RSK_CHILD_HASH: &str = "863002b6ad9a940f191f3ed3289e42e8eee107a769b6ecdfdaaad747f70c981d";
const RSK_MINER: &str = "32dfc7a84f24b10a5dded1d8b24f48b96ab77373";
const RSK_MERGE_MINING_HASH: &str =
    "f0d9129c65b3b91a89355b9ccf975e55c29229d78d4a66201b83d409ae001f73";

#[derive(Default)]
struct TestRow<'a> {
    chain: &'a str,
    child_height: &'a str,
    child_hash: &'a str,
    child_header: &'a str,
    child_time: &'a str,
    child_nbits: &'a str,
    coinbase_script: &'a str,
    coinbase_outputs: &'a str,
    full_coinbase: &'a str,
    classification: &'a str,
    relevance: &'a str,
    relevance_reason: &'a str,
}

fn row(value: TestRow<'_>) -> String {
    let TestRow {
        chain,
        child_height,
        child_hash,
        child_header,
        child_time,
        child_nbits,
        coinbase_script,
        coinbase_outputs,
        full_coinbase,
        classification,
        relevance,
        relevance_reason,
    } = value;
    let coinbase_script = if coinbase_script.is_empty() {
        "04ffff001d0104"
    } else {
        coinbase_script
    };
    format!(
        "{chain},full_inventory,<archive>,1,full_classifier_inventory,archive,\
         {child_height},{child_hash},{child_header},{child_time},{child_nbits},\
         0,{GENESIS_HASH},{},{},1d00ffff,2083236893,{GENESIS_HEADER},\
         {coinbase_script},{coinbase_outputs},{full_coinbase},{},VALID,1d00ffff,,{relevance},{relevance_reason}\n",
        "0".repeat(64),
        1_231_006_505,
        classification
    )
}

fn candidate(chain: &str, row: &str) -> Result<ImportCandidate, SkipReason> {
    candidate_with_nbits_table(chain, row, None)
}

fn candidate_with_nbits_table(
    chain: &str,
    row: &str,
    nbits_table: Option<&NbitsTable>,
) -> Result<ImportCandidate, SkipReason> {
    let spec = historical_chain_spec(chain).unwrap();
    let mut input = NORMALIZED_COLUMNS.join(",");
    if chain == "rsk" {
        input.push_str(
            ",rsk_miner,merge_mining_hash,is_uncle,uncle_index,\
             uncle_parent_height,rsk_merkle_proof,rsk_coinbase_tail",
        );
    }
    input.push('\n');
    input.push_str(row);
    let mut reader = csv::Reader::from_reader(input.as_bytes());
    let layout = CsvLayout::new(reader.headers().unwrap(), spec).unwrap();
    let record = reader.records().next().unwrap().unwrap();
    candidate_from_record(
        spec,
        &layout,
        &record,
        PINNED_RESEARCH_COMMIT.as_str(),
        nbits_table,
    )
}

fn error_observation_candidate(chain: &str, row: &str) -> Result<ImportCandidate, SkipReason> {
    error_observation_candidate_with_expected_nbits(chain, row, 0x1d00_ffff)
}

fn error_observation_candidate_with_expected_nbits(
    chain: &str,
    row: &str,
    expected_nbits: u32,
) -> Result<ImportCandidate, SkipReason> {
    let spec = historical_chain_spec(chain).unwrap();
    let mut input = NORMALIZED_COLUMNS.join(",");
    input.push_str(",rsk_miner,merge_mining_hash,is_uncle,uncle_index,");
    input.push_str("uncle_parent_height,rsk_merkle_proof,rsk_coinbase_tail\n");
    input.push_str(row.trim_end());
    input.push_str(",,,,,,,\n");
    let mut reader = csv::Reader::from_reader(input.as_bytes());
    let layout = CsvLayout::new(reader.headers().unwrap(), spec).unwrap();
    let record = reader.records().next().unwrap().unwrap();
    let nbits_table = NbitsTable::from_bitcoin_core_headers(&[BitcoinEpochHeader {
        height: 0,
        block_time: 0,
        bits: expected_nbits,
    }])
    .unwrap();
    error_observation_candidate_from_record(
        spec,
        &layout,
        &record,
        PINNED_RESEARCH_COMMIT.as_str(),
        &nbits_table,
    )
}

fn child_identity() -> (String, String) {
    child_identity_with_nbits(0x1d00_ffff)
}

fn child_identity_with_nbits(nbits: u32) -> (String, String) {
    let mut raw = hex::decode(GENESIS_HEADER).unwrap();
    raw[72..76].copy_from_slice(&nbits.to_le_bytes());
    let hash = sha256d::Hash::hash(&raw).to_byte_array();
    (hex::encode(hash), hex::encode(raw))
}

#[test]
fn requires_the_uniform_schema_for_every_chain() {
    let headers = csv::StringRecord::from(vec!["chain", "child_height", "btc_header_hex"]);
    let error = CsvLayout::new(&headers, historical_chain_spec("devcoin").unwrap())
        .err()
        .expect("incomplete schema must fail");
    assert!(error.to_string().contains("source_kind"));
}

#[test]
fn authenticates_and_preserves_a_complete_child_header_bundle() {
    let (hash, header) = child_identity();
    let parsed = candidate(
        "devcoin",
        &row(TestRow {
            chain: "devcoin",
            child_height: "42",
            child_hash: &hash,
            child_header: &header,
            child_time: "1231006505",
            child_nbits: "1d00ffff",
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        }),
    )
    .unwrap();
    assert_eq!(parsed.evidence.child_height, Some(42));
    assert_eq!(
        parsed.evidence.child_block_hash,
        Some(hex::decode(hash).unwrap())
    );
    assert_eq!(
        parsed.evidence.child_header_bytes,
        Some(hex::decode(header).unwrap())
    );
    assert_eq!(parsed.evidence.child_block_time, Some(1_231_006_505));
    assert_eq!(parsed.evidence.child_nbits, Some(0x1d00ffff));
    assert_eq!(parsed.evidence.pow_validates_child_target, Some(true));
}

#[test]
fn supports_exact_identity_without_height() {
    let (hash, header) = child_identity();
    let parsed = candidate(
        "i0coin",
        &row(TestRow {
            chain: "i0coin",
            child_hash: &hash,
            child_header: &header,
            child_time: "1231006505",
            child_nbits: "1d00ffff",
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        }),
    )
    .unwrap();
    assert_eq!(parsed.evidence.child_height, None);
    assert!(parsed.evidence.child_block_hash.is_some());
}

#[test]
fn supports_height_only_child_evidence_without_fabrication() {
    let parsed = candidate(
        "elastos",
        &row(TestRow {
            chain: "elastos",
            child_height: "360062",
            classification: "canonical",
            relevance_reason: "canonical_parent",
            ..TestRow::default()
        }),
    )
    .unwrap();
    assert_eq!(parsed.evidence.child_height, Some(360_062));
    assert_eq!(parsed.evidence.child_block_hash, None);
    assert_eq!(parsed.evidence.child_header_bytes, None);
    assert_eq!(parsed.evidence.child_block_time, None);
    assert_eq!(parsed.evidence.child_nbits, None);
}

#[test]
fn rsk_candidate_carries_sidecar_keyed_by_child_identity() {
    let mut input = row(TestRow {
        chain: "rsk",
        child_height: "263443",
        child_hash: RSK_CHILD_HASH,
        classification: "stale",
        relevance_reason: "valid_direct_stale",
        ..TestRow::default()
    });
    input.pop();
    input.push_str(&format!(",{RSK_MINER},{RSK_MERGE_MINING_HASH},0,,,0405,\n"));

    let parsed = candidate("rsk", &input).unwrap();
    let evidence = parsed.rsk_evidence.expect("RSK row carries sidecar");
    assert_eq!(
        evidence.rsk_block_hash,
        parsed.evidence.child_block_hash.unwrap()
    );
    assert_eq!(evidence.rsk_height, parsed.evidence.child_height.unwrap());
    assert!(!evidence.is_uncle);
    assert_eq!(evidence.merkle_proof.as_deref(), Some(&[0x04, 0x05][..]));
}

#[test]
fn error_observation_parser_is_separate_from_valid_evidence() {
    let error_row = row(TestRow {
        chain: "devcoin",
        child_height: "42",
        classification: "error_block",
        ..TestRow::default()
    })
    .replacen("full_classifier_inventory", "error-block-observations", 1)
    .replacen(
        ",VALID,1d00ffff,,",
        ",VALID_ERROR_BLOCK,1d00ffff,time_below_mtp,",
        1,
    );

    assert_eq!(
        candidate("devcoin", &error_row).unwrap_err(),
        SkipReason::UnsupportedClassification
    );
    let parsed = error_observation_candidate("devcoin", &error_row).unwrap();
    assert_eq!(
        parsed.source_classification,
        SourceClassification::ErrorBlock
    );
    assert_eq!(
        parsed.error_rejection_reason.as_deref(),
        Some("time_below_mtp")
    );
    assert_eq!(
        parsed.historical_provenance.artifact_scope,
        "error-block-observations"
    );
    assert_eq!(parsed.historical_provenance.btc_stale_relevance, None);
}

#[test]
fn ordinary_parser_rejects_the_reserved_error_observation_scope() {
    let ordinary_row = row(TestRow {
        chain: "devcoin",
        child_height: "42",
        classification: "canonical",
        relevance_reason: "canonical_parent",
        ..TestRow::default()
    })
    .replacen("full_classifier_inventory", "error-block-observations", 1);

    assert_eq!(
        candidate("devcoin", &ordinary_row).unwrap_err(),
        SkipReason::TaxonomyMismatch
    );
}

#[test]
fn error_observation_allows_catalogued_expected_nbits_mismatch() {
    let error_row = row(TestRow {
        chain: "devcoin",
        child_height: "42",
        classification: "error_block",
        ..TestRow::default()
    })
    .replacen("full_classifier_inventory", "error-block-observations", 1)
    .replacen(
        ",VALID,1d00ffff,,",
        ",VALID_ERROR_BLOCK,1d00fffe,nbits_retarget_not_applied,",
        1,
    );

    assert!(
        error_observation_candidate_with_expected_nbits("devcoin", &error_row, 0x1d00_fffe).is_ok()
    );

    let wrong_expected_nbits = error_row.replacen("1d00fffe", "1d00ffff", 1);
    assert_eq!(
        error_observation_candidate_with_expected_nbits(
            "devcoin",
            &wrong_expected_nbits,
            0x1d00_fffe,
        )
        .unwrap_err(),
        SkipReason::EvidenceMismatch
    );

    let non_retarget = error_row.replacen("nbits_retarget_not_applied", "time_below_mtp", 1);
    assert_eq!(
        error_observation_candidate("devcoin", &non_retarget).unwrap_err(),
        SkipReason::EvidenceMismatch
    );
}

#[test]
fn preserves_published_output_text_and_derives_payout_addresses() {
    let outputs = "76a914000000000000000000000000000000000000000088ac;OP_RETURN:0";
    let parsed = candidate(
        "devcoin",
        &row(TestRow {
            chain: "devcoin",
            child_height: "42",
            coinbase_outputs: outputs,
            classification: "canonical",
            relevance_reason: "canonical_parent",
            ..TestRow::default()
        }),
    )
    .unwrap();
    assert_eq!(
        parsed.evidence.btc_parent_coinbase_outputs_text.as_deref(),
        Some(outputs)
    );
    assert_eq!(parsed.parent_output_addresses.len(), 1);
}

#[test]
fn classifies_identity_free_canonical_rows_as_non_importable() {
    assert_eq!(
        candidate(
            "devcoin",
            &row(TestRow {
                chain: "devcoin",
                classification: "canonical",
                relevance_reason: "canonical_parent",
                ..TestRow::default()
            }),
        )
        .unwrap_err(),
        SkipReason::MissingChildIdentity
    );
    assert_eq!(
        candidate(
            "devcoin",
            &row(TestRow {
                chain: "devcoin",
                classification: "stale",
                relevance_reason: "valid_direct_stale",
                ..TestRow::default()
            }),
        )
        .unwrap_err(),
        SkipReason::EmptyField
    );
    let malformed_parent = row(TestRow {
        chain: "devcoin",
        classification: "canonical",
        relevance_reason: "canonical_parent",
        ..TestRow::default()
    })
    .replacen(GENESIS_HEADER, "", 1);
    assert_eq!(
        candidate("devcoin", &malformed_parent).unwrap_err(),
        SkipReason::EmptyField
    );
}

#[test]
fn accepts_independent_header_companions() {
    let (hash, header) = child_identity();
    for partial in [
        TestRow {
            chain: "devcoin",
            child_height: "42",
            child_header: &header,
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        },
        TestRow {
            chain: "devcoin",
            child_hash: &hash,
            child_header: &header,
            child_nbits: "1d00ffff",
            classification: "stale",
            relevance_reason: "valid_direct_stale",
            ..TestRow::default()
        },
    ] {
        candidate("devcoin", &row(partial)).unwrap();
    }
}

#[test]
fn rejects_child_hash_time_and_nbits_contradictions() {
    let (hash, header) = child_identity();
    for (bad_hash, bad_time, bad_nbits) in [
        ("11".repeat(32), "1231006505", "1d00ffff"),
        (hash.clone(), "1231006506", "1d00ffff"),
        (hash.clone(), "1231006505", "1d00fffe"),
    ] {
        let error = candidate(
            "devcoin",
            &row(TestRow {
                chain: "devcoin",
                child_height: "42",
                child_hash: &bad_hash,
                child_header: &header,
                child_time: bad_time,
                child_nbits: bad_nbits,
                classification: "stale",
                relevance_reason: "valid_direct_stale",
                ..TestRow::default()
            }),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            SkipReason::HashMismatch | SkipReason::EvidenceMismatch
        ));
    }
}

#[test]
fn classification_and_relevance_axes_must_agree() {
    assert_eq!(
        candidate(
            "devcoin",
            &row(TestRow {
                chain: "devcoin",
                child_height: "42",
                classification: "unknown",
                relevance: "strict_btc_orphan",
                relevance_reason: "valid_stale_descendant",
                ..TestRow::default()
            }),
        )
        .unwrap_err(),
        SkipReason::TaxonomyMismatch
    );
    assert_eq!(
        candidate(
            "devcoin",
            &row(TestRow {
                chain: "devcoin",
                child_height: "42",
                classification: "stale_descendant",
                relevance_reason: "valid_direct_stale",
                ..TestRow::default()
            }),
        )
        .unwrap_err(),
        SkipReason::TaxonomyMismatch
    );
    let input = row(TestRow {
        chain: "devcoin",
        child_height: "42",
        classification: "stale_descendant",
        relevance_reason: "valid_stale_descendant",
        ..TestRow::default()
    })
    .replacen(",VALID,1d00ffff", ",VALID_STALE_DESCENDANT,1d00ffff", 1);
    let parsed = candidate("devcoin", &input).unwrap();
    assert_eq!(
        parsed.source_classification,
        SourceClassification::StaleDescendant
    );
}
