//! Generic connected-component extraction shared by the stale-branch and
//! orphan-branch navigator indexes. Both build a prev -> children forest over
//! `(hash, prev_hash)` rows, sort siblings/roots by an ordering key then hash,
//! DFS each component, and keep components of depth >= 2. They differ only in
//! the ordering key (BTC height for stale branches, header time for orphan
//! branches) and their row/summary types, so the algorithm lives here once and
//! each module supplies a `BranchRow` impl plus a small component -> summary map.

use std::collections::{HashMap, HashSet};

/// A node in a branch index: keyed by hash, with a parent link and an ordering
/// key (BTC height for stale branches, header time for orphan branches).
pub(super) trait BranchRow: Clone {
    fn hash(&self) -> &[u8];
    fn prev_hash(&self) -> &[u8];
    fn order_key(&self) -> i64;
}

/// One extracted branch component. `key_min` / `key_max` are the ordering key's
/// range across the component's members (BTC height or header time).
pub(super) struct BranchComponent {
    pub root_hash: Vec<u8>,
    pub member_hashes: Vec<Vec<u8>>,
    pub tip_hashes: Vec<Vec<u8>>,
    pub key_min: i64,
    pub key_max: i64,
    pub depth: usize,
}

/// Group `rows` into connected components (depth >= 2), newest-key-first within
/// each component, deterministic across runs (ties broken by hash).
pub(super) fn branch_components<R: BranchRow>(rows: Vec<R>) -> Vec<BranchComponent> {
    let mut by_hash = HashMap::<Vec<u8>, R>::new();
    let mut children_by_prev = HashMap::<Vec<u8>, Vec<Vec<u8>>>::new();
    for row in rows {
        children_by_prev
            .entry(row.prev_hash().to_vec())
            .or_default()
            .push(row.hash().to_vec());
        by_hash.insert(row.hash().to_vec(), row);
    }
    for children in children_by_prev.values_mut() {
        children.sort_by(|a, b| {
            by_hash[a]
                .order_key()
                .cmp(&by_hash[b].order_key())
                .then_with(|| a.cmp(b))
        });
    }

    let mut roots = by_hash
        .values()
        .filter(|row| !by_hash.contains_key(row.prev_hash()))
        .cloned()
        .collect::<Vec<_>>();
    roots.sort_by(|a, b| {
        a.order_key()
            .cmp(&b.order_key())
            .then_with(|| a.hash().cmp(b.hash()))
    });

    let mut visited = HashSet::new();
    let mut components = Vec::new();
    for root in &roots {
        if let Some(component) =
            summarize_component(root.hash(), &by_hash, &children_by_prev, &mut visited)
        {
            components.push(component);
        }
    }

    // Cycles / rows whose parent is also in the set but that no root reached:
    // walk any still-unvisited node so a malformed forest still summarizes.
    let mut leftovers = by_hash.values().cloned().collect::<Vec<_>>();
    leftovers.sort_by(|a, b| {
        a.order_key()
            .cmp(&b.order_key())
            .then_with(|| a.hash().cmp(b.hash()))
    });
    for row in &leftovers {
        if visited.contains(row.hash()) {
            continue;
        }
        if let Some(component) =
            summarize_component(row.hash(), &by_hash, &children_by_prev, &mut visited)
        {
            components.push(component);
        }
    }

    components
        .into_iter()
        .filter(|component| component.depth >= 2)
        .collect()
}

/// DFS one component from `root_hash`, marking `visited`, collecting members,
/// and summarizing: tips are members with no in-component child, sorted
/// newest-key-first with hash tie-break; `key_min/key_max` span the component;
/// `depth` is the member count. `None` if the root resolves to no rows.
fn summarize_component<R: BranchRow>(
    root_hash: &[u8],
    by_hash: &HashMap<Vec<u8>, R>,
    children_by_prev: &HashMap<Vec<u8>, Vec<Vec<u8>>>,
    visited: &mut HashSet<Vec<u8>>,
) -> Option<BranchComponent> {
    let root = by_hash.get(root_hash)?;
    let mut stack = vec![root.hash().to_vec()];
    let mut members = Vec::<R>::new();
    while let Some(hash) = stack.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }
        if let Some(row) = by_hash.get(&hash) {
            members.push(row.clone());
            if let Some(children) = children_by_prev.get(&hash) {
                stack.extend(children.iter().rev().cloned());
            }
        }
    }
    if members.is_empty() {
        return None;
    }

    let member_hashes = members
        .iter()
        .map(|member| member.hash().to_vec())
        .collect::<HashSet<_>>();
    let mut member_hashes_sorted = member_hashes.iter().cloned().collect::<Vec<_>>();
    member_hashes_sorted.sort();
    let mut tip_rows = members
        .iter()
        .filter(|member| {
            children_by_prev
                .get(member.hash())
                .map(|children| !children.iter().any(|child| member_hashes.contains(child)))
                .unwrap_or(true)
        })
        .cloned()
        .collect::<Vec<_>>();
    tip_rows.sort_by(|a, b| {
        b.order_key()
            .cmp(&a.order_key())
            .then_with(|| a.hash().cmp(b.hash()))
    });

    let key_min = members.iter().map(|member| member.order_key()).min()?;
    let key_max = members.iter().map(|member| member.order_key()).max()?;
    Some(BranchComponent {
        root_hash: root.hash().to_vec(),
        member_hashes: member_hashes_sorted,
        tip_hashes: tip_rows
            .into_iter()
            .map(|row| row.hash().to_vec())
            .collect(),
        key_min,
        key_max,
        depth: members.len(),
    })
}
