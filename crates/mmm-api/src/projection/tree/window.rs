//! Tree window resolution (tip strategies, backbone coverage) and the
//! windowed block/event loaders.

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

/// Resolved height window for a `/tree` request: the `(from,to)` BTC-height bounds
/// (None for an unbounded/empty/anchor view), the `tip_height` for a defaulted
/// view, and the `empty_reason` machine string. Copied verbatim into the wire
/// `TreeWindow`; constructors below encode each resolution strategy.
#[derive(Debug, Clone, Copy)]
pub(super) struct ResolvedTreeWindow {
    pub(super) from_height: Option<i32>,
    pub(super) to_height: Option<i32>,
    pub(super) tip_height: Option<i32>,
    pub(super) defaulted_to_tip: bool,
    pub(super) empty_reason: Option<&'static str>,
}

impl ResolvedTreeWindow {
    /// A fully-bounded window from explicit `[from_height, to_height]` (exact-height,
    /// from/to params, compact radius windows, and the anchor placement window).
    /// Not a tip default and never empty.
    pub(super) fn explicit(from_height: i32, to_height: i32) -> Self {
        Self {
            from_height: Some(from_height),
            to_height: Some(to_height),
            tip_height: None,
            defaulted_to_tip: false,
            empty_reason: None,
        }
    }

    /// The no-param tip default: a DEFAULT_TREE_WINDOW_HEIGHTS-wide window ending at
    /// `tip_height` (clamped at genesis), with `defaulted_to_tip = true` so the
    /// frontend can flag the auto-selected view.
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

    /// A defaulted tip view that found no canonical tip: no bounds,
    /// `defaulted_to_tip = true` (a tip attempt), and the supplied `empty_reason`
    /// (e.g. `no_canonical_tip`). Kept distinct from `empty_lookup` so the wire
    /// `defaulted_to_tip` flag stays truthful.
    pub(super) fn empty(reason: &'static str) -> Self {
        Self {
            from_height: None,
            to_height: None,
            tip_height: None,
            defaulted_to_tip: true,
            empty_reason: Some(reason),
        }
    }

    /// An explicit at_time lookup that resolved to nothing (no complete canonical at
    /// or before the time): no bounds, `defaulted_to_tip = false` (an explicit query,
    /// not a tip default), with the supplied `empty_reason`. The `false` here vs
    /// `empty`'s `true` is a wire field, so the two stay separate.
    pub(super) fn empty_lookup(reason: &'static str) -> Self {
        Self {
            from_height: None,
            to_height: None,
            tip_height: None,
            defaulted_to_tip: false,
            empty_reason: Some(reason),
        }
    }

    /// Anchor mode shows the selected unknown plus its nearest unknown neighbors
    /// with no height spine. It is an explicit user view, not a tip default, so
    /// `defaulted_to_tip` is false and there is no `empty_reason`.
    pub(super) fn for_anchor() -> Self {
        Self {
            from_height: None,
            to_height: None,
            tip_height: None,
            defaulted_to_tip: false,
            empty_reason: None,
        }
    }

    /// The `(from,to)` height bounds when both are present, else `None`. Loaders and
    /// coverage checks branch on this to decide whether the window has a height spine
    /// at all (anchor/empty windows return `None`).
    pub(super) fn bounds(self) -> Option<(i32, i32)> {
        self.from_height.zip(self.to_height)
    }
}

