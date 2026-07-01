//! Tree-navigation readiness for the stale index projections.
//!
//! Given a stale block or branch span, decide whether a renderable `/tree` window
//! exists for it (backbone synced, node budget within the tree contract) and emit
//! either the `TreeNavigation` deep-link or the machine `NavigationError`. Shared
//! by stale and stale-branch navigator targets.

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::Client;

use super::ProjectionError;
use super::shared::{
    BackboneWindowCoverage, load_backbone_window_coverage_for_windows, split_height_windows,
};
use super::tree::TREE_NODE_LIMIT;

/// Heights of canonical context padded on each side of a navigation span when
/// computing the renderable tree window, bounded by the complete-canonical tip.
const NAVIGATION_PADDING: i32 = 16;

/// Precomputed tree-window navigation for a stale target, embedded in the
/// unified navigator item `view`.
/// `tree_from/tree_to` is the renderable height window (padded, backbone-
/// complete, within the tree node budget); `select_hash`/`center_hash` drive the
/// stepper. Field names and `mode = "tree_window"` are the locked JSON contract.
#[derive(Debug, Clone, Serialize)]
pub struct TreeNavigation {
    pub mode: &'static str,
    pub target_height: i32,
    pub tree_from: i32,
    pub tree_to: i32,
    pub select_hash: String,
    pub center_hash: String,
}

/// Why a stale target has no renderable `navigation`, serialized as
/// `navigation_error` (null when navigation is present). `code` is the locked
/// machine value (`target_backbone_unsynced` | `target_window_too_large`);
/// `action` is the operator remedy. Built by the `*_error` constructors below.
/// Field names are the locked JSON contract.
#[derive(Debug, Clone, Serialize)]
pub struct NavigationError {
    pub code: &'static str,
    pub target_height: i32,
    pub message: &'static str,
    pub action: &'static str,
}

/// A stale navigation request: the `target_height` to center on, the
/// `span_min/span_max` height range it must show, and `required_nodes` (extra
/// budget beyond the height range: 1 for a single stale, the branch depth for a
/// branch). Consumed by `navigation_for_span` and the coverage-window planner.
#[derive(Debug, Clone, Copy)]
pub(super) struct NavigationSpan {
    pub(super) target_height: i32,
    pub(super) span_min: i32,
    pub(super) span_max: i32,
    pub(super) required_nodes: usize,
}

/// Per-request navigation inputs loaded once: backbone `coverage` over the
/// merged windows plus the stale-node `budget`. Reused across every span so the
/// readiness queries run once, not per stale.
#[derive(Debug, Clone)]
pub(super) struct NavigationReadiness {
    coverage: BackboneWindowCoverage,
    budget: NavigationWindowBudget,
}

/// Sorted stale heights in the loaded windows, used to count a window's visible
/// nodes (canonical heights + the stale rows inside it) against
/// `TREE_NODE_LIMIT` so navigation never proposes an over-budget tree window.
#[derive(Debug, Clone)]
pub(super) struct NavigationWindowBudget {
    stale_heights: Vec<i32>,
}

/// Decide a stale target's navigation: returns `Some(TreeNavigation)` when the
/// backbone is complete and a tree window fits the node budget, else
/// `Some(NavigationError)` with the first failing reason (unsynced backbone or
/// over-budget window). Exactly one of the pair is `Some`. The check order is
/// the locked error-code precedence surfaced in the wire `navigation_error`.
pub(super) fn navigation_for_span(
    span: NavigationSpan,
    select_hash: String,
    center_hash: String,
    max_complete_height: Option<i32>,
    readiness: Option<&NavigationReadiness>,
) -> (Option<TreeNavigation>, Option<NavigationError>) {
    let Some(max_complete_height) = max_complete_height else {
        return (
            None,
            Some(target_backbone_unsynced_error(span.target_height)),
        );
    };
    if span.target_height > max_complete_height || span.span_max > max_complete_height {
        return (
            None,
            Some(target_backbone_unsynced_error(span.target_height)),
        );
    }
    if navigation_span_exceeds_tree_contract(span.span_min, span.span_max, span.required_nodes) {
        return (
            None,
            Some(target_window_too_large_error(span.target_height)),
        );
    }
    let Some(readiness) = readiness else {
        return (
            None,
            Some(target_backbone_unsynced_error(span.target_height)),
        );
    };
    if !readiness
        .budget
        .window_fits_tree_contract(span.span_min, span.span_max)
    {
        return (
            None,
            Some(target_window_too_large_error(span.target_height)),
        );
    }
    let Some((tree_from, tree_to)) =
        renderable_navigation_window(span.span_min, span.span_max, max_complete_height, readiness)
    else {
        return (
            None,
            Some(target_backbone_unsynced_error(span.target_height)),
        );
    };
    (
        Some(TreeNavigation {
            mode: "tree_window",
            target_height: span.target_height,
            tree_from,
            tree_to,
            select_hash,
            center_hash,
        }),
        None,
    )
}

