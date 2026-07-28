//! RSK sidecar-column parsing for the recovered-evidence importer.
//!
//! The RSK monitor-evidence export appends the seven columns
//! `rsk_merge_mining_evidence` requires beyond the shared event row. This
//! module resolves and parses them into the same [`RskEvidencePayload`] the
//! live RSK poller writes, so the import path and live capture share one
//! sidecar shape. mmm-api hard-errors on any `auxpow:rsk` event without a
//! sidecar row, which is why every column here is required on the RSK
//! dataset.

use anyhow::Result;
use mmm_capture::capture::{RSK_PROOF_FORMAT_OPAQUE, RskEvidencePayload};

use super::csv_source::{
    SkipReason, non_empty, parse_hex_field, parse_optional_hex_field, required_header,
};

/// Column indices for the RSK export's `rsk_merge_mining_evidence` fields.
/// All seven are required headers on the RSK dataset -- a file without them
/// predates the sidecar-bearing export and cannot construct the sidecar rows
/// mmm-api requires for every `auxpow:rsk` event.
pub(super) struct RskSidecarColumns {
    miner: usize,
    merge_mining_hash: usize,
    is_uncle: usize,
    uncle_index: usize,
    uncle_parent_height: usize,
    merkle_proof: usize,
    coinbase_tail: usize,
}

impl RskSidecarColumns {
    pub(super) fn new(headers: &csv::StringRecord) -> Result<Self> {
        Ok(Self {
            miner: required_header(headers, "rsk_miner")?,
            merge_mining_hash: required_header(headers, "merge_mining_hash")?,
            is_uncle: required_header(headers, "is_uncle")?,
            uncle_index: required_header(headers, "uncle_index")?,
            uncle_parent_height: required_header(headers, "uncle_parent_height")?,
            merkle_proof: required_header(headers, "rsk_merkle_proof")?,
            coinbase_tail: required_header(headers, "rsk_coinbase_tail")?,
        })
    }
}

/// Parse one RSK row's sidecar columns into the `rsk_merge_mining_evidence`
/// payload the store writes 1:1 with the event.
///
/// `rsk_block_hash` and `rsk_height` reuse the event's child identity (the
/// export's `child_block_hash` is the forward-order RSK block hash, matching
/// live storage). For uncle rows the export's `child_height` is the uncle's
/// OWN RSK block number, exactly the convention the live path's
/// `prepare_rsk_capture` stores (`block.number` of the uncle response, with
/// `uncle_parent_height` carrying the including canonical height) -- the
/// research recovery cross-checks every fetched block/uncle's `number`
/// against the recorded height and refuses a disagreeing row, so the
/// `(source_id, child_height, child_block_hash)` dedup premise holds for
/// uncles too. The miner must be exactly 20 bytes and the merge-mining
/// hash exactly 32; `is_uncle` is the export's `0`/`1`; uncle placement must
/// be consistent with it (both set when an uncle, both blank otherwise --
/// the sidecar table CHECKs the same invariant). `pool_identity_id` stays
/// NULL for the late-fill path `reclassify-pools` owns.
pub(super) fn parse_rsk_sidecar(
    columns: &RskSidecarColumns,
    record: &csv::StringRecord,
    child_height: i32,
    child_block_hash: &[u8],
) -> Result<RskEvidencePayload, SkipReason> {
    let miner = parse_hex_field(record.get(columns.miner))?;
    if miner.len() != 20 {
        return Err(SkipReason::Malformed);
    }
    let merge_mining_hash = parse_hex_field(record.get(columns.merge_mining_hash))?;
    if merge_mining_hash.len() != 32 {
        return Err(SkipReason::Malformed);
    }
    let is_uncle = match non_empty(record.get(columns.is_uncle))? {
        "0" => false,
        "1" => true,
        _ => return Err(SkipReason::Malformed),
    };
    let uncle_index = parse_optional_i32(record.get(columns.uncle_index))?;
    let uncle_parent_height = parse_optional_i32(record.get(columns.uncle_parent_height))?;
    let placement_consistent = if is_uncle {
        uncle_index.is_some() && uncle_parent_height.is_some()
    } else {
        uncle_index.is_none() && uncle_parent_height.is_none()
    };
    if !placement_consistent {
        return Err(SkipReason::Malformed);
    }
    Ok(RskEvidencePayload {
        rsk_block_hash: child_block_hash.to_vec(),
        rsk_height: child_height,
        is_uncle,
        uncle_index,
        uncle_parent_height,
        rsk_miner: miner,
        pool_identity_id: None,
        merge_mining_hash,
        merkle_proof: parse_optional_hex_field(record.get(columns.merkle_proof))?,
        coinbase_tail: parse_optional_hex_field(record.get(columns.coinbase_tail))?,
        proof_format: RSK_PROOF_FORMAT_OPAQUE,
    })
}

