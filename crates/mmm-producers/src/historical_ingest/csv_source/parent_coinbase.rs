//! Lossless parsing of the normalized publication's parent-coinbase fields.

use bitcoin::Transaction;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash as _;

use mmm_capture::auxpow::output_addresses;
use mmm_capture::capture::published_parent_coinbase_output_addresses;

use super::{CsvLayout, SkipReason, optional_string, parse_optional_hex_field};

pub(super) struct ParentCoinbaseFields {
    pub(super) txid: Option<Vec<u8>>,
    pub(super) script: Option<Vec<u8>>,
    pub(super) outputs: Option<Vec<u8>>,
    pub(super) outputs_text: Option<String>,
    pub(super) tx_bytes: Option<Vec<u8>>,
    pub(super) output_addresses: Vec<String>,
}

pub(super) fn parse_parent_coinbase_fields(
    layout: &CsvLayout,
    record: &csv::StringRecord,
) -> Result<ParentCoinbaseFields, SkipReason> {
    let published_script = parse_optional_hex_field(record.get(layout.coinbase_script))?;
    let outputs_text = optional_string(record.get(layout.coinbase_outputs));
    let tx_bytes = parse_optional_hex_field(record.get(layout.full_coinbase))?;
    let published_addresses = outputs_text
        .as_deref()
        .map(published_parent_coinbase_output_addresses)
        .unwrap_or_default();

    let Some(tx_bytes) = tx_bytes else {
        return Ok(ParentCoinbaseFields {
            txid: None,
            script: published_script,
            outputs: None,
            outputs_text,
            tx_bytes: None,
            output_addresses: published_addresses,
        });
    };
    let transaction: Transaction = deserialize(&tx_bytes).map_err(|_| SkipReason::Malformed)?;
    if !transaction.is_coinbase() {
        return Err(SkipReason::Malformed);
    }
    let script = transaction
        .input
        .first()
        .ok_or(SkipReason::Malformed)?
        .script_sig
        .as_bytes()
        .to_vec();
    if published_script
        .as_deref()
        .is_some_and(|published| published != script)
    {
        return Err(SkipReason::EvidenceMismatch);
    }
    let decoded_addresses = output_addresses(&transaction.output);
    if published_addresses
        .iter()
        .any(|published| !decoded_addresses.contains(published))
    {
        return Err(SkipReason::EvidenceMismatch);
    }
    Ok(ParentCoinbaseFields {
        txid: Some(transaction.compute_txid().to_byte_array().to_vec()),
        script: Some(script),
        outputs: Some(serialize(&transaction.output)),
        outputs_text,
        tx_bytes: Some(tx_bytes),
        output_addresses: decoded_addresses,
    })
}