/// Find the widest renderable `(tree_from, tree_to)` window around a span: pads
/// by `NAVIGATION_PADDING`, then picks the first window that is both
/// backbone-complete and within the visible-node budget. `None` if no padded
/// window qualifies (caller maps that to a backbone-unsynced error).
fn renderable_navigation_window(
    span_min: i32,
    span_max: i32,
    max_complete_height: i32,
    readiness: &NavigationReadiness,
) -> Option<(i32, i32)> {
    let min_from = span_min.saturating_sub(NAVIGATION_PADDING).max(0);
    let max_to = span_max
        .saturating_add(NAVIGATION_PADDING)
        .min(max_complete_height);
    for tree_from in min_from..=span_min {
        for tree_to in (span_max..=max_to).rev() {
            if readiness
                .budget
                .window_fits_tree_contract(tree_from, tree_to)
                && readiness.coverage.window_is_complete(tree_from, tree_to)
            {
                return Some((tree_from, tree_to));
            }
        }
    }
    None
}

/// Load navigation readiness once per request: merge all spans' coverage
/// windows, then load backbone coverage and the stale-node budget over them.
/// `None` (skip navigation) when the backbone is unsynced or no span needs a
/// window.
pub(super) async fn load_navigation_readiness(
    client: &Client,
    max_complete_height: Option<i32>,
    spans: &[NavigationSpan],
) -> Result<Option<NavigationReadiness>, ProjectionError> {
    let Some(max_complete_height) = max_complete_height else {
        return Ok(None);
    };
    let windows = navigation_coverage_windows(max_complete_height, spans);
    if windows.is_empty() {
        return Ok(None);
    }
    let coverage = load_backbone_window_coverage_for_windows(client, &windows).await?;
    let budget = load_navigation_budget(client, &windows).await?;
    Ok(Some(NavigationReadiness { coverage, budget }))
}

/// Load the sorted stale heights inside the requested windows (lateral range
/// lookup, ordered) so `NavigationWindowBudget` can count stales per window by
/// binary search.
async fn load_navigation_budget(
    client: &Client,
    windows: &[(i32, i32)],
) -> Result<NavigationWindowBudget> {
    let (from_heights, to_heights) = split_height_windows(windows);
    let rows = client
        .query(
            "WITH requested AS ( \
                 SELECT * FROM unnest($1::int[], $2::int[]) \
                    AS w(from_height, to_height) \
             ) \
             SELECT stale.btc_height \
             FROM requested w \
             JOIN LATERAL ( \
                 SELECT btc_height \
                 FROM block \
                 WHERE kind = 'stale' \
                   AND btc_height BETWEEN w.from_height AND w.to_height \
             ) stale ON TRUE \
             ORDER BY stale.btc_height",
            &[&from_heights, &to_heights],
        )
        .await
        .context("load navigation stale-node budget")?;
    Ok(NavigationWindowBudget {
        stale_heights: rows.into_iter().map(|row| row.get(0)).collect(),
    })
}

/// Build the padded coverage windows the navigation queries must load: one per
/// in-range span that fits the tree contract, then `merge_navigation_windows`
/// folds overlapping/adjacent windows so the readiness queries scan each height
/// at most once.
fn navigation_coverage_windows(
    max_complete_height: i32,
    spans: &[NavigationSpan],
) -> Vec<(i32, i32)> {
    let mut windows = Vec::<(i32, i32)>::new();
    for span in spans.iter().filter(|span| {
        span.target_height <= max_complete_height && span.span_max <= max_complete_height
    }) {
        if navigation_span_exceeds_tree_contract(span.span_min, span.span_max, span.required_nodes)
        {
            continue;
        }
        let from_height = span.span_min.saturating_sub(NAVIGATION_PADDING).max(0);
        let to_height = span
            .span_max
            .saturating_add(NAVIGATION_PADDING)
            .min(max_complete_height);
        windows.push((from_height, to_height));
    }
    merge_navigation_windows(windows)
}

/// Fold sorted height windows, merging any that overlap or are adjacent
/// (gap <= 1), so the coverage/budget queries never rescan a shared height.
fn merge_navigation_windows(mut windows: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
    windows.sort_unstable();
    let mut merged = Vec::<(i32, i32)>::new();
    for (from_height, to_height) in windows {
        let Some(last) = merged.last_mut() else {
            merged.push((from_height, to_height));
            continue;
        };
        if from_height <= last.1.saturating_add(1) {
            last.1 = last.1.max(to_height);
        } else {
            merged.push((from_height, to_height));
        }
    }
    merged
}

