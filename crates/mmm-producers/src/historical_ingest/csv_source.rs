//! Pure normalized monitor-evidence row parsing.
//!
//! Child height, hash, header, time, and `nBits` are independent evidence.
//! Empty cells remain absent. No child value is derived from a scan counter,
//! Bitcoin parent field, or synthetic placeholder.

use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::{Hash as _, sha256d};
use mmm_capture::auxpow::{parse_bip34_height, validates_target};
use mmm_capture::btc_orphan::{BtcOrphanVerdict, classify_btc_orphan, is_strict_bip34_chain};
use mmm_capture::capture::{
    HistoricalEventProvenance, NormalizedEventEvidence, RskEvidencePayload,
};

use super::config::{HistoricalChainSpec, PINNED_RESEARCH_COMMIT};
use super::publication::NORMALIZED_COLUMNS;
use super::rsk_sidecar::{RskSidecarColumns, parse_rsk_sidecar};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceClassification {
    Canonical,
    Stale,
    StaleDescendant,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelevanceSelection {
    StrictBtcOrphan,
    WeakBtcOrphan,
    KnownDirectStale,
    KnownStaleDescendant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PublicationCategory {
    Canonical,
    Stale,
    StaleDescendant,
    StrictBtcOrphan,
    WeakBtcOrphan,
}

#[derive(Debug, Clone)]
pub(super) struct ImportCandidate {
    pub(super) source_classification: SourceClassification,
    pub(super) evidence: NormalizedEventEvidence,
    pub(super) historical_provenance: HistoricalEventProvenance,
    pub(super) btc_parent_display_hash: String,
    pub(super) orphan_verdict: Option<BtcOrphanVerdict>,
    pub(super) relevance_selection: Option<RelevanceSelection>,
    pub(super) rsk_evidence: Option<RskEvidencePayload>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SkipReason {
    EmptyField,
    Malformed,
    HashMismatch,
    EvidenceMismatch,
    TaxonomyMismatch,
    TargetInvalid,
    UnsupportedClassification,
    Near,
    OrphanNotSelected,
    OrphanExcluded,
    OrphanPending,
    ClassificationMismatch,
    Unclassified,
}

impl SkipReason {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::EmptyField => "empty_field",
            Self::Malformed => "malformed",
            Self::HashMismatch => "hash_mismatch",
            Self::EvidenceMismatch => "evidence_mismatch",
            Self::TaxonomyMismatch => "taxonomy_mismatch",
            Self::TargetInvalid => "target_invalid",
            Self::UnsupportedClassification => "unsupported_classification",
            Self::Near => "near",
            Self::OrphanNotSelected => "orphan_not_selected",
            Self::OrphanExcluded => "orphan_excluded",
            Self::OrphanPending => "orphan_pending",
            Self::ClassificationMismatch => "classification_mismatch",
            Self::Unclassified => "unclassified",
        }
    }
}

pub(super) struct CsvLayout {
    chain: usize,
    source_kind: usize,
    source_path: usize,
    source_row_number: usize,
    artifact_scope: usize,
    provenance: usize,
    child_height: usize,
    child_hash: usize,
    child_header: usize,
    child_time: usize,
    child_nbits: usize,
    btc_height: usize,
    btc_hash: usize,
    btc_prev_hash: usize,
    btc_time: usize,
    btc_bits: usize,
    btc_nonce: usize,
    btc_header: usize,
    coinbase_script: usize,
    classification: usize,
    validation_status: usize,
    expected_nbits: usize,
    relevance: usize,
    relevance_reason: usize,
    rsk_sidecar: Option<RskSidecarColumns>,
}

impl CsvLayout {
    pub(super) fn new(headers: &csv::StringRecord, spec: &HistoricalChainSpec) -> Result<Self> {
        for column in NORMALIZED_COLUMNS {
            required_header(headers, column)?;
        }
        Ok(Self {
            chain: required_header(headers, "chain")?,
            source_kind: required_header(headers, "source_kind")?,
            source_path: required_header(headers, "source_path")?,
            source_row_number: required_header(headers, "source_row_number")?,
            artifact_scope: required_header(headers, "artifact_scope")?,
            provenance: required_header(headers, "provenance")?,
            child_height: required_header(headers, "child_height")?,
            child_hash: required_header(headers, "child_block_hash")?,
            child_header: required_header(headers, "child_header_hex")?,
            child_time: required_header(headers, "child_block_time")?,
            child_nbits: required_header(headers, "child_nbits")?,
            btc_height: required_header(headers, "btc_height")?,
            btc_hash: required_header(headers, "btc_header_hash")?,
            btc_prev_hash: required_header(headers, "btc_prev_hash")?,
            btc_time: required_header(headers, "btc_time")?,
            btc_bits: required_header(headers, "btc_bits")?,
            btc_nonce: required_header(headers, "btc_nonce")?,
            btc_header: required_header(headers, "btc_header_hex")?,
            coinbase_script: required_header(headers, "coinbase_scriptsig_hex")?,
            classification: required_header(headers, "classification")?,
            validation_status: required_header(headers, "validation_status")?,
            expected_nbits: required_header(headers, "expected_nbits")?,
            relevance: required_header(headers, "btc_stale_relevance")?,
            relevance_reason: required_header(headers, "relevance_reason")?,
            rsk_sidecar: if spec.chain == "rsk" {
                Some(RskSidecarColumns::new(headers)?)
            } else {
                None
            },
        })
    }
}

struct ChildFields {
    height: Option<i32>,
    block_hash: Option<Vec<u8>>,
    header_bytes: Option<Vec<u8>>,
    block_time: Option<i64>,
    nbits: Option<u32>,
}

struct TaxonomyFields {
    source_classification: SourceClassification,
    relevance_selection: Option<RelevanceSelection>,
    provenance: HistoricalEventProvenance,
}

pub(super) fn candidate_from_record(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
) -> Result<ImportCandidate, SkipReason> {
    if record.get(layout.chain).map(str::trim) != Some(spec.chain) {
        return Err(SkipReason::Malformed);
    }
    let child = parse_child_fields(spec, layout, record)?;
    let taxonomy = parse_taxonomy_fields(spec, layout, record)?;
    let header = parse_parent_header(record.get(layout.btc_header))?;
    let display_hash = header.block_hash().to_string();
    validate_parent_fields(layout, record, &header, &display_hash)?;

    let coinbase_script = parse_optional_hex_field(record.get(layout.coinbase_script))?;
    let orphan_verdict = if taxonomy.source_classification == SourceClassification::Unknown {
        let verdict = orphan_verdict(spec.chain, &header, coinbase_script.as_deref());
        filter_unknown(verdict, taxonomy.relevance_selection)?;
        Some(verdict)
    } else {
        None
    };
    let rsk_evidence = if let Some(columns) = &layout.rsk_sidecar {
        Some(parse_rsk_sidecar(
            columns,
            record,
            child.height.ok_or(SkipReason::EmptyField)?,
            child.block_hash.as_deref().ok_or(SkipReason::EmptyField)?,
        )?)
    } else {
        None
    };
    Ok(ImportCandidate {
        source_classification: taxonomy.source_classification,
        evidence: NormalizedEventEvidence {
            child_height: child.height,
            child_block_hash: child.block_hash,
            child_header_bytes: child.header_bytes,
            child_block_time: child.block_time,
            child_nbits: child.nbits,
            btc_parent_header: header,
            pow_validates_child_target: None,
            btc_parent_coinbase_txid: None,
            btc_parent_coinbase_script: coinbase_script,
            btc_parent_coinbase_outputs: None,
            child_coinbase_txid: None,
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            aux_merkle_proof: None,
        },
        historical_provenance: taxonomy.provenance,
        btc_parent_display_hash: display_hash,
        orphan_verdict,
        relevance_selection: taxonomy.relevance_selection,
        rsk_evidence,
    })
}

fn parse_child_fields(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
) -> Result<ChildFields, SkipReason> {
    let height = parse_optional_nonnegative_i32(record.get(layout.child_height))?;
    let block_hash = parse_optional_hash_field(record.get(layout.child_hash))?;
    if height.is_none() && block_hash.is_none() {
        return Err(SkipReason::EmptyField);
    }
    let header_bytes = parse_optional_hex_field(record.get(layout.child_header))?;
    let block_time = parse_optional_nonnegative_i64(record.get(layout.child_time))?;
    let nbits = parse_optional_compact_target(record.get(layout.child_nbits))?;
    validate_child_bundle(
        spec.chain,
        block_hash.as_deref(),
        header_bytes.as_deref(),
        block_time,
        nbits,
    )?;
    Ok(ChildFields {
        height,
        block_hash,
        header_bytes,
        block_time,
        nbits,
    })
}

fn parse_taxonomy_fields(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
) -> Result<TaxonomyFields, SkipReason> {
    let source_classification = parse_source_classification(record.get(layout.classification))?;
    let validation_status = optional_string(record.get(layout.validation_status));
    if matches!(
        source_classification,
        SourceClassification::Stale | SourceClassification::StaleDescendant
    ) && !validation_status
        .as_deref()
        .unwrap_or_default()
        .starts_with("VALID")
    {
        return Err(SkipReason::TaxonomyMismatch);
    }
    let relevance = optional_string(record.get(layout.relevance));
    let relevance_reason = optional_string(record.get(layout.relevance_reason));
    let category = publication_category(
        record
            .get(layout.classification)
            .map(str::trim)
            .unwrap_or_default(),
        relevance.as_deref().unwrap_or_default(),
        relevance_reason.as_deref().unwrap_or_default(),
    )?;
    let relevance_selection = match category {
        PublicationCategory::Canonical => None,
        PublicationCategory::Stale => Some(RelevanceSelection::KnownDirectStale),
        PublicationCategory::StaleDescendant => Some(RelevanceSelection::KnownStaleDescendant),
        PublicationCategory::StrictBtcOrphan => Some(RelevanceSelection::StrictBtcOrphan),
        PublicationCategory::WeakBtcOrphan => Some(RelevanceSelection::WeakBtcOrphan),
    };
    Ok(TaxonomyFields {
        source_classification,
        relevance_selection,
        provenance: HistoricalEventProvenance {
            publication_commit: PINNED_RESEARCH_COMMIT.to_owned(),
            chain: spec.chain.to_owned(),
            source_kind: non_empty(record.get(layout.source_kind))?.to_owned(),
            source_path: non_empty(record.get(layout.source_path))?.to_owned(),
            source_row_number: parse_positive_i64(record.get(layout.source_row_number))?,
            artifact_scope: non_empty(record.get(layout.artifact_scope))?.to_owned(),
            provenance: non_empty(record.get(layout.provenance))?.to_owned(),
            classification: non_empty(record.get(layout.classification))?.to_owned(),
            btc_height: parse_optional_nonnegative_i32(record.get(layout.btc_height))?,
            validation_status,
            btc_stale_relevance: relevance,
            relevance_reason,
        },
    })
}

pub(super) fn publication_category(
    classification: &str,
    relevance: &str,
    relevance_reason: &str,
) -> Result<PublicationCategory, SkipReason> {
    match (classification, relevance, relevance_reason) {
        ("stale" | "unknown", "", "valid_direct_stale") => Ok(PublicationCategory::Stale),
        ("stale_descendant" | "unknown", "", "valid_stale_descendant") => {
            Ok(PublicationCategory::StaleDescendant)
        }
        (_, _, "valid_direct_stale" | "valid_stale_descendant") => {
            Err(SkipReason::TaxonomyMismatch)
        }
        ("canonical", "", _) => Ok(PublicationCategory::Canonical),
        ("unknown", "strict_btc_orphan", _) => Ok(PublicationCategory::StrictBtcOrphan),
        ("unknown", "weak_btc_orphan", _) => Ok(PublicationCategory::WeakBtcOrphan),
        ("near", _, _) => Err(SkipReason::Near),
        ("canonical" | "stale" | "stale_descendant" | "unknown", _, _) => {
            Err(SkipReason::TaxonomyMismatch)
        }
        _ => Err(SkipReason::UnsupportedClassification),
    }
}

fn validate_parent_fields(
    layout: &CsvLayout,
    record: &csv::StringRecord,
    header: &Header,
    display_hash: &str,
) -> Result<(), SkipReason> {
    check_display_hash(record.get(layout.btc_hash), display_hash)?;
    check_display_hash(
        record.get(layout.btc_prev_hash),
        &header.prev_blockhash.to_string(),
    )?;
    check_optional_i64(record.get(layout.btc_time), i64::from(header.time))?;
    check_optional_u32_decimal(record.get(layout.btc_nonce), header.nonce)?;
    check_optional_compact_target(record.get(layout.btc_bits), header.bits.to_consensus())?;
    check_optional_compact_target(
        record.get(layout.expected_nbits),
        header.bits.to_consensus(),
    )?;
    if !validates_target(header.block_hash(), header.bits) {
        return Err(SkipReason::TargetInvalid);
    }
    Ok(())
}

fn validate_child_bundle(
    chain: &str,
    child_hash: Option<&[u8]>,
    child_header: Option<&[u8]>,
    child_time: Option<i64>,
    child_nbits: Option<u32>,
) -> Result<(), SkipReason> {
    let Some(header) = child_header else {
        return Ok(());
    };
    if header.len() != Header::SIZE {
        return Err(SkipReason::Malformed);
    }
    let hash = child_hash.ok_or(SkipReason::EvidenceMismatch)?;
    let time = child_time.ok_or(SkipReason::EvidenceMismatch)?;
    let nbits = child_nbits.ok_or(SkipReason::EvidenceMismatch)?;
    if sha256d::Hash::hash(header).to_byte_array().as_slice() != hash {
        return Err(SkipReason::HashMismatch);
    }
    let header_time = u32::from_le_bytes(
        header[68..72]
            .try_into()
            .expect("80-byte child header has time field"),
    );
    if i64::from(header_time) != time {
        return Err(SkipReason::EvidenceMismatch);
    }
    let header_nbits = u32::from_le_bytes(
        header[72..76]
            .try_into()
            .expect("80-byte child header has nBits field"),
    );
    if chain != "xaya" && header_nbits != nbits {
        return Err(SkipReason::EvidenceMismatch);
    }
    Ok(())
}

fn filter_unknown(
    verdict: BtcOrphanVerdict,
    selection: Option<RelevanceSelection>,
) -> Result<(), SkipReason> {
    if matches!(
        selection,
        Some(RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant)
    ) {
        return Ok(());
    }
    if !matches!(
        selection,
        Some(RelevanceSelection::StrictBtcOrphan | RelevanceSelection::WeakBtcOrphan)
    ) {
        return Err(SkipReason::OrphanNotSelected);
    }
    match verdict {
        BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak => Ok(()),
        BtcOrphanVerdict::Excluded => Err(SkipReason::OrphanExcluded),
        BtcOrphanVerdict::Pending => Err(SkipReason::OrphanPending),
    }
}

fn orphan_verdict(
    chain: &str,
    header: &Header,
    coinbase_script: Option<&[u8]>,
) -> BtcOrphanVerdict {
    let strict_height = is_strict_bip34_chain(chain)
        .then(|| coinbase_script.and_then(parse_bip34_height))
        .flatten();
    classify_btc_orphan(header.time as i64, header.bits, strict_height).0
}

pub(super) fn required_header(headers: &csv::StringRecord, name: &str) -> Result<usize> {
    headers
        .iter()
        .position(|header| header.trim() == name)
        .ok_or_else(|| anyhow::anyhow!("CSV missing required column {name}"))
}

fn parse_source_classification(value: Option<&str>) -> Result<SourceClassification, SkipReason> {
    match non_empty(value)? {
        "canonical" => Ok(SourceClassification::Canonical),
        "stale" => Ok(SourceClassification::Stale),
        "stale_descendant" => Ok(SourceClassification::StaleDescendant),
        "unknown" => Ok(SourceClassification::Unknown),
        "near" => Err(SkipReason::Near),
        _ => Err(SkipReason::UnsupportedClassification),
    }
}

fn parse_parent_header(value: Option<&str>) -> Result<Header, SkipReason> {
    let raw = parse_hex_field(value)?;
    if raw.len() != Header::SIZE {
        return Err(SkipReason::Malformed);
    }
    deserialize(&raw).map_err(|_| SkipReason::Malformed)
}

pub(super) fn parse_hex_field(value: Option<&str>) -> Result<Vec<u8>, SkipReason> {
    hex::decode(non_empty(value)?).map_err(|_| SkipReason::Malformed)
}

pub(super) fn parse_optional_hex_field(value: Option<&str>) -> Result<Option<Vec<u8>>, SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        Ok(None)
    } else {
        hex::decode(value)
            .map(Some)
            .map_err(|_| SkipReason::Malformed)
    }
}

