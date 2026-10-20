//! Tree window resolution (tip strategies, backbone coverage) and windowed block/event loaders.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio_postgres::Client;

use super::super::ProjectionError;
use super::super::shared::{
    BlockRow, EventRow, classification_filter_params, display_hash,
    ensure_backbone_window_coverage, map_event_rows, rows_to_blocks,
};
use super::{BLOCK_ROW_SELECT, DEFAULT_TREE_SPARSE_SCAN_HEIGHTS, DEFAULT_TREE_WINDOW_HEIGHTS};
use crate::normalize::ParentKind;
use crate::query::TreeQuery;
use crate::query::epoch_start_of_day;
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;

#[derive(Debug, Clone, Copy)]
pub(super) struct ResolvedTreeWindow {
    pub(super) from_height: Option<i32>,
    pub(super) to_height: Option<i32>,
    pub(super) tip_height: Option<i32>,
    pub(super) defaulted_to_tip: bool,
    pub(super) empty_reason: Option<&'static str>,
}

impl ResolvedTreeWindow {
    pub(super) fn explicit(from_height: i32, to_height: i32) -> Self {
        Self {
            from_height: Some(from_height),
            to_height: Some(to_height),
            tip_height: None,
            defaulted_to_tip: false,
            empty_reason: None,
        }
    }

    pub(super) fn for_tip(tip_height: i32) -> Self {
        let window_back = DEFAULT_TREE_WINDOW_HEIGHTS.saturating_sub(1);
        Self {
            from_height: Some(tip_height.saturating_sub(window_back).max(0)),
            to_height: Some(tip_height),
            tip_height: Some(tip_height),
            defaulted_to_tip: true,
            empty_reason: None,
        }
    }

    pub(super) fn empty(reason: &'static str) -> Self {
        Self {
            from_height: None,
            to_height: None,
            tip_height: None,
            defaulted_to_tip: true,
            empty_reason: Some(reason),
        }
    }

    pub(super) fn empty_lookup(reason: &'static str) -> Self {
        Self {
            from_height: None,
            to_height: None,
            tip_height: None,
            defaulted_to_tip: false,
            empty_reason: Some(reason),
        }
    }

    pub(super) fn for_anchor() -> Self {
        Self {
            from_height: None,
            to_height: None,
            tip_height: None,
            defaulted_to_tip: false,
            empty_reason: None,
        }
    }

    pub(super) fn bounds(self) -> Option<(i32, i32)> {
        self.from_height.zip(self.to_height)
    }
}

pub(super) async fn load_active_events_for_tree(
    client: &Client,
    source_filter: &[String],
    query: &TreeQuery,
    block_hashes: &[Vec<u8>],
) -> Result<Vec<EventRow>> {
    let all_sources = source_filter.is_empty();
    let load_direct_unheighted = query.include_unheighted;
    let unheighted_from = query.unheighted_from.map(epoch_start_of_day).unwrap_or(0);
    let unheighted_to = query
        .unheighted_to
        .map(|date| epoch_start_of_day(date) + 86_399)
        .unwrap_or(0);
    let rows = client
        .query(
            "SELECT e.id, e.source_id, s.code, s.kind, s.chain, e.child_height, \
                    e.btc_parent_header_hash, e.btc_parent_prev_header_hash, \
                    e.btc_parent_header_time, e.btc_parent_kind, \
                    e.pow_validates_btc_target, \
                    cmp.id, cmp.slug, cmp.canonical_name \
             FROM merge_mining_event e \
             JOIN source s ON s.id = e.source_id \
             LEFT JOIN child_block_observation o ON o.id = COALESCE(e.observation_id, e.id) \
             LEFT JOIN pool cmp ON cmp.id = o.child_pool_id \
             WHERE e.revoked_at IS NULL \
               AND ($1::boolean OR s.code = ANY($2::text[])) \
               AND ( \
                   e.btc_parent_header_hash = ANY($3::bytea[]) \
                   OR ( \
                       $4::boolean \
                       AND e.btc_parent_kind = 'unknown' \
                       AND e.btc_parent_header_time BETWEEN $5 AND $6 \
                   ) \
               ) \
             ORDER BY e.btc_parent_header_hash, e.id",
            &[
                &all_sources,
                &source_filter,
                &block_hashes,
                &load_direct_unheighted,
                &unheighted_from,
                &unheighted_to,
            ],
        )
        .await
        .context("load tree merge_mining_event rows")?;
    map_event_rows(rows)
}

