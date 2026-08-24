//! Synchronize the monitor's sparse Bitcoin epoch-header cache from Core.

use anyhow::{Context, Result, bail, ensure};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, CoreHeader, SyncedTip};
use mmm_capture::nbits_table::{DAA_EPOCH_INTERVAL, NbitsTable, daa_epoch_start};
use tokio_postgres::Client;

/// A retarget boundary is re-read from Core until it is this far behind a fresh
/// tip. After that, the boundary's hash, time, and nBits are final cache
/// evidence; a shallower reorg replaces only the moving cache suffix.
pub const CORE_HEADER_REORG_SAFE_DEPTH: i32 = 100;

/// A cache refresh reads several Core RPC responses. Retry a moving tip rather
/// than storing a mixture of two active chains.
const CORE_HEADER_SNAPSHOT_ATTEMPTS: usize = 3;

/// Bring the sparse Core-header cache through the current synced Core tip and
/// return the classification table for this command.
pub async fn refresh_bitcoin_core_header_cache(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
) -> Result<NbitsTable> {
    mmm_store::lock_bitcoin_core_header_cache(client).await?;
    let result = async {
        let (table, update) = refresh_bitcoin_core_header_cache_locked(client, classifier).await?;
        if update.reclassification_needed {
            mmm_read_model::run_reclassify_unknown_parents(
                client,
                classifier,
                mmm_read_model::ReclassifyUnknownParentsConfig {
                    recheck_orphans: update.recheck_orphans,
                    ..Default::default()
                },
            )
            .await
            .context("reclassify orphan placements after Core-cache coverage change")?;
            mmm_store::complete_bitcoin_core_header_cache_reclassification(client).await?;
        }
        Ok(table)
    }
    .await;
    mmm_store::finish_bitcoin_core_header_cache_operation(client, result).await
}

async fn refresh_bitcoin_core_header_cache_locked(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
) -> Result<(NbitsTable, mmm_store::BitcoinCoreHeaderCacheUpdate)> {
    for _ in 0..CORE_HEADER_SNAPSHOT_ATTEMPTS {
        if let Some((table, update)) =
            refresh_bitcoin_core_header_cache_snapshot(client, classifier).await?
        {
            return Ok((table, update));
        }
    }
    bail!("Bitcoin Core tip changed while refreshing the header cache")
}

async fn refresh_bitcoin_core_header_cache_snapshot(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
) -> Result<Option<(NbitsTable, mmm_store::BitcoinCoreHeaderCacheUpdate)>> {
    let tip = classifier
        .synced_tip()
        .await?
        .context("Bitcoin Core must be synced before monitor work can start")?;
    ensure!(
        tip.fresh,
        "Bitcoin Core tip is stale; refusing monitor work"
    );
    ensure!(
        tip.is_mainnet,
        "Bitcoin Core must be connected to mainnet; refusing monitor work"
    );
    let horizon_height = tip.height;
    let prior_horizon = mmm_store::load_bitcoin_core_header_cache_horizon(client).await?;
    let opening_horizon = classifier
        .canonical_header(horizon_height)
        .await
        .with_context(|| {
            format!("fetch opening Bitcoin Core header-cache horizon at {horizon_height}")
        })?;
    let prior_horizon_reorged = match prior_horizon.filter(|header| header.height < horizon_height)
    {
        Some(cached) => {
            let observed = classifier
                .canonical_header(cached.height)
                .await
                .with_context(|| {
                    format!(
                        "verify prior Bitcoin Core header-cache horizon at {}",
                        cached.height
                    )
                })?;
            bitcoin_core_header_cache_row(observed).block_hash != cached.block_hash
        }
        None => false,
    };
    let final_epoch = daa_epoch_start(horizon_height.saturating_sub(CORE_HEADER_REORG_SAFE_DEPTH));
    let highest_final = mmm_store::highest_final_bitcoin_core_epoch(client).await?;
    let next_epoch = highest_final.map_or(0, |height| height + DAA_EPOCH_INTERVAL);
    let mut final_epochs = Vec::new();
    for height in (next_epoch..=final_epoch).step_by(DAA_EPOCH_INTERVAL as usize) {
        let header = classifier
            .canonical_header(height)
            .await
            .with_context(|| format!("fetch Bitcoin Core epoch header at {height}"))?;
        final_epochs.push(bitcoin_core_header_cache_row(header));
    }

    let current_epoch = daa_epoch_start(horizon_height);
    let shallow_epoch = if current_epoch > final_epoch
        && highest_final.is_none_or(|height| current_epoch > height)
    {
        let header = classifier
            .canonical_header(current_epoch)
            .await
            .with_context(|| {
                format!("fetch shallow Bitcoin Core epoch header at {current_epoch}")
            })?;
        Some(bitcoin_core_header_cache_row(header))
    } else {
        None
    };

    let closing_tip = classifier
        .synced_tip()
        .await?
        .context("Bitcoin Core stopped reporting a synced tip during header-cache refresh")?;
    ensure!(
        closing_tip.fresh,
        "Bitcoin Core tip became stale during header-cache refresh"
    );
    ensure!(
        closing_tip.is_mainnet,
        "Bitcoin Core left mainnet during header-cache refresh"
    );
    let closing_horizon = classifier
        .canonical_header(horizon_height)
        .await
        .with_context(|| {
            format!("recheck Bitcoin Core header-cache horizon at {horizon_height}")
        })?;
    if !cache_snapshot_is_current(tip, opening_horizon, closing_tip, closing_horizon) {
        return Ok(None);
    }

    let update = mmm_store::replace_bitcoin_core_header_cache(
        client,
        final_epoch,
        &final_epochs,
        shallow_epoch.as_ref(),
        &bitcoin_core_header_cache_row(opening_horizon),
        prior_horizon_reorged,
    )
    .await?;
    Ok(Some((
        mmm_store::load_bitcoin_core_nbits_table(client).await?,
        update,
    )))
}

fn bitcoin_core_header_cache_row(header: CoreHeader) -> mmm_store::BitcoinCoreHeader {
    mmm_store::BitcoinCoreHeader {
        height: header.height,
        block_hash: header.hash.to_byte_array().to_vec(),
        block_time: header.header_time,
        bits: header.nbits,
    }
}

fn cache_snapshot_is_current(
    opening_tip: SyncedTip,
    opening_horizon: CoreHeader,
    closing_tip: SyncedTip,
    closing_horizon: CoreHeader,
) -> bool {
    opening_tip.height == closing_tip.height && opening_horizon == closing_horizon
}

#[cfg(test)]
mod tests {
    use bitcoin::BlockHash;

    use super::*;

    fn header(height: i32, hash_byte: u8) -> CoreHeader {
        CoreHeader {
            height,
            hash: BlockHash::from_byte_array([hash_byte; 32]),
            header_time: 1,
            nbits: 0x1d00_ffff,
        }
    }

    #[test]
    fn cache_snapshot_rejects_tip_or_horizon_drift() {
        let tip = SyncedTip {
            is_mainnet: true,
            height: 300_000,
            fresh: true,
        };
        let horizon = header(300_000, 1);
        assert!(cache_snapshot_is_current(tip, horizon, tip, horizon));
        assert!(!cache_snapshot_is_current(
            tip,
            horizon,
            tip,
            header(300_000, 2)
        ));
        assert!(!cache_snapshot_is_current(
            tip,
            horizon,
            SyncedTip {
                height: 300_001,
                ..tip
            },
            horizon,
        ));
    }
}
