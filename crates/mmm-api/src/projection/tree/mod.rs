//! `/api/v1/tree` entry, payload DTOs, and mode routing.

mod anchor;
mod build;
mod compact;
mod orphan_component;
mod protocol_commitments;
mod reduction;
mod relationship;
mod window;

use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

use super::ProjectionError;
use crate::normalize::ParentKind;
use crate::query::{date_from_epoch, epoch_start_of_day, kind_selected};
use build::build_branches;
pub use protocol_commitments::TreeProtocolCommitment;
use protocol_commitments::attach_protocol_commitments;
use reduction::ReductionNode;
pub use relationship::TreeRelationship;
use relationship::attach_relationship;
use window::ResolvedTreeWindow;

use anchor::anchor_tree;
use build::{
    attach_competitions, ensure_emitted_stale_competitions, load_competitions_for_hashes,
    materialize_tree, sort_nodes, stale_attach_parent_hashes, stale_branch_members_to_include,
    stale_member_competitor_hashes,
};
use window::{
    ensure_tree_backbone_coverage, load_active_events_for_tree, load_blocks_for_tree,
    load_unheighted_unknown_block_hashes, resolve_tree_window,
};

use super::materialize::{ParentProjection, project_blocks, project_direct_events};
use super::shared::{
    ChildChainEvidence, PoolObject, ProofState, SourceSummary, TreeCompetition,
    load_active_proofs_for_hashes, load_sources,
};
use crate::normalize::Classification;
use crate::query::TreeQuery;

/// Hard cap on null-height unknown nodes in one `/tree` response and anchor-strip neighbor budget.
pub(super) const UNHEIGHTED_NODE_LIMIT: usize = 250;
pub(super) const TREE_NODE_LIMIT: usize = reduction::NODE_LIMIT;

/// Default tip-window span in BTC heights (tip plus 15 heights below it).
pub(super) const DEFAULT_TREE_WINDOW_HEIGHTS: i32 = 16;

/// Max backward scan height span for the sparse clean-island tip strategy.
pub(super) const DEFAULT_TREE_SPARSE_SCAN_HEIGHTS: i32 = 2048;

pub(super) fn unheighted_in_range(
    projection: &ParentProjection,
    query: &TreeQuery,
) -> Result<bool, crate::error::ApiError> {
    let (Some(from), Some(to)) = (query.unheighted_from, query.unheighted_to) else {
        return Ok(false);
    };
    let date = date_from_epoch(projection.header_time).map_err(|_| {
        crate::error::ApiError::invalid_query(
            "parent header time could not be interpreted as UTC",
            serde_json::json!({ "hash": projection.hash }),
        )
    })?;
    Ok(date >= from && date <= to)
}

