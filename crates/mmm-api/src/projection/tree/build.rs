//! Tree payload building: competition decoration, stale-branch membership, node ordering,
//! materialization, and branch assignment.

use std::collections::{BTreeMap, HashMap, HashSet};

use anyhow::{Context, Result, bail};
use tokio_postgres::Client;

use super::super::materialize::ParentProjection;
use super::super::shared::{
    COMPETITION_FROM_SQL, CompetitionCore, TreeCompetition, competition_core_select, display_hash,
    projection_invariant_error,
};
use super::{TreeBranch, TreeEdge, TreeNode, TreeNodeBranch};
use crate::normalize::ParentKind;
use crate::query::{TreeQuery, kind_as_str, kind_selected};

pub(super) async fn load_competitions_for_hashes(
    client: &Client,
    block_hashes: &[Vec<u8>],
) -> Result<Vec<CompetitionCore>> {
    let sql = format!(
        "{select} {from} \
           AND (stale.btc_header_hash = ANY($1::bytea[]) \
                OR stale.canonical_competitor_hash = ANY($1::bytea[]))",
        select = competition_core_select(),
        from = COMPETITION_FROM_SQL,
    );
    let rows = client
        .query(&sql, &[&block_hashes])
        .await
        .context("derive competition relationships")?;
    Ok(rows.iter().map(CompetitionCore::from_row).collect())
}

pub(super) fn attach_competitions(
    projections: &mut [ParentProjection],
    competitions: &[CompetitionCore],
) {
    let hash_by_bytes = projections
        .iter()
        .map(|projection| (projection.hash_bytes.clone(), projection.hash.clone()))
        .collect::<HashMap<_, _>>();
    for projection in projections {
        if projection.kind != ParentKind::Stale {
            continue;
        }
        let Some(competition) = competitions
            .iter()
            .find(|competition| competition.stale_hash == projection.hash_bytes)
        else {
            continue;
        };
        let Ok(canonical_hash) = display_hash(&competition.canonical_hash) else {
            continue;
        };
        projection.competition = Some(TreeCompetition {
            btc_height: projection.height.unwrap_or_default(),
            stale_hash: hash_by_bytes
                .get(&competition.stale_hash)
                .cloned()
                .unwrap_or_else(|| projection.hash.clone()),
            canonical_hash,
            stale_bitcoin_miner_pool: competition.stale_bitcoin_miner_pool.clone(),
            canonical_bitcoin_miner_pool: competition.canonical_bitcoin_miner_pool.clone(),
            header_time_delta_s: competition.header_time_delta_s,
            propagation_delta_s: None,
        });
    }
}

/// Expand every selected, sufficiently-sourced stale into its full branch (root and all descendants).
pub(super) fn stale_branch_members_to_include(
    projections: &[ParentProjection],
    query: &TreeQuery,
) -> HashSet<String> {
    let stale_hashes = projections
        .iter()
        .filter(|projection| projection.height.is_some() && projection.kind == ParentKind::Stale)
        .map(|projection| projection.hash.clone())
        .collect::<HashSet<_>>();
    let prev_by_hash = projections
        .iter()
        .filter(|projection| {
            projection.height.is_some()
                && projection.kind == ParentKind::Stale
                && stale_hashes.contains(&projection.prev_hash)
        })
        .map(|projection| (projection.hash.clone(), projection.prev_hash.clone()))
        .collect::<HashMap<_, _>>();
    let mut children_by_hash = HashMap::<String, Vec<String>>::new();
    for projection in projections.iter().filter(|projection| {
        projection.height.is_some()
            && projection.kind == ParentKind::Stale
            && stale_hashes.contains(&projection.prev_hash)
    }) {
        children_by_hash
            .entry(projection.prev_hash.clone())
            .or_default()
            .push(projection.hash.clone());
    }

    let mut include = HashSet::new();
    for projection in projections
        .iter()
        .filter(|projection| projection.height.is_some() && projection.kind == ParentKind::Stale)
    {
        if !kind_selected(&query.kinds, projection.kind)
            || projection.source_summary.distinct_sources < query.min_sources
        {
            continue;
        }
        let root = stale_branch_root(&projection.hash, &prev_by_hash);
        include_stale_descendants(&root, &children_by_hash, &mut include);
    }
    include
}

fn stale_branch_root(hash: &str, prev_by_hash: &HashMap<String, String>) -> String {
    let mut root = hash.to_owned();
    let mut seen = HashSet::new();
    while seen.insert(root.clone()) {
        let Some(prev_hash) = prev_by_hash.get(&root) else {
            break;
        };
        root = prev_hash.clone();
    }
    root
}

