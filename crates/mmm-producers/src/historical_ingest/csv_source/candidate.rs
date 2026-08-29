//! Candidate decoding shared by ordinary and error-observation artifacts.

use anyhow::Result;
use bitcoin::CompactTarget;
use bitcoin::hashes::{Hash as _, sha256d};
use mmm_capture::auxpow::validates_target;
use mmm_capture::capture::{HistoricalEventProvenance, NormalizedEventEvidence};
use mmm_capture::nbits_table::{NbitsLookup, NbitsTable};

use super::super::config::HistoricalChainSpec;
use super::super::rsk_sidecar::parse_rsk_sidecar;
use super::parent_coinbase::parse_parent_coinbase_fields;
use super::{
    CsvLayout, ImportCandidate, PublicationCategory, RelevanceSelection, SkipReason,
    SourceClassification, filter_unknown, non_empty, optional_string, orphan_verdict,
    parse_optional_compact_target, parse_optional_hash_field, parse_optional_hex_field,
    parse_optional_nonnegative_i32, parse_optional_nonnegative_i64, parse_parent_header,
    parse_positive_i64, publication_category, validate_child_bundle, validate_parent_fields,
};

pub(super) struct ChildFields {
    pub(super) height: Option<i32>,
    pub(super) block_hash: Option<Vec<u8>>,
    pub(super) header_bytes: Option<Vec<u8>>,
    pub(super) block_time: Option<i64>,
    pub(super) nbits: Option<u32>,
}

struct TaxonomyFields {
    source_classification: SourceClassification,
    relevance_selection: Option<RelevanceSelection>,
    provenance: HistoricalEventProvenance,
    error_rejection_reason: Option<String>,
}

type TaxonomyParser = fn(
    &HistoricalChainSpec,
    &CsvLayout,
    &csv::StringRecord,
    &str,
    Option<&NbitsTable>,
) -> Result<TaxonomyFields, SkipReason>;

pub(crate) fn candidate_from_record(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
    publication_ref: &str,
    nbits_table: Option<&NbitsTable>,
) -> Result<ImportCandidate, SkipReason> {
    candidate_from_record_with_taxonomy(
        spec,
        layout,
        record,
        publication_ref,
        nbits_table,
        parse_taxonomy_fields,
    )
}

/// Decode a row from the error-observation aggregate without widening the
/// valid-evidence parser used by ordinary per-chain artifacts.
pub(crate) fn error_observation_candidate_from_record(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
    publication_ref: &str,
    nbits_table: &NbitsTable,
) -> Result<ImportCandidate, SkipReason> {
    candidate_from_record_with_taxonomy(
        spec,
        layout,
        record,
        publication_ref,
        Some(nbits_table),
        parse_error_observation_taxonomy_fields,
    )
}

fn candidate_from_record_with_taxonomy(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
    publication_ref: &str,
    nbits_table: Option<&NbitsTable>,
    parse_taxonomy: TaxonomyParser,
) -> Result<ImportCandidate, SkipReason> {
    if record.get(layout.chain).map(str::trim) != Some(spec.chain) {
        return Err(SkipReason::Malformed);
    }
    let child = parse_child_fields(spec, layout, record)?;
    let taxonomy = parse_taxonomy(spec, layout, record, publication_ref, nbits_table)?;
    let header = parse_parent_header(record.get(layout.btc_header))?;
    let display_hash = header.block_hash().to_string();
    validate_parent_fields(
        layout,
        record,
        &header,
        &display_hash,
        taxonomy.error_rejection_reason.as_deref()
            != Some(mmm_capture::error_blocks::NBITS_RETARGET_NOT_APPLIED),
    )?;
    let pow_validates_child_target = child
        .nbits
        .map(|nbits| validates_target(header.block_hash(), CompactTarget::from_consensus(nbits)));

    let coinbase = parse_parent_coinbase_fields(layout, record)?;
    let orphan_verdict = if taxonomy.source_classification == SourceClassification::Unknown
        && !matches!(
            taxonomy.relevance_selection,
            Some(RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant)
        ) {
        let verdict = orphan_verdict(
            nbits_table.ok_or(SkipReason::OrphanPending)?,
            spec.chain,
            &header,
            coinbase.script.as_deref(),
        );
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
            pow_validates_child_target,
            btc_parent_coinbase_txid: coinbase.txid,
            btc_parent_coinbase_script: coinbase.script,
            btc_parent_coinbase_outputs: coinbase.outputs,
            btc_parent_coinbase_outputs_text: coinbase.outputs_text,
            btc_parent_coinbase_tx_bytes: coinbase.tx_bytes,
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
        parent_output_addresses: coinbase.output_addresses,
        error_rejection_reason: taxonomy.error_rejection_reason,
    })
}

