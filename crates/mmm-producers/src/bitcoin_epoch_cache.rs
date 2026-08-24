//! Synchronize the monitor's sparse Bitcoin epoch-header cache from Core.

use anyhow::{Context, Result, ensure};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::nbits_table::{DAA_EPOCH_INTERVAL, NbitsTable, daa_epoch_start};
use tokio_postgres::Client;

/// Bring the sparse Core-header cache through the current synced Core tip and
/// return the classification table for this command.
pub async fn refresh_bitcoin_core_header_cache(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
) -> Result<NbitsTable> {
    let tip = classifier
        .synced_tip()
        .await?
        .context("Bitcoin Core must be synced before monitor work can start")?;
    ensure!(
        tip.fresh,
        "Bitcoin Core tip is stale; refusing monitor work"
    );
    let horizon_height = tip.height;

    let required_epoch = daa_epoch_start(horizon_height);
    let next_epoch = mmm_store::highest_bitcoin_core_epoch(client)
        .await?
        .map_or(0, |height| height + DAA_EPOCH_INTERVAL);
    for height in (next_epoch..=required_epoch).step_by(DAA_EPOCH_INTERVAL as usize) {
        let header = classifier
            .canonical_header(height)
            .await
            .with_context(|| format!("fetch Bitcoin Core epoch header at {height}"))?;
        mmm_store::record_bitcoin_core_header(
            client,
            &mmm_store::BitcoinCoreHeader {
                height: header.height,
                block_hash: header.hash.to_byte_array().to_vec(),
                block_time: header.header_time,
                bits: header.nbits,
            },
        )
        .await?;
    }

    let horizon = classifier
        .canonical_header(horizon_height)
        .await
        .with_context(|| format!("fetch Bitcoin Core header-cache horizon at {horizon_height}"))?;
    mmm_store::replace_bitcoin_core_header_horizon(
        client,
        &mmm_store::BitcoinCoreHeader {
            height: horizon.height,
            block_hash: horizon.hash.to_byte_array().to_vec(),
            block_time: horizon.header_time,
            bits: horizon.nbits,
        },
    )
    .await?;
    mmm_store::load_bitcoin_core_nbits_table(client).await
}