fn include_stale_descendants(
    root: &str,
    children_by_hash: &HashMap<String, Vec<String>>,
    include: &mut HashSet<String>,
) {
    let mut stack = vec![root.to_owned()];
    let mut seen = HashSet::new();
    while let Some(hash) = stack.pop() {
        if !seen.insert(hash.clone()) {
            continue;
        }
        include.insert(hash.clone());
        if let Some(children) = children_by_hash.get(&hash) {
            stack.extend(children.iter().cloned());
        }
    }
}

/// Canonical blocks that a stale branch attaches to (protected from context stripping).
pub(super) fn stale_attach_parent_hashes(
    projections: &[ParentProjection],
    stale_branch_members: &HashSet<String>,
) -> HashSet<String> {
    let kind_by_hash = projections
        .iter()
        .map(|projection| (projection.hash.clone(), projection.kind))
        .collect::<HashMap<_, _>>();
    projections
        .iter()
        .filter(|projection| {
            stale_branch_members.contains(&projection.hash)
                && !stale_branch_members.contains(&projection.prev_hash)
                && kind_by_hash.get(&projection.prev_hash) == Some(&ParentKind::Canonical)
        })
        .map(|projection| projection.prev_hash.clone())
        .collect()
}

/// Fail closed if any emitted stale block lacks a competition relationship.
pub(super) fn ensure_emitted_stale_competitions(projections: &[ParentProjection]) -> Result<()> {
    for projection in projections
        .iter()
        .filter(|projection| projection.kind == ParentKind::Stale)
    {
        if projection.competition.is_none() {
            let details = format!(
                "emitted stale block {} is missing competition",
                projection.hash
            );
            bail!(projection_invariant_error(&details));
        }
    }
    Ok(())
}

/// Order projections height-bearing first, then by ascending height, then by hash.
pub(super) fn sort_nodes(nodes: &mut [ParentProjection]) {
    nodes.sort_by(|a, b| {
        a.height
            .is_none()
            .cmp(&b.height.is_none())
            .then_with(|| a.height.cmp(&b.height))
            .then_with(|| a.hash.cmp(&b.hash))
    });
}

pub(super) fn materialize_tree(
    projections: Vec<ParentProjection>,
    ancestry_by_hash: &HashMap<String, String>,
) -> Result<(Vec<TreeNode>, Vec<TreeEdge>, u64)> {
    let mut id_by_hash = BTreeMap::new();
    for (index, projection) in projections.iter().enumerate() {
        id_by_hash.insert(projection.hash.clone(), index + 1);
    }

    let kind_by_hash = projections
        .iter()
        .map(|projection| (projection.hash.clone(), projection.kind))
        .collect::<BTreeMap<_, _>>();

    let mut edges = Vec::new();
    let mut hidden_total = 0u64;
    let mut nodes = Vec::new();
    for projection in projections {
        let visible_prev_hash = id_by_hash
            .contains_key(&projection.prev_hash)
            .then(|| projection.prev_hash.clone());
        let (prev_id, edge) = if let Some(prev_hash) = visible_prev_hash {
            let prev_id = id_by_hash.get(&prev_hash).copied();
            let edge = prev_id
                .zip(visible_edge_kind(&prev_hash, &projection, &kind_by_hash))
                .map(|(_, edge_kind)| TreeEdge {
                    from_hash: prev_hash,
                    to_hash: projection.hash.clone(),
                    hidden_count: None,
                    edge_kind,
                });
            (prev_id, edge)
        } else if projection.kind != ParentKind::ErrorBlock
            && let Some((prev_hash, hidden_count)) =
                hidden_predecessor(&projection, &id_by_hash, ancestry_by_hash)
        {
            let prev_id = id_by_hash.get(&prev_hash).copied();
            hidden_total += hidden_count;
            (
                prev_id,
                prev_id.map(|_| TreeEdge {
                    from_hash: prev_hash,
                    to_hash: projection.hash.clone(),
                    hidden_count: Some(hidden_count),
                    edge_kind: "hidden",
                }),
            )
        } else {
            (None, None)
        };
        if let Some(edge) = edge {
            edges.push(edge);
        }

        nodes.push(TreeNode {
            id: id_by_hash[&projection.hash],
            hash: projection.hash,
            height: projection.height,
            kind: kind_as_str(projection.kind),
            btc_orphan_class: projection.btc_orphan_class,
            prev_id,
            prev_hash: projection.prev_hash,
            bitcoin_miner_pool: projection.bitcoin_miner_pool,
            display_miner_pool: projection.display_miner_pool,
            display_miner_basis: projection.display_miner_basis,
            source_summary: projection.source_summary,
            child_chain_evidence: projection.child_chain_evidence,
            branch: None,
            proof_state: projection.proof_state,
            competition: projection.competition,
            protocol_commitments: None,
            placement_height: None,
            placement_approx: false,
        });
    }

    assign_branches(&mut nodes);
    sort_edges_by_destination_id(&mut edges, |hash| id_by_hash.get(hash).copied());
    Ok((nodes, edges, hidden_total))
}

