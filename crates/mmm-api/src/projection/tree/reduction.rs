//! Server-side tree reduction for the first-wave tree endpoint.

use std::collections::BTreeSet;

use crate::error::ApiError;
use crate::normalize::ParentKind;

/// The first-wave hard cap on visible tree nodes. The reducer collapses
/// removable canonical context until at/under this, then errors `range_too_large`
/// if still over. Re-exported as `TREE_NODE_LIMIT` for the branch-window budget.
pub(super) const NODE_LIMIT: usize = 500;

/// The minimal per-node view the reducer needs: hash/prev/height/kind plus the
/// `protected` (never strip) and `evidence` (carries findings, never strip) flags.
/// Built from `ParentProjection` in mod.rs; decoupled so the reducer never sees
/// the full projection.
#[derive(Debug, Clone)]
pub(super) struct ReductionNode {
    pub hash: String,
    pub prev_hash: String,
    pub height: Option<i32>,
    pub kind: ParentKind,
    pub protected: bool,
    pub evidence: bool,
}

/// Reducer output: the set of node hashes that survive the canonical-context
/// stripping. mod.rs retains candidates whose hash is in `visible_hashes`.
#[derive(Debug, Clone)]
pub(super) struct Reduction {
    pub visible_hashes: BTreeSet<String>,
}

/// Strip the longest removable runs of unprotected, non-evidence canonical
/// context until the visible set fits `NODE_LIMIT`, preserving run boundaries
/// (the omitted interiors reappear as hidden edges in materialize_tree). Errors
/// `range_too_large` if protected nodes alone exceed the cap or no run is removable.
pub(super) fn reduce(nodes: &[ReductionNode]) -> Result<Reduction, ApiError> {
    let protected = nodes.iter().filter(|node| node.protected).count();
    if protected > NODE_LIMIT {
        return Err(node_count_error(protected));
    }

    let mut visible: BTreeSet<String> = nodes.iter().map(|node| node.hash.clone()).collect();
    while visible.len() > NODE_LIMIT {
        let Some(run) = largest_removable_run(nodes, &visible) else {
            return Err(node_count_error(visible.len()));
        };
        for hash in run {
            visible.remove(&hash);
        }
    }

    Ok(Reduction {
        visible_hashes: visible,
    })
}

/// Among visible height-bearing nodes (height-sorted), find the longest
/// contiguous canonical run and return its removable interior. Breaks runs at
/// non-canonical nodes and at prev_hash discontinuities so only genuine linear
/// canonical spans collapse.
fn largest_removable_run(
    nodes: &[ReductionNode],
    visible: &BTreeSet<String>,
) -> Option<Vec<String>> {
    let mut sorted = nodes
        .iter()
        .filter(|node| visible.contains(&node.hash))
        .filter(|node| node.height.is_some())
        .collect::<Vec<_>>();
    sorted.sort_by(|a, b| a.height.cmp(&b.height).then_with(|| a.hash.cmp(&b.hash)));

    let mut best: Vec<String> = Vec::new();
    let mut current: Vec<&ReductionNode> = Vec::new();
    for node in sorted {
        if node.kind != ParentKind::Canonical {
            consider_run(&current, &mut best);
            current.clear();
            continue;
        }
        if current
            .last()
            .is_some_and(|previous| node.prev_hash != previous.hash)
        {
            consider_run(&current, &mut best);
            current.clear();
        }
        current.push(node);
    }
    consider_run(&current, &mut best);

    if best.is_empty() { None } else { Some(best) }
}

/// Score a candidate canonical run: keep only its strictly-interior nodes
/// (boundaries always survive) that are removable_context, and adopt it as `best`
/// if longer, or equal-length but lower-first-hash for deterministic selection.
fn consider_run(run: &[&ReductionNode], best: &mut Vec<String>) {
    if run.len() <= 2 {
        return;
    }
    let removable = run[1..run.len() - 1]
        .iter()
        .filter(|node| removable_context(node))
        .map(|node| node.hash.clone())
        .collect::<Vec<_>>();
    if removable.is_empty() {
        return;
    }
    if removable.len() > best.len()
        || (removable.len() == best.len()
            && !best.is_empty()
            && removable.first().expect("non-empty") < best.first().expect("non-empty"))
    {
        *best = removable;
    }
}

