//! Synchronize the monitor's sparse Bitcoin epoch-header cache from Core.

use anyhow::{Context, Result};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, CoreFault, CoreHeader, SyncedTip};
use mmm_capture::nbits_table::{DAA_EPOCH_INTERVAL, NbitsTable, daa_epoch_start};
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use tokio_postgres::Client;
use tracing::info;

/// A retarget boundary is re-read from Core until it is this far behind a fresh
/// tip. After that, the boundary's hash, time, and nBits are final cache
/// evidence; a shallower reorg replaces only the moving cache suffix.
pub const CORE_HEADER_REORG_SAFE_DEPTH: i32 = 100;

/// A cache refresh reads several Core RPC responses. Retry a moving tip rather
/// than storing a mixture of two active chains.
const CORE_HEADER_SNAPSHOT_ATTEMPTS: usize = 3;

/// A bounded local recovery step yielded. This is neither a Core outage nor a healthy cycle.
#[derive(Debug, Clone, Copy)]
pub struct LocalRecoveryPending(pub &'static str);
impl std::fmt::Display for LocalRecoveryPending {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for LocalRecoveryPending {}

/// Stage network observations without the exclusive lock, then publish only if
/// the persisted revision and queue still match. A live call drains one batch.
pub async fn refresh_bitcoin_core_header_cache(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
) -> Result<NbitsTable> {
    ready_tip(classifier).await?;
    let bitcoin_source_id = mmm_store::get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    drain_core_queue(client, bitcoin_source_id, classifier).await?;
    for attempt in 0..CORE_HEADER_SNAPSHOT_ATTEMPTS {
        if attempt > 0 && !classifier.is_live() {
            // A suffix replacement may enqueue work during preparation.
            drain_core_queue(client, bitcoin_source_id, classifier).await?;
        }
        match refresh_bitcoin_core_header_cache_snapshot(client, classifier).await {
            Ok(Some((table, update))) => {
                if update.pending {
                    info!(
                        "an unknown-parent recheck is pending; run reclassify-unknown-parents --scheduled"
                    );
                }
                return Ok(table);
            }
            Ok(None) => {}
            Err(err)
                if err
                    .downcast_ref::<tokio_postgres::Error>()
                    .and_then(tokio_postgres::Error::code)
                    == Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE) => {}
            Err(err) => return Err(err),
        }
    }
    Err(LocalRecoveryPending("core_cache_publication_pending").into())
}

pub(crate) async fn drain_core_queue(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    if classifier.is_live() {
        if !mmm_read_model::drain_core_reconcile_queue_batch(
            client,
            source_id,
            classifier,
            mmm_read_model::CORE_RECOVERY_BATCH,
        )
        .await?
        {
            return Err(LocalRecoveryPending("core_reconciliation_pending").into());
        }
        Ok(())
    } else {
        mmm_read_model::drain_core_reconcile_queue(client, source_id, classifier).await
    }
}

async fn ready_tip(classifier: &ConfiguredParentClassifier) -> Result<SyncedTip> {
    classifier.ready_tip().await.map_err(|err| {
        if err.downcast_ref::<CoreFault>() == Some(&CoreFault::WrongNetwork) {
            err.context(mmm_store::BitcoinCoreHeaderCacheIntegrityError)
                .context("Bitcoin Core must be connected to mainnet; refusing monitor work")
        } else {
            err
        }
    })
}

async fn refresh_bitcoin_core_header_cache_snapshot(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
) -> Result<Option<(NbitsTable, mmm_store::BitcoinCoreHeaderCacheUpdate)>> {
    let tip = ready_tip(classifier).await?;
    let revision = mmm_store::load_core_cache_publication_revision(client).await?;
    let highest_final = mmm_store::highest_final_bitcoin_core_epoch(client).await?;
    let next_epoch = highest_final.map_or(0, |height| height + DAA_EPOCH_INTERVAL);
    // A cold live start publishes a safe prefix, then yields before fetching the
    // next 25 boundaries. Its horizon never promises coverage of missing epochs.
    let horizon_height = if classifier.is_live() {
        tip.height
            .min(next_epoch + 24 * DAA_EPOCH_INTERVAL + CORE_HEADER_REORG_SAFE_DEPTH)
    } else {
        tip.height
    };
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
    let final_epochs = fetch_final_epochs(classifier, next_epoch, final_epoch).await?;

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

    let closing_tip = ready_tip(classifier).await?;
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
        revision,
    )
    .await?;
    let Some(update) = update else {
        return Ok(None);
    };
    if horizon_height < tip.height {
        return Err(LocalRecoveryPending("core_cache_bootstrap_pending").into());
    }
    Ok(Some((
        mmm_store::load_bitcoin_core_nbits_table(client).await?,
        update,
    )))
}

async fn fetch_final_epochs(
    classifier: &ConfiguredParentClassifier,
    first: i32,
    last: i32,
) -> Result<Vec<mmm_store::BitcoinCoreHeader>> {
    if first > last {
        return Ok(Vec::new());
    }
    let total = u64::try_from((last - first) / DAA_EPOCH_INTERVAL + 1)?;
    let mut progress =
        mmm_capture::progress::ProgressReporter::new("core-cache-epochs", Some(total));
    if let Some(metrics) = classifier.metrics() {
        progress = progress.with_summary(metrics.summary());
    }
    let mut epochs = Vec::new();
    for height in (first..=last).step_by(DAA_EPOCH_INTERVAL as usize) {
        let header = classifier
            .canonical_header(height)
            .await
            .with_context(|| format!("fetch Bitcoin Core epoch header at {height}"))?;
        epochs.push(bitcoin_core_header_cache_row(header));
        progress.advance(1);
    }
    progress.finish();
    Ok(epochs)
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
