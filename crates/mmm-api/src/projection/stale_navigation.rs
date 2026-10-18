//! Tree-navigation readiness for stale, stale-branch, and error-block navigator targets.

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::Client;

use super::ProjectionError;
use super::shared::{
    BackboneWindowCoverage, load_backbone_window_coverage_for_windows, split_height_windows,
};
use super::tree::TREE_NODE_LIMIT;

const NAVIGATION_PADDING: i32 = 16;

#[derive(Debug, Clone, Serialize)]
pub struct TreeNavigation {
    pub mode: &'static str,
    pub target_height: i32,
    pub tree_from: i32,
    pub tree_to: i32,
    pub select_hash: String,
    pub center_hash: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigationError {
    pub code: &'static str,
    pub target_height: i32,
    pub message: &'static str,
    pub action: &'static str,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct NavigationSpan {
    pub(super) target_height: i32,
    pub(super) span_min: i32,
    pub(super) span_max: i32,
    pub(super) required_nodes: usize,
}

#[derive(Debug, Clone)]
pub(super) struct NavigationReadiness {
    coverage: BackboneWindowCoverage,
    budget: NavigationWindowBudget,
}

/// Sorted heights of off-spine rows (stale blocks and sourced `error_block` rows) in the loaded
/// windows, counted against `TREE_NODE_LIMIT` so navigation never proposes an over-budget window.
#[derive(Debug, Clone)]
pub(super) struct NavigationWindowBudget {
    off_spine_heights: Vec<i32>,
}

/// Return `Some(TreeNavigation)` when the backbone is complete and a window fits `TREE_NODE_LIMIT`,
/// or `Some(NavigationError)` with the first failing reason (`target_backbone_unsynced` | `target_window_too_large`).
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

/// Load sorted off-spine heights inside `windows` (stale rows plus `error_block` rows with
/// `distinct_sources >= 1`, matching what the tree renders beside the canonical spine).
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
             SELECT off_spine.btc_height \
             FROM requested w \
             JOIN LATERAL ( \
                 SELECT btc_height \
                 FROM block \
                 WHERE btc_height BETWEEN w.from_height AND w.to_height \
                   AND ( \
                        kind = 'stale' \
                        OR (kind = 'error_block' AND distinct_sources >= 1) \
                   ) \
             ) off_spine ON TRUE \
             ORDER BY off_spine.btc_height",
            &[&from_heights, &to_heights],
        )
        .await
        .context("load navigation off-spine node budget")?;
    Ok(NavigationWindowBudget {
        off_spine_heights: rows.into_iter().map(|row| row.get(0)).collect(),
    })
}

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

fn navigation_span_exceeds_tree_contract(
    span_min: i32,
    span_max: i32,
    required_nodes: usize,
) -> bool {
    height_count(span_min, span_max).saturating_add(required_nodes as u64) > TREE_NODE_LIMIT as u64
}

fn height_count(from_height: i32, to_height: i32) -> u64 {
    if to_height < from_height {
        return 0;
    }
    (to_height as i64 - from_height as i64 + 1) as u64
}

impl NavigationWindowBudget {
    fn window_fits_tree_contract(&self, from_height: i32, to_height: i32) -> bool {
        self.visible_node_budget(from_height, to_height) <= TREE_NODE_LIMIT as u64
    }

    fn visible_node_budget(&self, from_height: i32, to_height: i32) -> u64 {
        height_count(from_height, to_height)
            .saturating_add(self.off_spine_count(from_height, to_height) as u64)
    }

    fn off_spine_count(&self, from_height: i32, to_height: i32) -> usize {
        if to_height < from_height {
            return 0;
        }
        let start = self
            .off_spine_heights
            .partition_point(|height| *height < from_height);
        let end = self
            .off_spine_heights
            .partition_point(|height| *height <= to_height);
        end.saturating_sub(start)
    }
}

fn target_backbone_unsynced_error(target_height: i32) -> NavigationError {
    NavigationError {
        code: "target_backbone_unsynced",
        target_height,
        message: "Bitcoin Core backbone is not synced for this navigation target",
        action: "run sync-bitcoin-core",
    }
}

fn target_window_too_large_error(target_height: i32) -> NavigationError {
    NavigationError {
        code: "target_window_too_large",
        target_height,
        message: "Navigation target is too large to render as one tree window",
        action: "open a narrower navigation target",
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
    fn navigation_budget_counts_off_spine_nodes_as_extra_visible_nodes() {
        let budget = NavigationWindowBudget {
            off_spine_heights: vec![90, 100, 100, 110],
        };

        assert_eq!(budget.off_spine_count(100, 100), 2);
        assert_eq!(budget.visible_node_budget(100, 100), 3);
        assert_eq!(budget.visible_node_budget(91, 109), 21);
        assert_eq!(budget.off_spine_count(111, 110), 0);
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
