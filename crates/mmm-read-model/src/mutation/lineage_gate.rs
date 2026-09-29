//! The Bitcoin-lineage gate on live capture.
//!
//! A merge-mined child chain can commit to any SHA-256 parent, so a proof that
//! verifies says nothing about whether its parent is a Bitcoin header. Before
//! a live capture classifies or writes anything, this gate decides the
//! parent's lineage from local evidence (`mmm_capture::lineage`): the pinned
//! error-block catalogue first, then where the parent's prev sits, the
//! parent coinbase height and Bitcoin's difficulty history from the Core
//! header cache. A parent from another chain is never stored, and a parent
//! whose verdict needs an epoch the cache has not reached yet is left for a
//! later capture.
//!
//! The gate runs under the shared Core-cache lock the capture already holds,
//! so the cache it reads cannot be replaced before the capture commits.

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use tokio_postgres::{GenericClient, Transaction};

use mmm_capture::btc_orphan::strict_bip34_height_from_evidence;
use mmm_capture::capture::MergeMiningEventPayload;
use mmm_capture::lineage::{Lineage, ParentLineageInput, bitcoin_lineage};

use super::PrimarySourceHealthBracket;
use crate::{load_block_cascade_state, lock_block_hashes, rebuild_parent_read_model};

/// What a live capture made of one child block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureOutcome {
    /// The parent is a Bitcoin header and the event was written.
    Written(i64),
    /// The parent belongs to another chain. Nothing was written, and any
    /// event this source held for the child block was removed. The reason
    /// names the failed check; `core_cache_generation` is the Core-cache
    /// generation the verdict was decided under, which the producer stamps on
    /// the block's child head so a later verdict-changing refresh reopens it.
    NotBitcoin {
        reason: &'static str,
        core_cache_generation: i64,
    },
    /// The parent's lineage needs an epoch the Core header cache has not
    /// reached yet. Nothing was written.
    LineagePending(&'static str),
}

/// Decide the payload parent's lineage. `None` admits it.
pub(super) async fn refuse_parent<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<Option<CaptureOutcome>> {
    // A catalogued block can carry wrong bits by design (717,696 kept the
    // previous epoch's), and it is Bitcoin's whatever the rule says.
    if mmm_capture::error_blocks::lookup(&payload.btc_parent_header_hash).is_some() {
        return Ok(None);
    }
    let header: Header = deserialize(&payload.btc_parent_header_bytes)
        .context("deserialize payload parent header for the lineage gate")?;
    let prev_height = placed_prev_height(client, &payload.btc_parent_prev_header_hash).await?;
    // A share on a Bitcoin prev is Bitcoin's whatever its bits, so the common
    // live case needs neither the difficulty table nor the coinbase.
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

/// The Bitcoin height of a prev the Monitor has placed: a canonical, stale or
/// catalogued error block, or a header in the Core cache (the tip it last
/// refreshed to, before the canonical sync writes its row).
async fn placed_prev_height<C: GenericClient>(client: &C, prev_hash: &[u8]) -> Result<Option<i32>> {
    let placed = client
        .query_opt(
            "SELECT btc_height FROM block \
              WHERE btc_header_hash = $1 AND btc_height IS NOT NULL \
                AND kind IN ('canonical', 'stale', 'error_block')",
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

/// The parent coinbase BIP34 height, for chains whose parent coinbase is real
/// Bitcoin coinbase data.
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

/// Remove this source's event for a child block whose parent is not Bitcoin's
/// and rebuild the parent's derived rows, inside the capture transaction.
/// A verdict can turn against a stored parent only after a Core-cache change,
/// which makes the height's child head non-final, so the next capture of the
/// block lands here. Returns the parent when its derived state changed, for
/// the dependent cascade.
pub(super) async fn retract_child_block(
    txn: &Transaction<'_>,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<Vec<Vec<u8>>> {
    let (Some(height), Some(child_hash)) =
        (payload.child_height, payload.child_block_hash.as_deref())
    else {
        return Ok(Vec::new());
    };
    let parent = &payload.btc_parent_header_hash;
    if !mmm_store::has_child_block_events(txn, source_id, height, child_hash, parent).await? {
        return Ok(Vec::new());
    }
    let mut hashes = vec![parent.clone(), payload.btc_parent_prev_header_hash.clone()];
    hashes.sort();
    hashes.dedup();
    lock_block_hashes(txn, &hashes).await?;
    let before = load_block_cascade_state(txn, parent).await?;
    let bracket = PrimarySourceHealthBracket::open(txn, parent).await?;
    mmm_store::delete_child_block_events(txn, source_id, height, child_hash, parent).await?;
    rebuild_parent_read_model(txn, parent, None, None).await?;
    bracket.close(txn).await?;
    let after = load_block_cascade_state(txn, parent).await?;
    Ok(if before == after {
        Vec::new()
    } else {
        vec![parent.clone()]
    })
}