pub(super) fn sort_edges_by_destination_id(
    edges: &mut [TreeEdge],
    id_of: impl Fn(&str) -> Option<usize>,
) {
    edges.sort_by(|a, b| {
        id_of(&a.to_hash)
            .cmp(&id_of(&b.to_hash))
            .then_with(|| a.from_hash.cmp(&b.from_hash))
    });
}

pub(super) fn stale_member_competitor_hashes(
    projections: &[ParentProjection],
    stale_branch_members: &HashSet<String>,
) -> HashSet<String> {
    projections
        .iter()
        .filter(|projection| stale_branch_members.contains(&projection.hash))
        .filter_map(|projection| {
            projection
                .competition
                .as_ref()
                .map(|competition| competition.canonical_hash.clone())
        })
        .collect()
}

fn visible_edge_kind(
    prev_hash: &str,
    projection: &ParentProjection,
    kind_by_hash: &BTreeMap<String, ParentKind>,
) -> Option<&'static str> {
    let prev_kind = kind_by_hash.get(prev_hash).copied();
    if projection.kind == ParentKind::Stale {
        return Some(if prev_kind == Some(ParentKind::Stale) {
            "stale"
        } else {
            "stale_entry"
        });
    }
    if projection.kind == ParentKind::Canonical && prev_kind == Some(ParentKind::Canonical) {
        Some("canonical")
    } else {
        None
    }
}

fn hidden_predecessor(
    projection: &ParentProjection,
    id_by_hash: &BTreeMap<String, usize>,
    ancestry_by_hash: &HashMap<String, String>,
) -> Option<(String, u64)> {
    let mut cursor = projection.prev_hash.clone();
    let mut hidden_count = 0u64;
    let mut seen = HashSet::new();
    while seen.insert(cursor.clone()) {
        if id_by_hash.contains_key(&cursor) {
            return (hidden_count > 0).then_some((cursor, hidden_count));
        }
        let Some(next) = ancestry_by_hash.get(&cursor) else {
            break;
        };
        hidden_count += 1;
        cursor = next.clone();
    }
    None
}

fn assign_branches(nodes: &mut [TreeNode]) {
    let stale_hashes = nodes
        .iter()
        .filter(|node| node.kind == "stale")
        .map(|node| node.hash.clone())
        .collect::<HashSet<_>>();
    let branch_ids = nodes
        .iter()
        .filter(|node| node.kind == "stale")
        .map(|node| {
            let mut root = node;
            while stale_hashes.contains(&root.prev_hash) {
                let Some(prev) = nodes
                    .iter()
                    .find(|candidate| candidate.hash == root.prev_hash)
                else {
                    break;
                };
                root = prev;
            }
            let height = root.height.unwrap_or_default();
            (node.hash.clone(), format!("stale-{height}-{}", root.hash))
        })
        .collect::<HashMap<_, _>>();
    for node in nodes.iter_mut().filter(|node| node.kind == "stale") {
        node.branch = branch_ids.get(&node.hash).map(|branch_id| TreeNodeBranch {
            branch_id: branch_id.clone(),
        });
    }
}

/// Group stale and orphan nodes by `branch_id` into `TreeBranch` summaries, ranking by
/// `height` or `placement_height`.
pub(super) fn build_branches(nodes: &[TreeNode]) -> Vec<TreeBranch> {
    let mut by_branch: BTreeMap<String, Vec<&TreeNode>> = BTreeMap::new();
    for node in nodes
        .iter()
        .filter(|node| node.kind == "stale" || node.kind == "unknown")
    {
        if let Some(branch) = &node.branch {
            by_branch
                .entry(branch.branch_id.clone())
                .or_default()
                .push(node);
        }
    }

    let rank_height = |node: &TreeNode| node.height.or(node.placement_height);
    let mut branches = Vec::new();
    for (branch_id, mut members) in by_branch {
        members.sort_by_key(|node| rank_height(node));
        let Some(root) = members.first() else {
            continue;
        };
        let Some(tip) = members.last() else {
            continue;
        };
        let competitor_hashes = members
            .iter()
            .filter_map(|node| node.competition.as_ref())
            .map(|competition| competition.canonical_hash.clone())
            .collect::<Vec<_>>();
        branches.push(TreeBranch {
            branch_id,
            root_hash: root.hash.clone(),
            tip_hash: tip.hash.clone(),
            member_hashes: members.iter().map(|node| node.hash.clone()).collect(),
            btc_height_min: rank_height(root).unwrap_or_default(),
            btc_height_max: rank_height(tip).unwrap_or_default(),
            depth: members.len(),
            canonical_competitor_hashes: competitor_hashes,
        });
    }
    branches.sort_by(|a, b| {
        a.btc_height_min
            .cmp(&b.btc_height_min)
            .then_with(|| a.root_hash.cmp(&b.root_hash))
    });
    branches
}
