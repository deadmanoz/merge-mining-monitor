//! Bitcoin-lineage gate on live capture (`mmm_capture::lineage::bitcoin_lineage`).
//!
//! Runs under the shared Core-cache lock before classifying or writing a parent:
//! foreign-lineage parents are never stored (they retract any event this source held
//! for the child block, which keeps only a child-side observation), while
//! above-horizon pending parents roll back.

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use tokio_postgres::{GenericClient, Transaction};

use mmm_capture::btc_orphan::strict_bip34_height_from_evidence;
use mmm_capture::capture::{ChildBlockObservation, MergeMiningEventPayload};
use mmm_capture::lineage::{Lineage, ParentLineageInput, bitcoin_lineage};
use mmm_store::CaptureWrite;

use super::PrimarySourceHealthBracket;
use crate::{load_block_cascade_state, lock_block_hashes, rebuild_parent_read_model};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// What the capture wrote: an event, or a near's observation alone.
    Written(CaptureWrite),
    /// Foreign-lineage parent: prior child-block events for this source removed and only the
    /// child block's `non_bitcoin_parent` observation written.
    NotBitcoin {
        reason: &'static str,
        core_cache_generation: i64,
    },
    /// Parent lineage needs a Core header-cache epoch not yet reached.
    LineagePending(&'static str),
}

/// Decide the payload parent's lineage (`None` admits it).
pub(super) async fn refuse_parent<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<Option<CaptureOutcome>> {
    if mmm_capture::invalid_blocks::lookup(&payload.btc_parent_header_hash).is_some() {
        return Ok(None);
    }
    let header: Header = deserialize(&payload.btc_parent_header_bytes)
        .context("deserialize payload parent header for the lineage gate")?;
    let prev_height = placed_prev_height(client, &payload.btc_parent_prev_header_hash).await?;
    if prev_height.is_some() && !payload.pow_validates_btc_target {
        return Ok(None);
    }
    let strict_height = match prev_height {
        Some(_) => None,
        None => strict_height(client, source_id, payload).await?,
    };
    let table = mmm_store::load_bitcoin_core_nbits_table(client).await?;
    let input = ParentLineageInput {
        bits: header.bits,
        time: i64::from(header.time),
        meets_own_target: payload.pow_validates_btc_target,
        prev_height,
        strict_height,
    };
    Ok(match bitcoin_lineage(&input, &table) {
        Lineage::Bitcoin(_) => None,
        Lineage::NotBitcoin(reason) => Some(CaptureOutcome::NotBitcoin {
            reason,
            core_cache_generation: mmm_store::load_core_cache_generation(client).await?,
        }),
        Lineage::Pending(reason) => Some(CaptureOutcome::LineagePending(reason)),
    })
}

async fn placed_prev_height<C: GenericClient>(client: &C, prev_hash: &[u8]) -> Result<Option<i32>> {
    let placed = client
        .query_opt(
            "SELECT btc_height FROM block \
              WHERE btc_header_hash = $1 AND btc_height IS NOT NULL \
                AND kind IN ('canonical', 'stale', 'invalid_block')",
            &[&prev_hash],
        )
        .await
        .context("load the parent prev's placement")?
        .map(|row| row.get(0));
    match placed {
        Some(height) => Ok(Some(height)),
        None => mmm_store::cached_bitcoin_core_header_height(client, prev_hash).await,
    }
}

async fn strict_height<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<Option<i32>> {
    let Some(script) = payload.btc_parent_coinbase_script.as_deref() else {
        return Ok(None);
    };
    let chain: Option<String> = client
        .query_one("SELECT chain FROM source WHERE id = $1", &[&source_id])
        .await
        .context("load the capturing source's chain")?
        .get(0);
    Ok(chain.and_then(|chain| {
        strict_bip34_height_from_evidence(
            &chain,
            script,
            payload.btc_parent_coinbase_tx_bytes.as_deref(),
        )
    }))
}

/// Rewrite a child block whose parent failed the lineage gate as `non_bitcoin_parent`. When
/// the block counts toward source health (an event or its near observation), its events and
/// hashless observations are deleted and the rewrite and parent rebuild run inside the
/// source-health bracket, so a refused near leaves the near count.
pub(super) async fn retract_child_block(
    txn: &Transaction<'_>,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<Vec<Vec<u8>>> {
    let Some(child_hash) = payload.child_block_hash.as_deref() else {
        return Ok(Vec::new());
    };
    let observation = ChildBlockObservation::non_bitcoin_parent(payload);
    let parent = &payload.btc_parent_header_hash;
    let height = payload.child_height;
    if !mmm_store::child_block_is_counted(txn, source_id, height, child_hash, parent).await? {
        mmm_store::upsert_child_only_observation(txn, source_id, &observation).await?;
        return Ok(Vec::new());
    }
    let mut hashes = vec![parent.clone(), payload.btc_parent_prev_header_hash.clone()];
    hashes.sort();
    hashes.dedup();
    lock_block_hashes(txn, &hashes).await?;
    let before = load_block_cascade_state(txn, parent).await?;
    let bracket = PrimarySourceHealthBracket::open(txn, parent).await?;
    mmm_store::delete_child_block_events(txn, source_id, height, child_hash, parent).await?;
    mmm_store::upsert_child_only_observation(txn, source_id, &observation).await?;
    rebuild_parent_read_model(txn, parent, None, None).await?;
    bracket.close(txn).await?;
    let after = load_block_cascade_state(txn, parent).await?;
    Ok(if before == after {
        Vec::new()
    } else {
        vec![parent.clone()]
    })
}