/// Resolve the default (non-anchor, non-compact) height window in precedence order:
/// `at_height`, `at_time`, explicit `from/to`, then default tip.
pub(super) async fn resolve_tree_window(
    client: &Client,
    query: &TreeQuery,
) -> Result<ResolvedTreeWindow> {
    if let Some(height) = query.at_height {
        return Ok(ResolvedTreeWindow::explicit(height, height));
    }
    if let Some(at_time) = query.at_time {
        return match load_complete_canonical_height_at_time(client, at_time).await? {
            Some(height) => Ok(ResolvedTreeWindow::explicit(height, height)),
            None => Ok(ResolvedTreeWindow::empty_lookup(
                "no_complete_canonical_at_or_before_time",
            )),
        };
    }
    if let (Some(from_height), Some(to_height)) = (query.from_height, query.to_height) {
        return Ok(ResolvedTreeWindow::explicit(from_height, to_height));
    }

    match load_default_tree_tip_height(client).await? {
        Some(tip_height) => Ok(ResolvedTreeWindow::for_tip(tip_height)),
        None => Ok(ResolvedTreeWindow::empty("no_canonical_tip")),
    }
}

pub(super) async fn load_complete_canonical_height_at_time(
    client: &Client,
    at_time: i64,
) -> Result<Option<i32>> {
    let row = client
        .query_opt(
            "SELECT btc_height \
             FROM block \
             WHERE kind = 'canonical' \
               AND btc_coinbase_status = 'complete' \
               AND btc_height IS NOT NULL \
               AND btc_header_time <= $1 \
             ORDER BY btc_header_time DESC, btc_height DESC, btc_header_hash ASC \
             LIMIT 1",
            &[&at_time],
        )
        .await
        .context("resolve complete canonical tree height at time")?;
    Ok(row.map(|row| row.get(0)))
}

pub(super) async fn load_verified_canonical_ancestry(
    client: &Client,
    window: ResolvedTreeWindow,
) -> Result<HashMap<String, String>, ProjectionError> {
    ensure_tree_backbone_coverage(client, window).await?;
    let Some((from_height, to_height)) = window.bounds() else {
        return Ok(HashMap::new());
    };

    let rows = client
        .query(
            "SELECT btc_header_hash, btc_prev_header_hash \
             FROM block \
             WHERE kind = 'canonical' \
               AND btc_height BETWEEN $1 AND $2 \
             ORDER BY btc_height, btc_header_hash",
            &[&from_height, &to_height],
        )
        .await
        .context("load verified canonical ancestry")?;

    let mut ancestry = HashMap::new();
    for row in rows {
        let hash: Vec<u8> = row.get(0);
        let prev_hash: Vec<u8> = row.get(1);
        ancestry.insert(display_hash(&hash)?, display_hash(&prev_hash)?);
    }
    Ok(ancestry)
}

/// Pick the default tip height: contiguous complete run with no higher complete block,
/// then sparse clean-island scan, then contiguous run without the no-higher guard.
pub(super) async fn load_default_tree_tip_height(client: &Client) -> Result<Option<i32>> {
    if let Some(tip_height) = load_contiguous_complete_tree_tip_height(client, true).await? {
        return Ok(Some(tip_height));
    }
    match load_sparse_complete_tree_tip_height(client).await? {
        Some(tip_height) => Ok(Some(tip_height)),
        None => load_contiguous_complete_tree_tip_height(client, false).await,
    }
}