fn parse_optional_hash_field(value: Option<&str>) -> Result<Option<Vec<u8>>, SkipReason> {
    let Some(bytes) = parse_optional_hex_field(value)? else {
        return Ok(None);
    };
    if bytes.len() == 32 {
        Ok(Some(bytes))
    } else {
        Err(SkipReason::Malformed)
    }
}

fn parse_optional_nonnegative_i32(value: Option<&str>) -> Result<Option<i32>, SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<i32>() {
        Ok(parsed) if parsed >= 0 => Ok(Some(parsed)),
        _ => Err(SkipReason::Malformed),
    }
}

fn parse_optional_nonnegative_i64(value: Option<&str>) -> Result<Option<i64>, SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Ok(None);
    }
    match value.parse::<i64>() {
        Ok(parsed) if parsed >= 0 => Ok(Some(parsed)),
        _ => Err(SkipReason::Malformed),
    }
}

fn parse_positive_i64(value: Option<&str>) -> Result<i64, SkipReason> {
    match non_empty(value)?.parse::<i64>() {
        Ok(parsed) if parsed > 0 => Ok(parsed),
        _ => Err(SkipReason::Malformed),
    }
}

fn optional_string(value: Option<&str>) -> Option<String> {
    let value = value.map(str::trim).unwrap_or_default();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_optional_compact_target(value: Option<&str>) -> Result<Option<u32>, SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Ok(None);
    }
    let value = value.strip_prefix("0x").unwrap_or(value);
    if value.is_empty() || value.len() > 8 {
        return Err(SkipReason::Malformed);
    }
    u32::from_str_radix(value, 16)
        .map(Some)
        .map_err(|_| SkipReason::Malformed)
}

