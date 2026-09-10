//! Pure normalized monitor-evidence row parsing.
//!
//! Child height, hash, header, time, and `nBits` are independent evidence.
//! Empty cells remain absent. No child value is derived from a scan counter,
//! Bitcoin parent field, or synthetic placeholder.
use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::{Hash as _, sha256d};
use mmm_capture::auxpow::validates_target;
use mmm_capture::btc_orphan::{BtcOrphanVerdict, strict_bip34_height_from_evidence};
use mmm_capture::capture::{
    HistoricalEventProvenance, NormalizedEventEvidence, RskEvidencePayload,
};
use mmm_capture::nbits_table::NbitsTable;
use mmm_capture::source_registry::ChildTargetLocation;

use super::config::HistoricalChainSpec;
use super::publication::NORMALIZED_COLUMNS;
use super::rsk_sidecar::RskSidecarColumns;

mod candidate;
mod parent_coinbase;
mod publication_state;
pub(super) use candidate::{candidate_from_record, error_observation_candidate_from_record};
pub(super) use publication_state::{
    ComparablePublicationState, ExpectedPublicationState, PublicationRowKey,
    publication_row_key_from_record, publication_state_from_record,
};

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceClassification {
    Canonical,
    Stale,
    StaleDescendant,
    Unknown,
    ErrorBlock,
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
    pub(super) parent_output_addresses: Vec<String>,
    pub(super) error_rejection_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SkipReason {
    EmptyField,
    MissingChildIdentity,
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
            Self::MissingChildIdentity => "missing_child_identity",
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
    coinbase_outputs: usize,
    full_coinbase: usize,
    classification: usize,
    validation_status: usize,
    expected_nbits: usize,
    rejection_reason: usize,
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
            coinbase_outputs: required_header(headers, "coinbase_outputs")?,
            full_coinbase: required_header(headers, "full_coinbase_hex")?,
            classification: required_header(headers, "classification")?,
            validation_status: required_header(headers, "validation_status")?,
            expected_nbits: required_header(headers, "expected_nbits")?,
            rejection_reason: required_header(headers, "rejection_reason")?,
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
    expected_nbits_must_match_header: bool,
) -> Result<(), SkipReason> {
    check_display_hash(record.get(layout.btc_hash), display_hash)?;
    check_display_hash(
        record.get(layout.btc_prev_hash),
        &header.prev_blockhash.to_string(),
    )?;
    check_optional_i64(record.get(layout.btc_time), i64::from(header.time))?;
    check_optional_u32_decimal(record.get(layout.btc_nonce), header.nonce)?;
    check_optional_compact_target(record.get(layout.btc_bits), header.bits.to_consensus())?;
    if expected_nbits_must_match_header {
        check_optional_compact_target(
            record.get(layout.expected_nbits),
            header.bits.to_consensus(),
        )?;
    }
    if !validates_target(header.block_hash(), header.bits) {
        return Err(SkipReason::TargetInvalid);
    }
    Ok(())
}

fn validate_child_bundle(
    child_target_location: ChildTargetLocation,
    child_hash: Option<&[u8]>,
    child_header: Option<&[u8]>,
    child_time: Option<i64>,
    child_nbits: Option<u32>,
) -> Result<(), SkipReason> {
    if child_target_location == ChildTargetLocation::PowData && child_nbits == Some(0) {
        return Err(SkipReason::EvidenceMismatch);
    }
    let Some(header) = child_header else {
        return Ok(());
    };
    if header.len() != Header::SIZE {
        return Err(SkipReason::Malformed);
    }
    if child_hash.is_some_and(|hash| sha256d::Hash::hash(header).to_byte_array().as_slice() != hash)
    {
        return Err(SkipReason::HashMismatch);
    }
    let header_time = u32::from_le_bytes(
        header[68..72]
            .try_into()
            .expect("80-byte child header has time field"),
    );
    if child_time.is_some_and(|time| i64::from(header_time) != time) {
        return Err(SkipReason::EvidenceMismatch);
    }
    let header_nbits = u32::from_le_bytes(
        header[72..76]
            .try_into()
            .expect("80-byte child header has nBits field"),
    );
    match child_target_location {
        ChildTargetLocation::HeaderNbits => {
            if child_nbits.is_some_and(|nbits| header_nbits != nbits) {
                return Err(SkipReason::EvidenceMismatch);
            }
        }
        ChildTargetLocation::PowData => {
            if header_nbits != 0 {
                return Err(SkipReason::EvidenceMismatch);
            }
        }
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
    match (verdict, selection) {
        (BtcOrphanVerdict::Strict, Some(RelevanceSelection::StrictBtcOrphan))
        | (BtcOrphanVerdict::Weak, Some(RelevanceSelection::WeakBtcOrphan))
        // The publication promotes every observation of a BTC header to the
        // strongest verdict independently attested by any chain. A chain such
        // as RSK can therefore carry a strict publication verdict even though
        // its coinbase-free local evidence supports only the weak path.
        | (BtcOrphanVerdict::Weak, Some(RelevanceSelection::StrictBtcOrphan)) => Ok(()),
        (BtcOrphanVerdict::Strict, Some(RelevanceSelection::WeakBtcOrphan)) => {
            Err(SkipReason::TaxonomyMismatch)
        }
        (BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak, _) => {
            Err(SkipReason::OrphanNotSelected)
        }
        (BtcOrphanVerdict::Excluded, _) => Err(SkipReason::OrphanExcluded),
        (BtcOrphanVerdict::Pending, _) => Err(SkipReason::OrphanPending),
    }
}

fn orphan_verdict(
    nbits_table: &NbitsTable,
    chain: &str,
    header: &Header,
    coinbase_script: Option<&[u8]>,
    coinbase_tx_bytes: Option<&[u8]>,
) -> BtcOrphanVerdict {
    let strict_height = coinbase_script
        .and_then(|script| strict_bip34_height_from_evidence(chain, script, coinbase_tx_bytes));
    mmm_capture::btc_orphan::classify_btc_orphan_with(
        nbits_table,
        header.time as i64,
        header.bits,
        strict_height,
    )
    .0
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
