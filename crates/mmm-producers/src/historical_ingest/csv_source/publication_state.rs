//! Compact, classifier-free publication rows used by `import-all` planning.

use anyhow::{Context, Result};
use bitcoin::CompactTarget;
use bitcoin::hashes::{Hash as _, HashEngine as _, sha256};
use mmm_capture::auxpow::validates_target;

use super::candidate::{parse_child_fields, require_importable_child_identity};
use super::parent_coinbase::parse_parent_coinbase_fields;
use super::{
    CsvLayout, SkipReason, non_empty, optional_string, parse_optional_compact_target,
    parse_optional_nonnegative_i32, parse_parent_header, parse_positive_i64,
    validate_parent_fields,
};
use crate::historical_ingest::config::HistoricalChainSpec;
use crate::historical_ingest::rsk_sidecar::parse_rsk_sidecar;

const CHILD_HEIGHT: u16 = 1 << 0;
const CHILD_HASH: u16 = 1 << 1;
const CHILD_HEADER: u16 = 1 << 2;
const CHILD_TIME: u16 = 1 << 3;
const CHILD_NBITS: u16 = 1 << 4;
const CHILD_TARGET_RESULT: u16 = 1 << 5;
const COINBASE_TXID: u16 = 1 << 6;
const COINBASE_SCRIPT: u16 = 1 << 7;
const COINBASE_OUTPUTS: u16 = 1 << 8;
const COINBASE_OUTPUTS_TEXT: u16 = 1 << 9;
const COINBASE_TX: u16 = 1 << 10;
const RSK_MERKLE_PROOF: u16 = 1 << 11;
const RSK_COINBASE_TAIL: u16 = 1 << 12;
const ERROR_BLOCK_REASON: u16 = 1 << 13;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct PublicationRowKey {
    pub(crate) chain: String,
    pub(crate) source_path: String,
    pub(crate) source_row_number: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PublicationFieldMask(u16);

impl PublicationFieldMask {
    const fn includes(self, field: u16) -> bool {
        self.0 & field != 0
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ExpectedPublicationState {
    pub(crate) key: PublicationRowKey,
    mask: PublicationFieldMask,
    fingerprint: [u8; 32],
    pub(crate) child_height: Option<i32>,
    pub(crate) child_block_hash: Option<[u8; 32]>,
    pub(crate) btc_parent_header_hash: [u8; 32],
}

impl ExpectedPublicationState {
    pub(crate) fn matches(&self, stored: &ComparablePublicationState) -> bool {
        self.fingerprint == stored.fingerprint(self.mask)
    }
}

#[derive(Debug, Clone)]
pub(super) struct ComparableRskState {
    pub(super) block_hash: [u8; 32],
    pub(super) height: i32,
    pub(super) is_uncle: bool,
    pub(super) uncle_index: Option<i32>,
    pub(super) uncle_parent_height: Option<i32>,
    pub(super) miner: Vec<u8>,
    pub(super) merge_mining_hash: [u8; 32],
    pub(super) merkle_proof_sha256: Option<[u8; 32]>,
    pub(super) coinbase_tail_sha256: Option<[u8; 32]>,
    pub(super) proof_format: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ComparablePublicationState {
    pub(super) source_kind: String,
    pub(super) artifact_scope: String,
    pub(super) provenance: String,
    pub(super) classification: String,
    pub(super) btc_height: Option<i32>,
    pub(super) validation_status: Option<String>,
    pub(super) btc_stale_relevance: Option<String>,
    pub(super) relevance_reason: Option<String>,
    pub(super) child_height: Option<i32>,
    pub(super) child_block_hash: Option<[u8; 32]>,
    pub(super) child_header_sha256: Option<[u8; 32]>,
    pub(super) child_block_time: Option<i64>,
    pub(super) child_nbits: Option<i64>,
    pub(super) pow_validates_child_target: Option<bool>,
    pub(super) btc_parent_header_hash: [u8; 32],
    pub(super) btc_parent_prev_header_hash: [u8; 32],
    pub(super) btc_parent_header_time: i64,
    pub(super) btc_parent_coinbase_txid: Option<[u8; 32]>,
    pub(super) btc_parent_coinbase_script_sha256: Option<[u8; 32]>,
    pub(super) btc_parent_coinbase_outputs_sha256: Option<[u8; 32]>,
    pub(super) btc_parent_coinbase_outputs_text_sha256: Option<[u8; 32]>,
    pub(super) btc_parent_coinbase_tx_sha256: Option<[u8; 32]>,
    pub(super) elastos_reactivation_required: bool,
    pub(super) rsk: Option<ComparableRskState>,
    pub(super) error_block_reason: Option<String>,
}

impl ComparablePublicationState {
    pub(super) fn fingerprint(&self, mask: PublicationFieldMask) -> [u8; 32] {
        let mut engine = sha256::Hash::engine();
        push_bytes(&mut engine, self.source_kind.as_bytes());
        push_bytes(&mut engine, self.artifact_scope.as_bytes());
        push_bytes(&mut engine, self.provenance.as_bytes());
        push_bytes(&mut engine, self.classification.as_bytes());
        push_option_i32(&mut engine, self.btc_height);
        push_option_bytes(
            &mut engine,
            self.validation_status.as_deref().map(str::as_bytes),
        );
        push_option_bytes(
            &mut engine,
            self.btc_stale_relevance.as_deref().map(str::as_bytes),
        );
        push_option_bytes(
            &mut engine,
            self.relevance_reason.as_deref().map(str::as_bytes),
        );
        push_bytes(&mut engine, &self.btc_parent_header_hash);
        push_bytes(&mut engine, &self.btc_parent_prev_header_hash);
        push_i64(&mut engine, self.btc_parent_header_time);
        push_bool(&mut engine, self.elastos_reactivation_required);
        if mask.includes(ERROR_BLOCK_REASON) {
            push_option_bytes(
                &mut engine,
                self.error_block_reason.as_deref().map(str::as_bytes),
            );
        }

        if mask.includes(CHILD_HEIGHT) {
            push_option_i32(&mut engine, self.child_height);
        }
        if mask.includes(CHILD_HASH) {
            push_option_array(&mut engine, self.child_block_hash.as_ref());
        }
        if mask.includes(CHILD_HEADER) {
            push_option_array(&mut engine, self.child_header_sha256.as_ref());
        }
        if mask.includes(CHILD_TIME) {
            push_option_i64(&mut engine, self.child_block_time);
        }
        if mask.includes(CHILD_NBITS) {
            push_option_i64(&mut engine, self.child_nbits);
        }
        if mask.includes(CHILD_TARGET_RESULT) {
            push_option_bool(&mut engine, self.pow_validates_child_target);
        }
        if mask.includes(COINBASE_TXID) {
            push_option_array(&mut engine, self.btc_parent_coinbase_txid.as_ref());
        }
        if mask.includes(COINBASE_SCRIPT) {
            push_option_array(&mut engine, self.btc_parent_coinbase_script_sha256.as_ref());
        }
        if mask.includes(COINBASE_OUTPUTS) {
            push_option_array(
                &mut engine,
                self.btc_parent_coinbase_outputs_sha256.as_ref(),
            );
        }
        if mask.includes(COINBASE_OUTPUTS_TEXT) {
            push_option_array(
                &mut engine,
                self.btc_parent_coinbase_outputs_text_sha256.as_ref(),
            );
        }
        if mask.includes(COINBASE_TX) {
            push_option_array(&mut engine, self.btc_parent_coinbase_tx_sha256.as_ref());
        }

        match &self.rsk {
            Some(rsk) => {
                push_bool(&mut engine, true);
                push_bytes(&mut engine, &rsk.block_hash);
                push_i32(&mut engine, rsk.height);
                push_bool(&mut engine, rsk.is_uncle);
                push_option_i32(&mut engine, rsk.uncle_index);
                push_option_i32(&mut engine, rsk.uncle_parent_height);
                push_bytes(&mut engine, &rsk.miner);
                push_bytes(&mut engine, &rsk.merge_mining_hash);
                push_bytes(&mut engine, rsk.proof_format.as_bytes());
                if mask.includes(RSK_MERKLE_PROOF) {
                    push_option_array(&mut engine, rsk.merkle_proof_sha256.as_ref());
                }
                if mask.includes(RSK_COINBASE_TAIL) {
                    push_option_array(&mut engine, rsk.coinbase_tail_sha256.as_ref());
                }
            }
            None => push_bool(&mut engine, false),
        }

        sha256::Hash::from_engine(engine).to_byte_array()
    }

    pub(crate) fn from_stored(row: mmm_store::HistoricalPublicationStateRow) -> Result<Self> {
        let rsk = match row.rsk_block_hash {
            Some(block_hash) => Some(ComparableRskState {
                block_hash: required_array32(block_hash, "rsk_block_hash")?,
                height: row.rsk_height.context("stored RSK row has no height")?,
                is_uncle: row
                    .rsk_is_uncle
                    .context("stored RSK row has no uncle flag")?,
                uncle_index: row.rsk_uncle_index,
                uncle_parent_height: row.rsk_uncle_parent_height,
                miner: row.rsk_miner.context("stored RSK row has no miner")?,
                merge_mining_hash: required_array32(
                    row.rsk_merge_mining_hash
                        .context("stored RSK row has no merge-mining hash")?,
                    "rsk_merge_mining_hash",
                )?,
                merkle_proof_sha256: stored_option_array32(
                    row.rsk_merkle_proof_sha256,
                    "rsk_merkle_proof_sha256",
                )?,
                coinbase_tail_sha256: stored_option_array32(
                    row.rsk_coinbase_tail_sha256,
                    "rsk_coinbase_tail_sha256",
                )?,
                proof_format: row
                    .rsk_proof_format
                    .context("stored RSK row has no proof format")?,
            }),
            None => {
                anyhow::ensure!(
                    row.rsk_height.is_none()
                        && row.rsk_is_uncle.is_none()
                        && row.rsk_uncle_index.is_none()
                        && row.rsk_uncle_parent_height.is_none()
                        && row.rsk_miner.is_none()
                        && row.rsk_merge_mining_hash.is_none()
                        && row.rsk_merkle_proof_sha256.is_none()
                        && row.rsk_coinbase_tail_sha256.is_none()
                        && row.rsk_proof_format.is_none(),
                    "stored publication row has a partial RSK sidecar"
                );
                None
            }
        };
        Ok(Self {
            source_kind: row.source_kind,
            artifact_scope: row.artifact_scope,
            provenance: row.provenance,
            classification: row.classification,
            btc_height: row.btc_height,
            validation_status: row.validation_status,
            btc_stale_relevance: row.btc_stale_relevance,
            relevance_reason: row.relevance_reason,
            child_height: row.child_height,
            child_block_hash: stored_option_array32(row.child_block_hash, "child_block_hash")?,
            child_header_sha256: stored_option_array32(
                row.child_header_sha256,
                "child_header_sha256",
            )?,
            child_block_time: row.child_block_time,
            child_nbits: row.child_nbits,
            pow_validates_child_target: row.pow_validates_child_target,
            btc_parent_header_hash: required_array32(
                row.btc_parent_header_hash,
                "btc_parent_header_hash",
            )?,
            btc_parent_prev_header_hash: required_array32(
                row.btc_parent_prev_header_hash,
                "btc_parent_prev_header_hash",
            )?,
            btc_parent_header_time: row.btc_parent_header_time,
            btc_parent_coinbase_txid: stored_option_array32(
                row.btc_parent_coinbase_txid,
                "btc_parent_coinbase_txid",
            )?,
            btc_parent_coinbase_script_sha256: stored_option_array32(
                row.btc_parent_coinbase_script_sha256,
                "btc_parent_coinbase_script_sha256",
            )?,
            btc_parent_coinbase_outputs_sha256: stored_option_array32(
                row.btc_parent_coinbase_outputs_sha256,
                "btc_parent_coinbase_outputs_sha256",
            )?,
            btc_parent_coinbase_outputs_text_sha256: stored_option_array32(
                row.btc_parent_coinbase_outputs_text_sha256,
                "btc_parent_coinbase_outputs_text_sha256",
            )?,
            btc_parent_coinbase_tx_sha256: stored_option_array32(
                row.btc_parent_coinbase_tx_sha256,
                "btc_parent_coinbase_tx_sha256",
            )?,
            elastos_reactivation_required: row.chain == "elastos"
                && row.revoked_at.is_some()
                && row.revocation_reason.as_deref()
                    == Some(mmm_capture::capture::ELASTOS_REVOKE_NON_BTC),
            rsk,
            error_block_reason: row.error_block_reason,
        })
    }
}

pub(crate) fn publication_state_from_record(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
    error_observation: bool,
) -> Result<ExpectedPublicationState, SkipReason> {
    if record.get(layout.chain).map(str::trim) != Some(spec.chain) {
        return Err(SkipReason::Malformed);
    }
    let state = comparable_state_from_record(spec, layout, record, error_observation)?;
    let mask = publication_field_mask(&state);
    let fingerprint = state.fingerprint(mask);
    Ok(ExpectedPublicationState {
        key: publication_row_key_from_record(spec, layout, record)?,
        mask,
        fingerprint,
        child_height: state.child_height,
        child_block_hash: state.child_block_hash,
        btc_parent_header_hash: state.btc_parent_header_hash,
    })
}

pub(crate) fn publication_row_key_from_record(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
) -> Result<PublicationRowKey, SkipReason> {
    Ok(PublicationRowKey {
        chain: spec.chain.to_owned(),
        source_path: non_empty(record.get(layout.source_path))?.to_owned(),
        source_row_number: parse_positive_i64(record.get(layout.source_row_number))?,
    })
}

fn comparable_state_from_record(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
    error_observation: bool,
) -> Result<ComparablePublicationState, SkipReason> {
    let child = parse_child_fields(spec, layout, record)?;
    let source_classification = if error_observation {
        super::SourceClassification::ErrorBlock
    } else {
        super::parse_source_classification(record.get(layout.classification))?
    };
    let header = parse_parent_header(record.get(layout.btc_header))?;
    let display_hash = header.block_hash().to_string();
    let rejection_reason = optional_string(record.get(layout.rejection_reason));
    validate_parent_fields(
        layout,
        record,
        &header,
        &display_hash,
        rejection_reason.as_deref() != Some(mmm_capture::error_blocks::NBITS_RETARGET_NOT_APPLIED),
    )?;
    let parent_coinbase = parse_parent_coinbase_fields(layout, record)?;
    let artifact_scope = non_empty(record.get(layout.artifact_scope))?.to_owned();
    if error_observation != (artifact_scope == "error-block-observations") {
        return Err(SkipReason::TaxonomyMismatch);
    }
    validate_error_observation_fields(
        layout,
        record,
        error_observation,
        rejection_reason.as_deref(),
    )?;
    let child_hash = option_array32(child.block_hash.as_deref())?;
    let parent_hash = header.block_hash().to_byte_array();
    require_importable_child_identity(&child, source_classification)?;
    let rsk = comparable_rsk_state(layout, record, &child)?;
    let pow_validates_child_target = child
        .nbits
        .map(|nbits| validates_target(header.block_hash(), CompactTarget::from_consensus(nbits)));
    let state = ComparablePublicationState {
        source_kind: non_empty(record.get(layout.source_kind))?.to_owned(),
        artifact_scope: artifact_scope.clone(),
        provenance: non_empty(record.get(layout.provenance))?.to_owned(),
        classification: non_empty(record.get(layout.classification))?.to_owned(),
        btc_height: parse_optional_nonnegative_i32(record.get(layout.btc_height))?,
        validation_status: optional_string(record.get(layout.validation_status)),
        btc_stale_relevance: optional_string(record.get(layout.relevance)),
        relevance_reason: optional_string(record.get(layout.relevance_reason)),
        child_height: child.height,
        child_block_hash: child_hash,
        child_header_sha256: digest_optional(child.header_bytes.as_deref()),
        child_block_time: child.block_time,
        child_nbits: child.nbits.map(i64::from),
        pow_validates_child_target,
        btc_parent_header_hash: parent_hash,
        btc_parent_prev_header_hash: header.prev_blockhash.to_byte_array(),
        btc_parent_header_time: i64::from(header.time),
        btc_parent_coinbase_txid: option_array32(parent_coinbase.txid.as_deref())?,
        btc_parent_coinbase_script_sha256: digest_optional(parent_coinbase.script.as_deref()),
        btc_parent_coinbase_outputs_sha256: digest_optional(parent_coinbase.outputs.as_deref()),
        btc_parent_coinbase_outputs_text_sha256: parent_coinbase
            .outputs_text
            .as_deref()
            .map(|value| digest(value.as_bytes())),
        btc_parent_coinbase_tx_sha256: digest_optional(parent_coinbase.tx_bytes.as_deref()),
        elastos_reactivation_required: false,
        rsk,
        error_block_reason: if error_observation {
            rejection_reason
        } else {
            None
        },
    };
    Ok(state)
}

fn comparable_rsk_state(
    layout: &CsvLayout,
    record: &csv::StringRecord,
    child: &super::candidate::ChildFields,
) -> Result<Option<ComparableRskState>, SkipReason> {
    layout
        .rsk_sidecar
        .as_ref()
        .map(|columns| {
            let evidence = parse_rsk_sidecar(
                columns,
                record,
                child.height.ok_or(SkipReason::EmptyField)?,
                child.block_hash.as_deref().ok_or(SkipReason::EmptyField)?,
            )?;
            Ok(ComparableRskState {
                block_hash: array32(&evidence.rsk_block_hash)?,
                height: evidence.rsk_height,
                is_uncle: evidence.is_uncle,
                uncle_index: evidence.uncle_index,
                uncle_parent_height: evidence.uncle_parent_height,
                miner: evidence.rsk_miner,
                merge_mining_hash: array32(&evidence.merge_mining_hash)?,
                merkle_proof_sha256: digest_optional(evidence.merkle_proof.as_deref()),
                coinbase_tail_sha256: digest_optional(evidence.coinbase_tail.as_deref()),
                proof_format: evidence.proof_format.to_owned(),
            })
        })
        .transpose()
}

fn validate_error_observation_fields(
    layout: &CsvLayout,
    record: &csv::StringRecord,
    error_observation: bool,
    rejection_reason: Option<&str>,
) -> Result<(), SkipReason> {
    if !error_observation {
        return Ok(());
    }
    if record.get(layout.classification).map(str::trim) != Some("error_block")
        || record.get(layout.validation_status).map(str::trim) != Some("VALID_ERROR_BLOCK")
        || optional_string(record.get(layout.relevance)).is_some()
        || optional_string(record.get(layout.relevance_reason)).is_some()
        || parse_optional_nonnegative_i32(record.get(layout.btc_height))?.is_none()
        || parse_optional_compact_target(record.get(layout.expected_nbits))?.is_none()
        || rejection_reason.is_none()
    {
        return Err(SkipReason::TaxonomyMismatch);
    }
    Ok(())
}

fn publication_field_mask(state: &ComparablePublicationState) -> PublicationFieldMask {
    let mut mask = 0_u16;
    set_if(&mut mask, CHILD_HEIGHT, state.child_height.is_some());
    set_if(&mut mask, CHILD_HASH, state.child_block_hash.is_some());
    set_if(&mut mask, CHILD_HEADER, state.child_header_sha256.is_some());
    set_if(&mut mask, CHILD_TIME, state.child_block_time.is_some());
    set_if(&mut mask, CHILD_NBITS, state.child_nbits.is_some());
    set_if(
        &mut mask,
        CHILD_TARGET_RESULT,
        state.pow_validates_child_target.is_some(),
    );
    set_if(
        &mut mask,
        COINBASE_TXID,
        state.btc_parent_coinbase_txid.is_some(),
    );
    set_if(
        &mut mask,
        COINBASE_SCRIPT,
        state.btc_parent_coinbase_script_sha256.is_some(),
    );
    set_if(
        &mut mask,
        COINBASE_OUTPUTS,
        state.btc_parent_coinbase_outputs_sha256.is_some(),
    );
    set_if(
        &mut mask,
        COINBASE_OUTPUTS_TEXT,
        state.btc_parent_coinbase_outputs_text_sha256.is_some(),
    );
    set_if(
        &mut mask,
        COINBASE_TX,
        state.btc_parent_coinbase_tx_sha256.is_some(),
    );
    if let Some(rsk) = &state.rsk {
        set_if(
            &mut mask,
            RSK_MERKLE_PROOF,
            rsk.merkle_proof_sha256.is_some(),
        );
        set_if(
            &mut mask,
            RSK_COINBASE_TAIL,
            rsk.coinbase_tail_sha256.is_some(),
        );
    }
    set_if(
        &mut mask,
        ERROR_BLOCK_REASON,
        state.error_block_reason.is_some(),
    );
    PublicationFieldMask(mask)
}

pub(super) fn array32(value: &[u8]) -> Result<[u8; 32], SkipReason> {
    value.try_into().map_err(|_| SkipReason::Malformed)
}

pub(super) fn option_array32(value: Option<&[u8]>) -> Result<Option<[u8; 32]>, SkipReason> {
    value.map(array32).transpose()
}

fn set_if(mask: &mut u16, bit: u16, present: bool) {
    if present {
        *mask |= bit;
    }
}

fn digest_optional(value: Option<&[u8]>) -> Option<[u8; 32]> {
    value.map(digest)
}

fn digest(value: &[u8]) -> [u8; 32] {
    sha256::Hash::hash(value).to_byte_array()
}

fn push_bytes(engine: &mut sha256::HashEngine, value: &[u8]) {
    let length = u64::try_from(value.len()).expect("publication field length fits u64");
    engine.input(&length.to_be_bytes());
    engine.input(value);
}

fn push_option_bytes(engine: &mut sha256::HashEngine, value: Option<&[u8]>) {
    match value {
        Some(value) => {
            push_bool(engine, true);
            push_bytes(engine, value);
        }
        None => push_bool(engine, false),
    }
}

fn push_option_array(engine: &mut sha256::HashEngine, value: Option<&[u8; 32]>) {
    push_option_bytes(engine, value.map(|array| array.as_slice()));
}

fn required_array32(value: Vec<u8>, field: &str) -> Result<[u8; 32]> {
    value
        .try_into()
        .map_err(|_| anyhow::anyhow!("stored {field} is not 32 bytes"))
}

fn stored_option_array32(value: Option<Vec<u8>>, field: &str) -> Result<Option<[u8; 32]>> {
    value
        .map(|bytes| required_array32(bytes, field))
        .transpose()
}

fn push_i32(engine: &mut sha256::HashEngine, value: i32) {
    engine.input(&value.to_be_bytes());
}

fn push_i64(engine: &mut sha256::HashEngine, value: i64) {
    engine.input(&value.to_be_bytes());
}

fn push_bool(engine: &mut sha256::HashEngine, value: bool) {
    engine.input(&[u8::from(value)]);
}

fn push_option_i32(engine: &mut sha256::HashEngine, value: Option<i32>) {
    push_bool(engine, value.is_some());
    if let Some(value) = value {
        push_i32(engine, value);
    }
}

fn push_option_i64(engine: &mut sha256::HashEngine, value: Option<i64>) {
    push_bool(engine, value.is_some());
    if let Some(value) = value {
        push_i64(engine, value);
    }
}

fn push_option_bool(engine: &mut sha256::HashEngine, value: Option<bool>) {
    push_bool(engine, value.is_some());
    if let Some(value) = value {
        push_bool(engine, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn comparable() -> ComparablePublicationState {
        ComparablePublicationState {
            source_kind: "monitor-evidence".to_owned(),
            artifact_scope: "event".to_owned(),
            provenance: "fixture".to_owned(),
            classification: "canonical".to_owned(),
            btc_height: Some(700_000),
            validation_status: Some("VALID".to_owned()),
            btc_stale_relevance: None,
            relevance_reason: Some("canonical_parent".to_owned()),
            child_height: Some(12),
            child_block_hash: None,
            child_header_sha256: None,
            child_block_time: None,
            child_nbits: None,
            pow_validates_child_target: None,
            btc_parent_header_hash: [1; 32],
            btc_parent_prev_header_hash: [2; 32],
            btc_parent_header_time: 1_700_000_000,
            btc_parent_coinbase_txid: None,
            btc_parent_coinbase_script_sha256: None,
            btc_parent_coinbase_outputs_sha256: None,
            btc_parent_coinbase_outputs_text_sha256: None,
            btc_parent_coinbase_tx_sha256: None,
            elastos_reactivation_required: false,
            rsk: None,
            error_block_reason: None,
        }
    }

    #[test]
    fn unpublished_database_enrichment_does_not_change_the_fingerprint() {
        let expected = comparable();
        let mut enriched = expected.clone();
        enriched.child_block_hash = Some([3; 32]);
        enriched.child_header_sha256 = Some([4; 32]);
        enriched.child_block_time = Some(1_700_000_001);
        enriched.btc_parent_coinbase_txid = Some([5; 32]);
        enriched.btc_parent_coinbase_script_sha256 = Some([6; 32]);
        let mask = PublicationFieldMask(CHILD_HEIGHT);
        assert_eq!(expected.fingerprint(mask), enriched.fingerprint(mask));

        enriched.child_height = Some(13);
        assert_ne!(expected.fingerprint(mask), enriched.fingerprint(mask));
    }

    #[test]
    fn rsk_identity_fields_match_while_optional_proof_enrichment_is_masked() {
        let mut expected = comparable();
        expected.rsk = Some(ComparableRskState {
            block_hash: [7; 32],
            height: 42,
            is_uncle: false,
            uncle_index: None,
            uncle_parent_height: None,
            miner: vec![8; 20],
            merge_mining_hash: [9; 32],
            merkle_proof_sha256: None,
            coinbase_tail_sha256: None,
            proof_format: "rskj_rpc_opaque".to_owned(),
        });
        let mut enriched = expected.clone();
        enriched
            .rsk
            .as_mut()
            .expect("RSK state")
            .merkle_proof_sha256 = Some([10; 32]);
        assert_eq!(
            expected.fingerprint(PublicationFieldMask(0)),
            enriched.fingerprint(PublicationFieldMask(0))
        );
        assert_ne!(
            expected.fingerprint(PublicationFieldMask(RSK_MERKLE_PROOF)),
            enriched.fingerprint(PublicationFieldMask(RSK_MERKLE_PROOF))
        );
    }

    #[test]
    fn error_reason_and_elastos_reactivation_are_comparison_state() {
        let mut expected = comparable();
        expected.error_block_reason = Some("time_below_mtp".to_owned());
        let mut changed = expected.clone();
        changed.error_block_reason = Some("different".to_owned());
        assert_ne!(
            expected.fingerprint(PublicationFieldMask(ERROR_BLOCK_REASON)),
            changed.fingerprint(PublicationFieldMask(ERROR_BLOCK_REASON))
        );

        changed = expected.clone();
        changed.elastos_reactivation_required = true;
        assert_ne!(
            expected.fingerprint(PublicationFieldMask(0)),
            changed.fingerprint(PublicationFieldMask(0))
        );
    }
}