pub(super) async fn load_contiguous_complete_tree_tip_height(
    client: &Client,
    require_no_higher_complete: bool,
) -> Result<Option<i32>> {
    let row = client
        .query_opt(
            "WITH state AS ( \
                 SELECT st.contiguous_complete_height AS tip_height, \
                        GREATEST(st.contiguous_complete_height - ($2::int - 1), 0) AS from_height \
                 FROM bitcoin_core_sync_state st \
                 JOIN source s ON s.id = st.source_id \
                 WHERE s.code = $1 \
                   AND st.sync_mode = 'contiguous' \
                   AND st.contiguous_complete_height >= 0 \
                   AND (NOT $3::boolean OR COALESCE(( \
                       SELECT max(btc_height) \
                       FROM block \
                       WHERE kind = 'canonical' \
                         AND btc_coinbase_status = 'complete' \
                         AND btc_height IS NOT NULL \
                   ), -1) <= st.contiguous_complete_height) \
             ) \
             SELECT tip_height \
             FROM state \
             WHERE ( \
                   SELECT count(*)::int \
                   FROM block b \
                   WHERE b.kind = 'canonical' \
                     AND b.btc_height BETWEEN state.from_height AND state.tip_height \
               ) = state.tip_height - state.from_height + 1 \
               AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM block b \
                   WHERE b.kind = 'canonical' \
                     AND b.btc_height BETWEEN state.from_height AND state.tip_height \
                   GROUP BY b.btc_height \
                   HAVING count(*) <> 1 OR NOT bool_and(b.btc_coinbase_status = 'complete') \
               ) \
               AND NOT EXISTS ( \
                   SELECT 1 \
                   FROM block child \
                   JOIN block parent \
                     ON parent.kind = 'canonical' \
                    AND parent.btc_height = child.btc_height - 1 \
                   WHERE child.kind = 'canonical' \
                     AND child.btc_height BETWEEN state.from_height + 1 AND state.tip_height \
                     AND child.btc_prev_header_hash <> parent.btc_header_hash \
               )",
            &[
                &BITCOIN_SOURCE_CODE,
                &DEFAULT_TREE_WINDOW_HEIGHTS,
                &require_no_higher_complete,
            ],
        )
        .await
        .context("load contiguous complete canonical tree tip")?;
    Ok(row.map(|row| row.get(0)))
}

pub(super) async fn load_sparse_complete_tree_tip_height(client: &Client) -> Result<Option<i32>> {
    let row = client
        .query_opt(
            "WITH max_complete AS ( \
                 SELECT max(btc_height) AS max_height \
                 FROM block \
                 WHERE kind = 'canonical' \
                   AND btc_coinbase_status = 'complete' \
                   AND btc_height IS NOT NULL \
             ), clean_blocks AS ( \
                 SELECT btc_height, \
                        (array_agg(btc_header_hash))[1] AS btc_header_hash, \
                        (array_agg(btc_prev_header_hash))[1] AS btc_prev_header_hash \
                 FROM block, max_complete \
                 WHERE kind = 'canonical' \
                   AND btc_height IS NOT NULL \
                   AND max_complete.max_height IS NOT NULL \
                   AND btc_height >= GREATEST(max_complete.max_height - ($2::int - 1), 0) \
                 GROUP BY btc_height \
                 HAVING count(*) = 1 AND bool_and(btc_coinbase_status = 'complete') \
             ), ordered AS ( \
                 SELECT btc_height, \
                        btc_header_hash, \
                        btc_prev_header_hash, \
                        lag(btc_height) OVER ordered_heights AS prev_height, \
                        lag(btc_header_hash) OVER ordered_heights AS prev_height_hash \
                 FROM clean_blocks \
                 WINDOW ordered_heights AS (ORDER BY btc_height) \
             ), islands AS ( \
                 SELECT btc_height, \
                        sum(CASE \
                            WHEN btc_height = 0 AND prev_height IS NULL THEN 0 \
                            WHEN prev_height = btc_height - 1 \
                             AND btc_prev_header_hash = prev_height_hash THEN 0 \
                            ELSE 1 \
                        END) OVER (ORDER BY btc_height) AS island_id \
                 FROM ordered \
             ), ranked AS ( \
                 SELECT btc_height, \
                        island_id, \
                        row_number() OVER (PARTITION BY island_id ORDER BY btc_height) AS rn \
                 FROM islands \
             ), windowed AS ( \
                 SELECT btc_height AS tip_height, \
                        rn, \
                        lag(btc_height, $1::int - 1) OVER (PARTITION BY island_id ORDER BY btc_height) AS start_height, \
                        lag(rn, $1::int - 1) OVER (PARTITION BY island_id ORDER BY btc_height) AS start_rn \
                 FROM ranked \
             ) \
             SELECT tip_height \
             FROM windowed \
             WHERE ( \
                    tip_height >= $1::int - 1 \
                    AND start_height = tip_height - ($1::int - 1) \
                    AND rn - start_rn = ($1::bigint - 1) \
                 ) \
                OR ( \
                    tip_height < $1::int - 1 \
                    AND rn = tip_height::bigint + 1 \
                 ) \
             ORDER BY tip_height DESC \
             LIMIT 1",
            &[
                &DEFAULT_TREE_WINDOW_HEIGHTS,
                &DEFAULT_TREE_SPARSE_SCAN_HEIGHTS,
            ],
        )
        .await
        .context("load sparse complete canonical tree tip")?;
    Ok(row.map(|row| row.get(0)))
}