pub(super) fn parse_child_fields(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
) -> Result<ChildFields, SkipReason> {
    let height = parse_optional_nonnegative_i32(record.get(layout.child_height))?;
    let header_bytes = parse_optional_hex_field(record.get(layout.child_header))?;
    let mut block_hash = parse_optional_hash_field(record.get(layout.child_hash))?;
    if height.is_none() && block_hash.is_none() && header_bytes.is_none() {
        return Err(SkipReason::EmptyField);
    }
    let block_time = parse_optional_nonnegative_i64(record.get(layout.child_time))?;
    let nbits = parse_optional_compact_target(record.get(layout.child_nbits))?;
    validate_child_bundle(
        spec.chain,
        block_hash.as_deref(),
        header_bytes.as_deref(),
        block_time,
        nbits,
    )?;
    if block_hash.is_none()
        && let Some(header) = header_bytes.as_deref()
    {
        block_hash = Some(sha256d::Hash::hash(header).to_byte_array().to_vec());
    }
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
    publication_ref: &str,
    _nbits_table: Option<&NbitsTable>,
) -> Result<TaxonomyFields, SkipReason> {
    let source_classification =
        super::parse_source_classification(record.get(layout.classification))?;
    let validation_status = optional_string(record.get(layout.validation_status));
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
    if !validation_status_matches(category, validation_status.as_deref()) {
        return Err(SkipReason::TaxonomyMismatch);
    }
    let artifact_scope = non_empty(record.get(layout.artifact_scope))?;
    if artifact_scope == "error-block-observations" {
        return Err(SkipReason::TaxonomyMismatch);
    }
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
            publication_ref: publication_ref.to_owned(),
            chain: spec.chain.to_owned(),
            source_kind: non_empty(record.get(layout.source_kind))?.to_owned(),
            source_path: non_empty(record.get(layout.source_path))?.to_owned(),
            source_row_number: parse_positive_i64(record.get(layout.source_row_number))?,
            artifact_scope: artifact_scope.to_owned(),
            provenance: non_empty(record.get(layout.provenance))?.to_owned(),
            classification: non_empty(record.get(layout.classification))?.to_owned(),
            btc_height: parse_optional_nonnegative_i32(record.get(layout.btc_height))?,
            validation_status,
            btc_stale_relevance: relevance,
            relevance_reason,
        },
        error_rejection_reason: None,
    })
}

fn parse_error_observation_taxonomy_fields(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
    publication_ref: &str,
    nbits_table: Option<&NbitsTable>,
) -> Result<TaxonomyFields, SkipReason> {
    if record.get(layout.classification).map(str::trim) != Some("error_block")
        || record.get(layout.artifact_scope).map(str::trim) != Some("error-block-observations")
        || record.get(layout.validation_status).map(str::trim) != Some("VALID_ERROR_BLOCK")
        || optional_string(record.get(layout.relevance)).is_some()
        || optional_string(record.get(layout.relevance_reason)).is_some()
    {
        return Err(SkipReason::TaxonomyMismatch);
    }
    let rejection_reason = non_empty(record.get(layout.rejection_reason))?.to_owned();
    let btc_height = parse_optional_nonnegative_i32(record.get(layout.btc_height))?
        .ok_or(SkipReason::EmptyField)?;
    let expected_nbits = parse_optional_compact_target(record.get(layout.expected_nbits))?
        .ok_or(SkipReason::EmptyField)?;
    if rejection_reason == mmm_capture::error_blocks::NBITS_RETARGET_NOT_APPLIED
        && nbits_table
            .ok_or(SkipReason::Unclassified)?
            .expected_nbits(btc_height)
            != NbitsLookup::Found(expected_nbits)
    {
        return Err(SkipReason::EvidenceMismatch);
    }
    Ok(TaxonomyFields {
        source_classification: SourceClassification::ErrorBlock,
        relevance_selection: None,
        provenance: HistoricalEventProvenance {
            publication_ref: publication_ref.to_owned(),
            chain: spec.chain.to_owned(),
            source_kind: non_empty(record.get(layout.source_kind))?.to_owned(),
            source_path: non_empty(record.get(layout.source_path))?.to_owned(),
            source_row_number: parse_positive_i64(record.get(layout.source_row_number))?,
            artifact_scope: "error-block-observations".to_owned(),
            provenance: non_empty(record.get(layout.provenance))?.to_owned(),
            classification: "error_block".to_owned(),
            btc_height: Some(btc_height),
            validation_status: Some("VALID_ERROR_BLOCK".to_owned()),
            btc_stale_relevance: None,
            relevance_reason: None,
        },
        error_rejection_reason: Some(rejection_reason),
    })
}

fn validation_status_matches(category: PublicationCategory, status: Option<&str>) -> bool {
    match category {
        PublicationCategory::Stale => status.is_some_and(has_direct_stale_valid_token),
        PublicationCategory::StaleDescendant => status == Some("VALID_STALE_DESCENDANT"),
        PublicationCategory::Canonical
        | PublicationCategory::StrictBtcOrphan
        | PublicationCategory::WeakBtcOrphan => true,
    }
}

fn has_direct_stale_valid_token(value: &str) -> bool {
    value.strip_prefix("VALID").is_some_and(|suffix| {
        suffix
            .chars()
            .next()
            .is_none_or(|character| character.is_whitespace() || character == '(')
    })
}