#[derive(Debug, Clone, Serialize)]
pub struct TreePayload {
    pub window: TreeWindow,
    pub nodes: Vec<TreeNode>,
    pub edges: Vec<TreeEdge>,
    pub branches: Vec<TreeBranch>,
    pub legend: TreeLegend,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeWindow {
    pub btc_height_min: Option<i32>,
    pub btc_height_max: Option<i32>,
    pub tip_height: Option<i32>,
    pub defaulted_to_tip: bool,
    pub empty_reason: Option<&'static str>,
    pub hidden_linear_block_count: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeNode {
    pub id: usize,
    pub hash: String,
    pub height: Option<i32>,
    pub kind: &'static str,
    pub btc_orphan_class: Option<String>,
    pub prev_id: Option<usize>,
    pub prev_hash: String,
    pub bitcoin_miner_pool: PoolObject,
    pub display_miner_pool: PoolObject,
    pub display_miner_basis: &'static str,
    pub source_summary: SourceSummary,
    pub child_chain_evidence: Vec<ChildChainEvidence>,
    pub branch: Option<TreeNodeBranch>,
    pub proof_state: ProofState,
    pub competition: Option<TreeCompetition>,
    /// `null` until the protocol commitment rebuild has run, else the header's declarations.
    pub protocol_commitments: Option<Vec<TreeProtocolCommitment>>,
    /// Whether the block's relationship signals name more than one pool, with those pools.
    pub relationship: TreeRelationship,
    /// Derived layout height for an anchor-mode orphan node (absent elsewhere).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement_height: Option<i32>,
    /// True when `placement_height` is an epoch approximation rather than strict BIP34.
    #[serde(skip_serializing_if = "is_false")]
    pub placement_approx: bool,
}

pub(super) fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeNodeBranch {
    pub branch_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeEdge {
    pub from_hash: String,
    pub to_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_count: Option<u64>,
    pub edge_kind: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeBranch {
    pub branch_id: String,
    pub root_hash: String,
    pub tip_hash: String,
    pub member_hashes: Vec<String>,
    pub btc_height_min: i32,
    pub btc_height_max: i32,
    pub depth: usize,
    pub canonical_competitor_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeLegend {
    pub kinds: Vec<&'static str>,
    pub edge_kinds: Vec<&'static str>,
}

const BLOCK_ROW_SELECT: &str = "\
SELECT b.btc_header_hash, b.btc_prev_header_hash, b.btc_height, b.kind, \
       b.btc_header_time, b.live_observed, b.core_attested, b.pow_validated, \
       p.id, p.slug, p.canonical_name, b.btc_orphan_class \
FROM block b \
LEFT JOIN pool p ON p.id = b.bitcoin_miner_pool_id";

fn tree_window_from_resolved(
    window: ResolvedTreeWindow,
    hidden_linear_block_count: u64,
) -> TreeWindow {
    TreeWindow {
        btc_height_min: window.from_height,
        btc_height_max: window.to_height,
        tip_height: window.tip_height,
        defaulted_to_tip: window.defaulted_to_tip,
        empty_reason: window.empty_reason,
        hidden_linear_block_count,
    }
}

fn base_tree_legend() -> TreeLegend {
    TreeLegend {
        kinds: vec!["canonical", "stale", "invalid_block", "unknown"],
        edge_kinds: vec!["canonical", "stale_entry", "stale", "hidden"],
    }
}

fn orphan_tree_legend() -> TreeLegend {
    let mut legend = base_tree_legend();
    legend.edge_kinds.extend(["orphan", "orphan_approx"]);
    legend
}

/// Dispatch `/api/v1/tree` by precedence: `unheighted_anchor`, `context=compact`, then windowed.
pub async fn tree(client: &Client, query: &TreeQuery) -> Result<TreePayload, ProjectionError> {
    let mut payload = if let Some(anchor) = query.unheighted_anchor.as_deref() {
        anchor_tree(client, query, anchor).await?
    } else if query.context.is_compact() {
        compact::compact_tree(client, query).await?
    } else {
        let window = resolve_tree_window(client, query).await?;
        tree_projection(client, query, window).await?
    };
    attach_protocol_commitments(client, &mut payload.nodes).await?;
    attach_relationship(client, &mut payload.nodes).await?;
    Ok(payload)
}

struct NodeProtection {
    stale_branch_members: HashSet<String>,
    protected_stale_attach_parents: HashSet<String>,
    protected_competitors: HashSet<String>,
    protected_context: HashSet<String>,
}

fn select_tree_candidates(
    query: &TreeQuery,
    projections: Vec<super::materialize::ParentProjection>,
    all_events: &[super::shared::EventRow],
    direct_projection_hashes: &HashSet<Vec<u8>>,
    protection: &NodeProtection,
) -> Result<(Vec<super::materialize::ParentProjection>, usize), ProjectionError> {
    let NodeProtection {
        stale_branch_members,
        protected_stale_attach_parents,
        protected_competitors,
        protected_context,
    } = protection;
    let mut candidates = Vec::new();
    let mut unheighted_count = 0usize;
    for mut projection in projections {
        let is_selected = kind_selected(&query.kinds, projection.kind);
        let enough_sources = projection.source_summary.distinct_sources >= query.min_sources;
        match (projection.height, projection.kind) {
            (Some(_), ParentKind::Canonical) => {
                projection.evidence = is_selected && enough_sources;
                projection.protected = projection.evidence
                    || protected_competitors.contains(&projection.hash)
                    || protected_stale_attach_parents.contains(&projection.hash)
                    || protected_context.contains(&projection.hash);
                candidates.push(projection);
            }
            (Some(_), ParentKind::Stale) if stale_branch_members.contains(&projection.hash) => {
                projection.evidence = is_selected && enough_sources;
                projection.protected = true;
                candidates.push(projection);
            }
            (Some(_), ParentKind::InvalidBlock) if is_selected && enough_sources => {
                projection.evidence = true;
                projection.protected = true;
                candidates.push(projection);
            }
            (None, ParentKind::Unknown)
                if query.include_unheighted
                    && is_selected
                    && enough_sources
                    && unheighted_in_range(&projection, query).map_err(ProjectionError::Api)? =>
            {
                projection.evidence = true;
                projection.protected = true;
                unheighted_count += 1;
                candidates.push(projection);
            }
            _ => {}
        }
    }

    // Direct-event unknowns have no `block` row (pending by construction); only emit them
    // when `pending` is in `query.classification`.
    if query.include_unheighted
        && query.classification.contains(&Classification::Pending)
        && kind_selected(&query.kinds, ParentKind::Unknown)
    {
        let direct = project_direct_events(all_events, direct_projection_hashes)?;
        for mut projection in direct {
            if projection.source_summary.distinct_sources >= query.min_sources
                && unheighted_in_range(&projection, query).map_err(ProjectionError::Api)?
            {
                projection.evidence = true;
                projection.protected = true;
                unheighted_count += 1;
                candidates.push(projection);
            }
        }
    }

    Ok((candidates, unheighted_count))
}

fn reduce_and_assemble_tree(
    mut candidates: Vec<super::materialize::ParentProjection>,
    window: ResolvedTreeWindow,
    extra_ancestry_by_hash: HashMap<String, String>,
) -> Result<TreePayload, ProjectionError> {
    let mut ancestry_by_hash = candidates
        .iter()
        .map(|node| (node.hash.clone(), node.prev_hash.clone()))
        .collect::<HashMap<_, _>>();
    ancestry_by_hash.extend(extra_ancestry_by_hash);
    let reduction_nodes = candidates
        .iter()
        .map(|node| ReductionNode {
            hash: node.hash.clone(),
            prev_hash: node.prev_hash.clone(),
            height: node.height,
            kind: node.kind,
            protected: node.protected,
            evidence: node.evidence,
        })
        .collect::<Vec<_>>();
    let reduction = reduction::reduce(&reduction_nodes).map_err(ProjectionError::Api)?;
    candidates.retain(|node| reduction.visible_hashes.contains(&node.hash));
    sort_nodes(&mut candidates);

    let (nodes, edges, hidden_count) = materialize_tree(candidates, &ancestry_by_hash)?;
    let branches = build_branches(&nodes);
    Ok(TreePayload {
        window: tree_window_from_resolved(window, hidden_count),
        nodes,
        edges,
        branches,
        legend: base_tree_legend(),
    })
}

async fn tree_projection(
    client: &Client,
    query: &TreeQuery,
    window: ResolvedTreeWindow,
) -> Result<TreePayload, ProjectionError> {
    ensure_tree_backbone_coverage(client, window).await?;

    let blocks = load_blocks_for_tree(client, query, window).await?;
    let block_hashes = blocks
        .iter()
        .map(|block| block.hash.clone())
        .collect::<Vec<_>>();
    let mut direct_projection_hashes = block_hashes.iter().cloned().collect::<HashSet<_>>();
    let all_events =
        load_active_events_for_tree(client, &query.source_filter, query, &block_hashes).await?;
    let proofs = load_active_proofs_for_hashes(client, &query.source_filter, &block_hashes).await?;
    let sources = load_sources(client).await?;
    let competitions = load_competitions_for_hashes(client, &block_hashes).await?;

    for competition in &competitions {
        direct_projection_hashes.insert(competition.canonical_hash.clone());
        direct_projection_hashes.insert(competition.stale_hash.clone());
    }
    // Exclude all block-backed unheighted unknowns in the window from direct projection so
    // rows filtered out by `classification` are not re-materialized from events as pending.
    if query.include_unheighted {
        let from = query.unheighted_from.map(epoch_start_of_day).unwrap_or(0);
        let to = query
            .unheighted_to
            .map(|date| epoch_start_of_day(date) + 86_399)
            .unwrap_or(0);
        direct_projection_hashes
            .extend(load_unheighted_unknown_block_hashes(client, from, to).await?);
    }

    let mut projections = project_blocks(
        &blocks,
        &all_events,
        &proofs,
        &sources,
        &query.source_filter,
    )?;
    attach_competitions(&mut projections, &competitions);
    let stale_branch_members = stale_branch_members_to_include(&projections, query);
    let protected_stale_attach_parents =
        stale_attach_parent_hashes(&projections, &stale_branch_members);
    let protected_competitors = stale_member_competitor_hashes(&projections, &stale_branch_members);

    let (candidates, unheighted_count) = select_tree_candidates(
        query,
        projections,
        &all_events,
        &direct_projection_hashes,
        &NodeProtection {
            stale_branch_members,
            protected_stale_attach_parents,
            protected_competitors,
            protected_context: HashSet::new(),
        },
    )?;

    if unheighted_count > UNHEIGHTED_NODE_LIMIT {
        return Err(ProjectionError::Api(reduction::unheighted_count_error(
            unheighted_count,
        )));
    }
    ensure_emitted_stale_competitions(&candidates)?;

    reduce_and_assemble_tree(candidates, window, HashMap::new())
}