pub(super) async fn ensure_tree_backbone_coverage(
    client: &Client,
    window: ResolvedTreeWindow,
) -> Result<(), ProjectionError> {
    let Some((from_height, to_height)) = window.bounds() else {
        return Ok(());
    };
    ensure_backbone_window_coverage(client, from_height, to_height).await
}

/// Load the window's height-spine blocks plus (when `include_unheighted`) null-height
/// `unknown` blocks matching `query.classification`.
pub(super) async fn load_blocks_for_tree(
    client: &Client,
    query: &TreeQuery,
    window: ResolvedTreeWindow,
) -> Result<Vec<BlockRow>> {
    let (load_heighted, from_height, to_height) =
        if let Some((from_height, to_height)) = window.bounds() {
            (true, from_height, to_height)
        } else {
            (false, 0, 0)
        };
    let include_unheighted_unknown =
        query.include_unheighted && query.kinds.contains(&ParentKind::Unknown);
    let unheighted_from = query.unheighted_from.map(epoch_start_of_day).unwrap_or(0);
    let unheighted_to = query
        .unheighted_to
        .map(|date| epoch_start_of_day(date) + 86_399)
        .unwrap_or(0);
    let (class_values, include_pending) = classification_filter_params(&query.classification);
    let sql = format!(
        "{BLOCK_ROW_SELECT} \
         WHERE ( \
                $1::boolean \
                AND b.btc_height BETWEEN $2 AND $3 \
            ) \
            OR ( \
                $4::boolean \
                AND b.btc_height IS NULL \
                AND b.kind = 'unknown' \
                AND b.btc_header_time BETWEEN $5 AND $6 \
                AND ( \
                    b.btc_orphan_class = ANY($7::text[]) \
                    OR ($8::boolean AND b.btc_orphan_class IS NULL) \
                ) \
            )"
    );
    let rows = client
        .query(
            &sql,
            &[
                &load_heighted,
                &from_height,
                &to_height,
                &include_unheighted_unknown,
                &unheighted_from,
                &unheighted_to,
                &class_values,
                &include_pending,
            ],
        )
        .await
        .context("load tree block rows")?;
    rows_to_blocks(rows)
}

/// All block hashes for unheighted `unknown` parents in the time window, regardless of
/// `btc_orphan_class` (used to exclude classified blocks from direct-event projection).
pub(super) async fn load_unheighted_unknown_block_hashes(
    client: &Client,
    from_epoch: i64,
    to_epoch: i64,
) -> Result<Vec<Vec<u8>>> {
    let rows = client
        .query(
            "SELECT btc_header_hash FROM block \
             WHERE kind = 'unknown' AND btc_height IS NULL \
               AND btc_header_time BETWEEN $1 AND $2",
            &[&from_epoch, &to_epoch],
        )
        .await
        .context("load unheighted unknown block hashes for direct de-dup")?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}
