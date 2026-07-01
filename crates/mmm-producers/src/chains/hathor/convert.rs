//! Small BTC-address / hash conversions for the reconstructed Hathor parent,
//! split out of `capture` to keep the state-machine file within the size gate.

use std::str::FromStr;

use anyhow::{Context, Result};
use bitcoin::hashes::Hash as _;
use bitcoin::{Address, BlockHash, Network, Transaction};

/// BTC mainnet payout addresses derived from the reconstructed coinbase outputs.
pub(super) fn derive_output_addresses(coinbase: &Transaction) -> Vec<String> {
    coinbase
        .output
        .iter()
        .filter_map(|out| {
            Address::from_script(&out.script_pubkey, Network::Bitcoin)
                .ok()
                .map(|address| address.to_string())
        })
        .collect()
}

/// Parse a display-order Hathor block hash into internal (wire) byte order, as
/// stored in `merge_mining_event.child_block_hash`.
pub(super) fn block_hash_internal(tx_id: &str) -> Result<Vec<u8>> {
    Ok(BlockHash::from_str(tx_id)
        .context("parse Hathor block hash")?
        .to_byte_array()
        .to_vec())
}