/// True when a span cannot fit one tree window at all: its height count plus
/// `required_nodes` exceeds `TREE_NODE_LIMIT`. The unconditional overflow check
/// that yields the `target_window_too_large` navigation error.
fn navigation_span_exceeds_tree_contract(
    span_min: i32,
    span_max: i32,
    required_nodes: usize,
) -> bool {
    height_count(span_min, span_max).saturating_add(required_nodes as u64) > TREE_NODE_LIMIT as u64
}

/// Inclusive count of heights in `[from, to]`; 0 when the range is inverted.
fn height_count(from_height: i32, to_height: i32) -> u64 {
    if to_height < from_height {
        return 0;
    }
    (to_height as i64 - from_height as i64 + 1) as u64
}

impl NavigationWindowBudget {
    /// Does the window's visible-node count (canonical heights + the stale rows in
    /// it) fit `TREE_NODE_LIMIT`? Stales are extra nodes the tree must render.
    fn window_fits_tree_contract(&self, from_height: i32, to_height: i32) -> bool {
        self.visible_node_budget(from_height, to_height) <= TREE_NODE_LIMIT as u64
    }

    /// Visible tree nodes in a window: the inclusive height count plus the stale
    /// rows inside it (each renders as an extra node).
    fn visible_node_budget(&self, from_height: i32, to_height: i32) -> u64 {
        height_count(from_height, to_height)
            .saturating_add(self.stale_count(from_height, to_height) as u64)
    }

    /// Number of stale rows with height in `[from, to]`, by binary search over the
    /// sorted `stale_heights` (relies on the loader's ORDER BY).
    fn stale_count(&self, from_height: i32, to_height: i32) -> usize {
        if to_height < from_height {
            return 0;
        }
        let start = self
            .stale_heights
            .partition_point(|height| *height < from_height);
        let end = self
            .stale_heights
            .partition_point(|height| *height <= to_height);
        end.saturating_sub(start)
    }
}

/// Build the `target_backbone_unsynced` navigation error (backbone not synced to
/// the target). Its `code`/`message`/`action` strings are the locked wire
/// contract.
fn target_backbone_unsynced_error(target_height: i32) -> NavigationError {
    NavigationError {
        code: "target_backbone_unsynced",
        target_height,
        message: "Bitcoin Core backbone is not synced for this navigation target",
        action: "run sync-bitcoin-core",
    }
}

/// Build the `target_window_too_large` navigation error (span exceeds one tree
/// window). Its `code`/`message`/`action` strings are the locked wire contract.
fn target_window_too_large_error(target_height: i32) -> NavigationError {
    NavigationError {
        code: "target_window_too_large",
        target_height,
        message: "Navigation target is too large to render as one tree window",
        action: "open a narrower stale block target",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_coverage_windows_keep_sparse_targets_separate() {
        let windows = navigation_coverage_windows(
            950_600,
            &[
                NavigationSpan {
                    target_height: 149_589,
                    span_min: 149_589,
                    span_max: 149_589,
                    required_nodes: 1,
                },
                NavigationSpan {
                    target_height: 950_517,
                    span_min: 950_517,
                    span_max: 950_517,
                    required_nodes: 1,
                },
            ],
        );

        assert_eq!(windows, vec![(149_573, 149_605), (950_501, 950_533)]);
    }

    #[test]
    fn navigation_coverage_windows_merge_adjacent_targets() {
        let windows = navigation_coverage_windows(
            200,
            &[
                NavigationSpan {
                    target_height: 100,
                    span_min: 100,
                    span_max: 100,
                    required_nodes: 1,
                },
                NavigationSpan {
                    target_height: 133,
                    span_min: 133,
                    span_max: 133,
                    required_nodes: 1,
                },
            ],
        );

        assert_eq!(windows, vec![(84, 149)]);
    }

    #[test]
    fn navigation_budget_counts_stale_nodes_as_extra_visible_nodes() {
        let budget = NavigationWindowBudget {
            stale_heights: vec![90, 100, 100, 110],
        };

        assert_eq!(budget.stale_count(100, 100), 2);
        assert_eq!(budget.visible_node_budget(100, 100), 3);
        assert_eq!(budget.visible_node_budget(91, 109), 21);
        assert_eq!(budget.stale_count(111, 110), 0);
    }

    #[test]
    fn navigation_contract_rejects_explicit_tree_overflows() {
        assert!(!navigation_span_exceeds_tree_contract(100, 598, 1));
        assert!(navigation_span_exceeds_tree_contract(100, 599, 1));
        assert!(navigation_span_exceeds_tree_contract(
            100,
            101,
            TREE_NODE_LIMIT + 1
        ));
        assert!(!navigation_span_exceeds_tree_contract(100, 101, 2));
    }
}
