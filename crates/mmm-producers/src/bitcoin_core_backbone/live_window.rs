//! Bitcoin Core live-tip window repair and verification.
//!
//! Fills holes in `[tip - window + 1, tip]` and verifies a single unbroken,
//! coinbase-complete canonical chain ending at the captured Core tip; any
//! breach raises `BackboneIntegrityError::LiveWindowInvariant`.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use bitcoin::hashes::Hash as _;
use serde_json::{Value, json};
use tokio_postgres::Client;

use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use mmm_store::get_source_id;

use super::{
    BackboneIntegrityError, BitcoinCoreBackboneSource, BitcoinCoreBackboneTip,
    BitcoinCoreSyncConfig, BitcoinCoreSyncStats, LIVE_WINDOW_INVARIANT_ERROR_CODE, integrity_error,
    load_or_init_sync_state, run_sync_bitcoin_core, update_sync_error,
};

#[derive(Debug, Clone)]
struct CanonicalWindowRow {
    height: i32,
    hash: Vec<u8>,
    prev_hash: Vec<u8>,
    coinbase_status: String,
}

#[derive(Debug, Clone, Copy)]
struct LiveWindowContext {
    source_id: i64,
    start_height: i32,
    target: BitcoinCoreBackboneTip,
}

pub(super) fn live_backbone_window_start_height(tip_height: i32, window_heights: i32) -> i32 {
    tip_height
        .saturating_sub(window_heights.saturating_sub(1))
        .max(0)
}

/// Fill any holes in `[start, target.height]` via a bounded `missing_only`
/// batch, verifying `target.hash` against Core before and after.
pub(crate) async fn repair_near_tip_gaps_to_target<S>(
    client: &mut Client,
    source: &S,
    target: BitcoinCoreBackboneTip,
    delay: Duration,
    window_heights: i32,
) -> Result<BitcoinCoreSyncStats>
where
    S: BitcoinCoreBackboneSource,
{
    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    load_or_init_sync_state(client, source_id).await?;
    verify_live_backbone_target_hash(client, source, source_id, target).await?;
    let stats = run_sync_bitcoin_core(
        client,
        source,
        BitcoinCoreSyncConfig {
            from_height: Some(live_backbone_window_start_height(
                target.height,
                window_heights,
            )),
            to_height: Some(target.height),
            missing_only: true,
            tip: false,
            delay,
            ..BitcoinCoreSyncConfig::default()
        },
    )
    .await?;
    verify_live_backbone_target_hash(client, source, source_id, target).await?;
    Ok(stats)
}

/// Assert the live window is a gapless, coinbase-complete canonical chain
/// ending on `target`.
pub(crate) async fn verify_live_backbone_window<S>(
    client: &Client,
    source: &S,
    target: BitcoinCoreBackboneTip,
    window_heights: i32,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    load_or_init_sync_state(client, source_id).await?;
    verify_live_backbone_target_hash(client, source, source_id, target).await?;

    let window_start = live_backbone_window_start_height(target.height, window_heights);
    let rows = load_canonical_window_rows(client, window_start, target.height).await?;
    let mut rows_by_height: BTreeMap<i32, Vec<CanonicalWindowRow>> = BTreeMap::new();
    for row in rows {
        rows_by_height.entry(row.height).or_default().push(row);
    }
    verify_live_backbone_window_rows(
        client,
        LiveWindowContext {
            source_id,
            start_height: window_start,
            target,
        },
        rows_by_height,
    )
    .await
}

async fn verify_live_backbone_window_rows(
    client: &Client,
    ctx: LiveWindowContext,
    mut rows_by_height: BTreeMap<i32, Vec<CanonicalWindowRow>>,
) -> Result<()> {
    let mut previous_hash: Option<Vec<u8>> = None;
    let mut tip_hash: Option<Vec<u8>> = None;
    for height in ctx.start_height..=ctx.target.height {
        let row = take_single_live_window_row(client, ctx, &mut rows_by_height, height).await?;
        verify_live_window_coinbase(client, ctx, &row).await?;
        verify_live_window_prev_link(client, ctx, &row, previous_hash.as_deref()).await?;
        if height == ctx.target.height {
            tip_hash = Some(row.hash.clone());
        }
        previous_hash = Some(row.hash);
    }
    verify_live_window_tip_hash(client, ctx, tip_hash).await
}

async fn take_single_live_window_row(
    client: &Client,
    ctx: LiveWindowContext,
    rows_by_height: &mut BTreeMap<i32, Vec<CanonicalWindowRow>>,
    height: i32,
) -> Result<CanonicalWindowRow> {
    let rows = rows_by_height.remove(&height).unwrap_or_default();
    if rows.is_empty() {
        return live_window_invariant_failure(
            client,
            ctx.source_id,
            height,
            format!(
                "Bitcoin Core live backbone window missing canonical height {height} \
                 in {}..={}",
                ctx.start_height, ctx.target.height
            ),
            json!({
                "reason": "missing_height",
                "window_start": ctx.start_height,
                "window_end": ctx.target.height,
                "missing_height": height,
                "target_tip_hash": ctx.target.hash.to_string(),
            }),
        )
        .await;
    }
    if rows.len() != 1 {
        return live_window_invariant_failure(
            client,
            ctx.source_id,
            height,
            format!(
                "Bitcoin Core live backbone window has {} canonical rows at height {height}",
                rows.len()
            ),
            json!({
                "reason": "duplicate_height",
                "window_start": ctx.start_height,
                "window_end": ctx.target.height,
                "height": height,
                "row_count": rows.len(),
                "hashes": rows.iter().map(|row| hex::encode(&row.hash)).collect::<Vec<_>>(),
                "target_tip_hash": ctx.target.hash.to_string(),
            }),
        )
        .await;
    }
    Ok(rows.into_iter().next().expect("row cardinality checked"))
}