/// Load non-revoked `merge_mining_event` rows for the tree: those attached to the
/// window's block hashes, plus (when `include_unheighted`) the `near`/`unknown`
/// direct rows whose parent header time is in the date window. `ORDER BY
/// (btc_parent_header_hash, e.id)` keeps per-parent grouping deterministic for
/// projection. Honors the source filter (`$1` all-sources bypass).
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
             LEFT JOIN pool cmp ON cmp.id = e.child_miner_pool_id \
             WHERE e.revoked_at IS NULL \
               AND ($1::boolean OR s.code = ANY($2::text[])) \
               AND ( \
                   e.btc_parent_header_hash = ANY($3::bytea[]) \
                   OR ( \
                       $4::boolean \
                       AND e.btc_parent_kind IN ('near', 'unknown') \
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

/// Resolve the height window for the default (non-anchor, non-compact) path, in
/// precedence order: `at_height` (exact), `at_time` (nearest complete-canonical at
/// or before, else `empty_lookup`), explicit `from/to`, then the default tip (else
/// `empty` with `no_canonical_tip`). This precedence is the wire contract.
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

/// Newest complete-canonical height with `btc_header_time <= at_time`, or `None`.
/// Backs the `at_time` window/compact target resolution. `ORDER BY btc_header_time
/// DESC, btc_height DESC, btc_header_hash ASC LIMIT 1` makes the at-or-before pick
/// deterministic across equal-time rows.
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

/// Canonical `hash -> prev_hash` ancestry over the window (display hex), after
/// asserting backbone coverage. Feeds compact's hidden-edge `hidden_predecessor`
/// walk so collapsed runs resolve to a real in-window predecessor.
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

/// Pick the default tip height by three strategies in order: a contiguous complete
/// run with no higher complete block, then the sparse clean-island scan, then the
/// contiguous run without the no-higher guard. The order is the contract: prefer a
/// fully-synced tip, degrade to a clean island, and only then a contiguous run
/// that may sit below a higher partial.
pub(super) async fn load_default_tree_tip_height(client: &Client) -> Result<Option<i32>> {
    if let Some(tip_height) = load_contiguous_complete_tree_tip_height(client, true).await? {
        return Ok(Some(tip_height));
    }
    match load_sparse_complete_tree_tip_height(client).await? {
        Some(tip_height) => Ok(Some(tip_height)),
        None => load_contiguous_complete_tree_tip_height(client, false).await,
    }
}

/// Contiguous tip strategy: the sync-state `contiguous_complete_height` is a valid
/// tip iff the DEFAULT_TREE_WINDOW_HEIGHTS-wide window below it is a gapless run of
/// exactly-one complete canonical block per height with consistent prev-links.
/// `require_no_higher_complete` adds the guard that no complete canonical sits
/// above it (the strict first pass; relaxed in the final fallback).
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

/// Sparse tip strategy: scan back DEFAULT_TREE_SPARSE_SCAN_HEIGHTS over
/// single-block complete canonical heights, segment them into prev-link-contiguous
/// islands, and return the highest height that closes a DEFAULT_TREE_WINDOW_HEIGHTS
/// run within one island (or a short near-genesis island). Used when the
/// contiguous strategy finds no clean run at the synced tip.
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

/// Assert the canonical backbone covers the window's height span before a tree
/// load (delegates to `shared::ensure_backbone_window_coverage`). An unbounded
/// window (anchor/empty) has no spine to verify, so it is a no-op.
pub(super) async fn ensure_tree_backbone_coverage(
    client: &Client,
    window: ResolvedTreeWindow,
) -> Result<(), ProjectionError> {
    let Some((from_height, to_height)) = window.bounds() else {
        return Ok(());
    };
    ensure_backbone_window_coverage(client, from_height, to_height).await
}

/// Load the window's blocks: the height-spine rows in `[from,to]`, plus (when
/// `include_unheighted` and `unknown` is selected) null-height unknown blocks in
/// the date window. The `classification` orphan-class filter applies ONLY to the
/// unheighted-unknown branch, so the public unheighted view surfaces the same
/// orphan population as the navigator; the height-spine branch is unfiltered.
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
    // The orphan-class filter (default strict+weak) applies ONLY to the
    // date-window unheighted-unknown branch, so the public unheighted tree view
    // surfaces the same orphan population as the navigator. The height-window
    // branch is unaffected.
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

/// All block hashes for unheighted (`btc_height IS NULL`) unknown parents in the
/// time window, REGARDLESS of orphan class. Seeds the direct-projection de-dup so
/// a block-backed unknown that `classification` filtered out of the tree's block
/// rows is never re-projected from its active events as a class-less (pending)
/// direct node.
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