fn check_display_hash(value: Option<&str>, expected: &str) -> Result<(), SkipReason> {
    let value = non_empty(value)?;
    if value.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(SkipReason::HashMismatch)
    }
}

fn check_optional_i64(value: Option<&str>, expected: i64) -> Result<(), SkipReason> {
    if parse_optional_nonnegative_i64(value)?.is_none_or(|value| value == expected) {
        Ok(())
    } else {
        Err(SkipReason::EvidenceMismatch)
    }
}

fn check_optional_u32_decimal(value: Option<&str>, expected: u32) -> Result<(), SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        return Ok(());
    }
    if value.parse::<u32>().is_ok_and(|value| value == expected) {
        Ok(())
    } else {
        Err(SkipReason::EvidenceMismatch)
    }
}

fn check_optional_compact_target(value: Option<&str>, expected: u32) -> Result<(), SkipReason> {
    if parse_optional_compact_target(value)?.is_none_or(|value| value == expected) {
        Ok(())
    } else {
        Err(SkipReason::EvidenceMismatch)
    }
}

pub(super) fn non_empty(value: Option<&str>) -> Result<&str, SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        Err(SkipReason::EmptyField)
    } else {
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::super::config::historical_chain_spec;
    use super::*;

    const GENESIS_HEADER: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
    const GENESIS_HASH: &str = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

    #[derive(Default)]
    struct TestRow<'a> {
        chain: &'a str,
        child_height: &'a str,
        child_hash: &'a str,
        child_header: &'a str,
        child_time: &'a str,
        child_nbits: &'a str,
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
            classification,
            relevance,
            relevance_reason,
        } = value;
        format!(
            "{chain},full_inventory,<archive>,1,full_classifier_inventory,archive,\
             {child_height},{child_hash},{child_header},{child_time},{child_nbits},\
             0,{GENESIS_HASH},{},{},1d00ffff,2083236893,{GENESIS_HEADER},\
             04ffff001d0104,,,{},VALID,1d00ffff,,{relevance},{relevance_reason}\n",
            "0".repeat(64),
            1_231_006_505,
            classification
        )
    }

    fn candidate(chain: &str, row: &str) -> Result<ImportCandidate, SkipReason> {
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
        candidate_from_record(spec, &layout, &record)
    }

    fn child_identity() -> (String, String) {
        let raw = hex::decode(GENESIS_HEADER).unwrap();
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
    fn rejects_identity_free_rows_and_partial_header_bundles() {
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
            SkipReason::EmptyField
        );
        let (hash, header) = child_identity();
        assert_eq!(
            candidate(
                "devcoin",
                &row(TestRow {
                    chain: "devcoin",
                    child_height: "42",
                    child_hash: &hash,
                    child_header: &header,
                    child_nbits: "1d00ffff",
                    classification: "stale",
                    relevance_reason: "valid_direct_stale",
                    ..TestRow::default()
                }),
            )
            .unwrap_err(),
            SkipReason::EvidenceMismatch
        );
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
    fn xaya_uses_its_external_authenticated_child_target() {
        let (hash, header) = child_identity();
        let parsed = candidate(
            "xaya",
            &row(TestRow {
                chain: "xaya",
                child_height: "42",
                child_hash: &hash,
                child_header: &header,
                child_time: "1231006505",
                child_nbits: "184c238c",
                classification: "stale",
                relevance_reason: "valid_direct_stale",
                ..TestRow::default()
            }),
        )
        .unwrap();
        assert_eq!(parsed.evidence.child_nbits, Some(0x184c238c));
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
        let parsed = candidate(
            "devcoin",
            &row(TestRow {
                chain: "devcoin",
                child_height: "42",
                classification: "stale_descendant",
                relevance_reason: "valid_stale_descendant",
                ..TestRow::default()
            }),
        )
        .unwrap();
        assert_eq!(
            parsed.source_classification,
            SourceClassification::StaleDescendant
        );
    }
}
