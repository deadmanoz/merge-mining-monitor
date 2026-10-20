//! The observation phase of `reclassify-pools`: re-resolve the pools of child blocks no event
//! backs (nears and child-only rows) from what their observations store. An evidence row's
//! pools follow its event and are re-resolved by the main and RSK phases.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use tokio_postgres::Client;
use tracing::info;

use mmm_capture::capture::resolve_child_pool_attribution_from_script;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_capture::progress::ProgressReporter;
use mmm_store::{
    ObservationPoolRow, load_child_identity_pools, load_observation_pool_batch,
    update_observation_pools,
};

use super::plan::{StoredParentCoinbase, resolve_stored_parent_pool};
use super::{ReclassifyPoolsConfig, ReclassifyPoolsStats};

pub(super) async fn reresolve_observation_pools(
    client: &Client,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Result<()> {
    let source_id = super::source_filter(client, config).await?;
    let identity_pools = load_child_identity_pools(client).await?;
    let progress = ProgressReporter::new("reclassify-pools-observations", None);
    let mut cursor: Option<i64> = None;
    loop {
        let rows = load_observation_pool_batch(
            client,
            cursor,
            source_id,
            config.overwrite,
            config.batch_size,
        )
        .await?;
        let Some(last) = rows.last() else {
            break;
        };
        cursor = Some(last.id);
        let mut updates = Vec::new();
        for row in &rows {
            if let Some(update) = plan_observation_pools(
                row,
                &identity_pools,
                resolver,
                pool_ids_by_slug,
                config,
                stats,
            ) {
                updates.push(update);
            }
        }
        update_observation_pools(client, &updates).await?;
        stats.observations_scanned += rows.len();
        stats.observation_pool_updates += updates.len();
        progress.advance(rows.len() as u64);
    }
    progress.finish();
    info!(
        phase = "observations",
        rows_scanned = stats.observations_scanned,
        pool_updates = stats.observation_pool_updates,
        "reclassify-pools: observation pass complete"
    );
    Ok(())
}

/// The observation's re-resolved `(id, child_pool_id, btc_parent_pool_id)`, or `None` when
/// nothing changes. A resolved pool fills a missing one, or replaces a different one under
/// `--overwrite`; an unresolved pool never erases a stored one.
fn plan_observation_pools(
    row: &ObservationPoolRow,
    identity_pools: &HashMap<i64, i64>,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Option<(i64, Option<i64>, Option<i64>)> {
    let mut child_pools = row
        .child_identity_ids
        .iter()
        .filter_map(|id| identity_pools.get(id).copied())
        .collect::<BTreeSet<_>>();
    child_pools.extend(
        resolve_child_pool_attribution_from_script(
            row.child_coinbase_script.as_deref(),
            resolver,
            pool_ids_by_slug,
        )
        .and_then(|attribution| attribution.pool_id),
    );
    let child_pool = (child_pools.len() == 1)
        .then(|| child_pools.first().copied())
        .flatten();
    let parent_pool = if row.near {
        let resolution = resolve_stored_parent_pool(
            &StoredParentCoinbase {
                id: row.id,
                script: row.btc_parent_coinbase_script.as_deref(),
                outputs: row.btc_parent_coinbase_outputs.as_deref(),
                outputs_text: None,
            },
            resolver,
            pool_ids_by_slug,
        );
        if resolution.corrupt_outputs {
            stats.corrupt_parent_outputs += 1;
        }
        resolution
            .attribution
            .and_then(|attribution| attribution.pool_id)
    } else {
        None
    };
    let child_pool_id = settle(row.child_pool_id, child_pool, config.overwrite);
    let btc_parent_pool_id = settle(row.btc_parent_pool_id, parent_pool, config.overwrite);
    ((child_pool_id, btc_parent_pool_id) != (row.child_pool_id, row.btc_parent_pool_id))
        .then_some((row.id, child_pool_id, btc_parent_pool_id))
}

fn settle(stored: Option<i64>, resolved: Option<i64>, overwrite: bool) -> Option<i64> {
    match (stored, resolved) {
        (None, resolved) => resolved,
        (Some(_), Some(resolved)) if overwrite => Some(resolved),
        (stored, _) => stored,
    }
}