/// A node is collapsible context iff it is canonical and neither protected nor
/// evidence-bearing. The predicate that keeps event/boundary nodes on screen
/// while letting bare canonical interiors hide.
fn removable_context(node: &ReductionNode) -> bool {
    node.kind == ParentKind::Canonical && !node.protected && !node.evidence
}

/// The `range_too_large("node_count")` error when the visible set cannot be
/// reduced to `NODE_LIMIT`. Centralizes the cap-overflow error so all reduce exit
/// paths report identically.
fn node_count_error(received: usize) -> ApiError {
    ApiError::range_too_large(
        "node_count",
        NODE_LIMIT as u64,
        received as u64,
        "requested tree window cannot be reduced to the first-wave node limit",
    )
}

/// The `range_too_large("unheighted_nodes")` error raised when a window's
/// null-height (unheighted) nodes exceed 250, which the reducer cannot collapse.
/// Kept beside the node-count reducer because it is the sibling cardinality cap.
pub(super) fn unheighted_count_error(received: usize) -> ApiError {
    ApiError::range_too_large(
        "unheighted_nodes",
        250,
        received as u64,
        "requested unheighted tree window returns too many null-height nodes",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canonical(hash: usize, height: i32, protected: bool, evidence: bool) -> ReductionNode {
        ReductionNode {
            hash: format!("{hash:064x}"),
            prev_hash: if hash == 0 {
                "genesis".to_owned()
            } else {
                format!("{:064x}", hash - 1)
            },
            height: Some(height),
            kind: ParentKind::Canonical,
            protected,
            evidence,
        }
    }

    #[test]
    fn strips_canonical_context_but_keeps_boundaries() {
        let nodes = (0..600)
            .map(|i| canonical(i, i as i32, false, false))
            .collect::<Vec<_>>();
        let reduced = reduce(&nodes).unwrap();
        assert_eq!(reduced.visible_hashes.len(), 2);
        assert!(reduced.visible_hashes.contains(&format!("{:064x}", 0)));
        assert!(reduced.visible_hashes.contains(&format!("{:064x}", 599)));
    }

    #[test]
    fn protected_nodes_over_cap_are_range_too_large() {
        let nodes = (0..501)
            .map(|i| canonical(i, i as i32, true, true))
            .collect::<Vec<_>>();
        let err = reduce(&nodes).unwrap_err();
        assert_eq!(err.code(), "range_too_large");
    }

    #[test]
    fn evidence_context_is_not_stripped() {
        let nodes = (0..501)
            .map(|i| canonical(i, i as i32, false, true))
            .collect::<Vec<_>>();
        let err = reduce(&nodes).unwrap_err();
        assert_eq!(err.code(), "range_too_large");
    }

    #[test]
    fn disconnected_canonical_context_is_not_a_removable_run() {
        let nodes = (0..501)
            .map(|i| {
                let mut node = canonical(i, i as i32, false, false);
                node.prev_hash = format!("missing-{i}");
                node
            })
            .collect::<Vec<_>>();
        let err = reduce(&nodes).unwrap_err();
        assert_eq!(err.code(), "range_too_large");
    }

    #[test]
    fn short_context_span_between_protected_boundaries_is_removed() {
        let nodes = (0..502)
            .map(|i| canonical(i, i as i32, i != 250 && i != 251, i != 250 && i != 251))
            .collect::<Vec<_>>();
        let reduced = reduce(&nodes).unwrap();
        assert_eq!(reduced.visible_hashes.len(), NODE_LIMIT);
        assert!(!reduced.visible_hashes.contains(&format!("{:064x}", 250)));
        assert!(!reduced.visible_hashes.contains(&format!("{:064x}", 251)));
        assert!(reduced.visible_hashes.contains(&format!("{:064x}", 249)));
        assert!(reduced.visible_hashes.contains(&format!("{:064x}", 252)));
    }
}