/// Parse an optional non-negative integer cell: blank yields `None`; a
/// non-integer or negative value is malformed (the live path only ever
/// produces non-negative uncle placement, matching `parse_child_height`).
fn parse_optional_i32(value: Option<&str>) -> Result<Option<i32>, SkipReason> {
    let trimmed = value.unwrap_or_default().trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    match trimmed.parse::<i32>() {
        Ok(parsed) if parsed >= 0 => Ok(Some(parsed)),
        _ => Err(SkipReason::Malformed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSK_CHILD_HASH: &str = "863002b6ad9a940f191f3ed3289e42e8eee107a769b6ecdfdaaad747f70c981d";
    const RSK_MINER: &str = "32dfc7a84f24b10a5dded1d8b24f48b96ab77373";
    const RSK_MM_HASH: &str = "f0d9129c65b3b91a89355b9ccf975e55c29229d78d4a66201b83d409ae001f73";

    fn parse(
        miner: &str,
        is_uncle: &str,
        uncle_index: &str,
        uncle_parent_height: &str,
    ) -> Result<RskEvidencePayload, SkipReason> {
        let input = format!(
            "rsk_miner,merge_mining_hash,is_uncle,uncle_index,uncle_parent_height,\
             rsk_merkle_proof,rsk_coinbase_tail\n\
             {miner},{RSK_MM_HASH},{is_uncle},{uncle_index},{uncle_parent_height},0405,\n"
        );
        let mut reader = csv::Reader::from_reader(input.as_bytes());
        let columns = RskSidecarColumns::new(reader.headers().unwrap()).unwrap();
        let record = reader.records().next().unwrap().unwrap();
        parse_rsk_sidecar(
            &columns,
            &record,
            263_443,
            &hex::decode(RSK_CHILD_HASH).unwrap(),
        )
    }

    #[test]
    fn missing_sidecar_columns_reject_the_layout() {
        let input = "rsk_merkle_proof,rsk_coinbase_tail
";
        let mut reader = csv::Reader::from_reader(input.as_bytes());
        let error = RskSidecarColumns::new(reader.headers().unwrap())
            .err()
            .expect("a headerless RSK layout must be rejected");
        assert!(error.to_string().contains("rsk_miner"), "{error}");
    }

    #[test]
    fn parses_block_and_uncle_sidecar_rows() {
        let block = parse(RSK_MINER, "0", "", "").unwrap();
        assert_eq!(block.rsk_block_hash, hex::decode(RSK_CHILD_HASH).unwrap());
        assert_eq!(block.rsk_height, 263_443);
        assert!(!block.is_uncle);
        assert_eq!(block.uncle_index, None);
        assert_eq!(block.uncle_parent_height, None);
        assert_eq!(block.rsk_miner, hex::decode(RSK_MINER).unwrap());
        assert_eq!(block.pool_identity_id, None);
        assert_eq!(block.merge_mining_hash, hex::decode(RSK_MM_HASH).unwrap());
        assert_eq!(block.merkle_proof.as_deref(), Some(&[0x04, 0x05][..]));
        assert_eq!(block.coinbase_tail, None);
        assert_eq!(block.proof_format, RSK_PROOF_FORMAT_OPAQUE);

        let uncle = parse(RSK_MINER, "1", "0", "417924").unwrap();
        assert!(uncle.is_uncle);
        assert_eq!(uncle.uncle_index, Some(0));
        assert_eq!(uncle.uncle_parent_height, Some(417_924));
    }

    #[test]
    fn rejects_inconsistent_uncle_placement_and_bad_fields() {
        // The sidecar table CHECKs the same placement invariant; a violating
        // row must die at parse rather than abort the import mid-transaction.
        assert_eq!(
            parse(RSK_MINER, "1", "", "417924").unwrap_err(),
            SkipReason::Malformed
        );
        assert_eq!(
            parse(RSK_MINER, "1", "0", "").unwrap_err(),
            SkipReason::Malformed
        );
        assert_eq!(
            parse(RSK_MINER, "0", "0", "").unwrap_err(),
            SkipReason::Malformed
        );
        assert_eq!(
            parse(RSK_MINER, "0", "", "417924").unwrap_err(),
            SkipReason::Malformed
        );
        assert_eq!(
            parse(RSK_MINER, "2", "", "").unwrap_err(),
            SkipReason::Malformed
        );
        // Negative placement never occurs on the live path; reject it.
        assert_eq!(
            parse(RSK_MINER, "1", "-1", "417924").unwrap_err(),
            SkipReason::Malformed
        );
        // Miner must be exactly 20 bytes; blank is an empty required field.
        assert_eq!(
            parse("32dfc7", "0", "", "").unwrap_err(),
            SkipReason::Malformed
        );
        assert_eq!(parse("", "0", "", "").unwrap_err(), SkipReason::EmptyField);
    }
}