async fn verify_live_window_coinbase(
    client: &Client,
    ctx: LiveWindowContext,
    row: &CanonicalWindowRow,
) -> Result<()> {
    if row.coinbase_status == "complete" {
        return Ok(());
    }
    live_window_invariant_failure(
        client,
        ctx.source_id,
        row.height,
        format!(
            "Bitcoin Core live backbone window height {} has incomplete coinbase status {}",
            row.height, row.coinbase_status
        ),
        json!({
            "reason": "incomplete_coinbase",
            "window_start": ctx.start_height,
            "window_end": ctx.target.height,
            "height": row.height,
            "coinbase_status": row.coinbase_status,
            "hash": hex::encode(&row.hash),
            "target_tip_hash": ctx.target.hash.to_string(),
        }),
    )
    .await
}

async fn verify_live_window_prev_link(
    client: &Client,
    ctx: LiveWindowContext,
    row: &CanonicalWindowRow,
    previous_hash: Option<&[u8]>,
) -> Result<()> {
    let Some(previous_hash) = previous_hash else {
        return Ok(());
    };
    if row.prev_hash.as_slice() == previous_hash {
        return Ok(());
    }
    live_window_invariant_failure(
        client,
        ctx.source_id,
        row.height,
        format!(
            "Bitcoin Core live backbone window prev-link mismatch at height {}: prev={} previous_height_hash={}",
            row.height,
            hex::encode(&row.prev_hash),
            hex::encode(previous_hash)
        ),
        json!({
            "reason": "prev_link_mismatch",
            "window_start": ctx.start_height,
            "window_end": ctx.target.height,
            "height": row.height,
            "expected_prev_hash": hex::encode(previous_hash),
            "actual_prev_hash": hex::encode(&row.prev_hash),
            "hash": hex::encode(&row.hash),
            "target_tip_hash": ctx.target.hash.to_string(),
        }),
    )
    .await
}

async fn verify_live_window_tip_hash(
    client: &Client,
    ctx: LiveWindowContext,
    tip_hash: Option<Vec<u8>>,
) -> Result<()> {
    let target_hash = ctx.target.hash.to_byte_array();
    if tip_hash.as_deref() == Some(target_hash.as_slice()) {
        return Ok(());
    }
    live_window_invariant_failure(
        client,
        ctx.source_id,
        ctx.target.height,
        format!(
            "Bitcoin Core live backbone window tip hash mismatch at height {}: Core={} local={}",
            ctx.target.height,
            ctx.target.hash,
            tip_hash
                .as_ref()
                .map(hex::encode)
                .unwrap_or_else(|| "<missing>".to_owned())
        ),
        json!({
            "reason": "tip_hash_mismatch",
            "window_start": ctx.start_height,
            "window_end": ctx.target.height,
            "height": ctx.target.height,
            "expected_tip_hash": ctx.target.hash.to_string(),
            "local_tip_hash": tip_hash.map(hex::encode),
        }),
    )
    .await
}

async fn load_canonical_window_rows(
    client: &Client,
    from_height: i32,
    to_height: i32,
) -> Result<Vec<CanonicalWindowRow>> {
    let rows = client
        .query(
            "SELECT btc_height, btc_header_hash, btc_prev_header_hash, btc_coinbase_status \
             FROM block \
             WHERE kind = 'canonical' \
               AND btc_height BETWEEN $1 AND $2 \
             ORDER BY btc_height, btc_header_hash",
            &[&from_height, &to_height],
        )
        .await
        .with_context(|| {
            format!("load canonical live backbone window {from_height}..={to_height}")
        })?;
    Ok(rows
        .into_iter()
        .map(|row| CanonicalWindowRow {
            height: row
                .get::<_, Option<i32>>(0)
                .expect("canonical rows have heights"),
            hash: row.get(1),
            prev_hash: row.get(2),
            coinbase_status: row.get(3),
        })
        .collect())
}

async fn verify_live_backbone_target_hash<S>(
    client: &Client,
    source: &S,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    let current_hash = source.block_hash(target.height).await.with_context(|| {
        format!(
            "confirm Bitcoin Core live backbone target hash at height {}",
            target.height
        )
    })?;
    if current_hash == target.hash {
        return Ok(());
    }
    live_window_invariant_failure(
        client,
        source_id,
        target.height,
        format!(
            "Bitcoin Core live backbone window target hash changed at height {}: captured={} current={}",
            target.height, target.hash, current_hash
        ),
        json!({
            "reason": "target_hash_mismatch",
            "height": target.height,
            "captured_tip_hash": target.hash.to_string(),
            "current_core_hash": current_hash.to_string(),
        }),
    )
    .await
}

async fn live_window_invariant_failure<T>(
    client: &Client,
    source_id: i64,
    height: i32,
    message: String,
    details: Value,
) -> Result<T> {
    update_sync_error(
        client,
        source_id,
        height,
        LIVE_WINDOW_INVARIANT_ERROR_CODE,
        &message,
        details,
    )
    .await?;
    Err(integrity_error(
        BackboneIntegrityError::LiveWindowInvariant,
        message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_backbone_window_start_height_clamps_to_zero_and_tracks_window() {
        let window = 64;
        assert_eq!(live_backbone_window_start_height(0, window), 0);
        assert_eq!(live_backbone_window_start_height(1, window), 0);
        assert_eq!(live_backbone_window_start_height(window - 1, window), 0);
        assert_eq!(live_backbone_window_start_height(window, window), 1);
        assert_eq!(live_backbone_window_start_height(953_621, window), 953_558);
        assert_eq!(live_backbone_window_start_height(953_621, 32), 953_590);
    }
}
