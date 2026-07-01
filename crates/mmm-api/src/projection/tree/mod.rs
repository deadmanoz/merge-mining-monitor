//! Tree projection: the `/api/v1/tree` entry, payload DTOs, and the
//! tree-only serde helpers.

mod anchor;
mod build;
mod compact;
mod orphan_component;
mod reduction;
mod window;

use anyhow::Result;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use tokio_postgres::Client;

use super::ProjectionError;
use crate::normalize::ParentKind;
use crate::query::{date_from_epoch, epoch_start_of_day, kind_selected};
use build::build_branches;
use reduction::ReductionNode;
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

/// Hard cap on null-height (unheighted) unknown nodes in one `/tree` response, and
/// the anchor-strip neighbor budget. Over this, mod.rs returns `unheighted_count_error`
/// rather than emit an unbounded null-height set the buildless frontend cannot lay out.
pub(super) const UNHEIGHTED_NODE_LIMIT: usize = 250;
pub(super) const TREE_NODE_LIMIT: usize = reduction::NODE_LIMIT;

/// Default tip-window span in BTC heights: the tip plus the 15 heights below it.
/// Bounds `ResolvedTreeWindow::for_tip` and the contiguous/sparse tip SQL so the
/// no-param `/tree` view is a fixed-width strip ending at the canonical tip.
pub(super) const DEFAULT_TREE_WINDOW_HEIGHTS: i32 = 16;

/// How far back the sparse tip strategy scans for a clean DEFAULT_TREE_WINDOW_HEIGHTS
/// run of single-block complete canonical heights when the contiguous tip fails.
/// Caps the window-function island scan so a long gappy backbone cannot blow up
/// the default-tip query.
pub(super) const DEFAULT_TREE_SPARSE_SCAN_HEIGHTS: i32 = 2048;

/// True when a projection's parent header time falls inside the request's
/// `unheighted_from..=unheighted_to` date window (UTC date compare). Gates which
/// null-height unknowns enter the windowed `include_unheighted` view; returns
/// false when either bound is unset.
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

/// `/api/v1/tree` success payload (pinned by fixtures `tree.json` and
/// `tree-unheighted-anchor.json`). `window` echoes the resolved bounds; `nodes`
/// and `edges` are the reduced graph; `branches` groups stale/orphan forks;
/// `legend` lists the kind/edge vocabularies the frontend renders. Field names
/// are the locked JSON contract.
#[derive(Debug, Clone, Serialize)]
pub struct TreePayload {
    pub window: TreeWindow,
    pub nodes: Vec<TreeNode>,
    pub edges: Vec<TreeEdge>,
    pub branches: Vec<TreeBranch>,
    pub legend: TreeLegend,
}

/// Resolved-window echo in the tree payload (wire contract; tree fixtures).
/// `defaulted_to_tip` flags the no-param tip default; `empty_reason` carries the
/// machine string when no window resolved (`no_canonical_tip`,
/// `no_complete_canonical_at_or_before_time`); `hidden_linear_block_count` is the
/// total collapsed by the `hidden` edges.
#[derive(Debug, Clone, Serialize)]
pub struct TreeWindow {
    pub btc_height_min: Option<i32>,
    pub btc_height_max: Option<i32>,
    pub tip_height: Option<i32>,
    pub defaulted_to_tip: bool,
    pub empty_reason: Option<&'static str>,
    pub hidden_linear_block_count: u64,
}

/// One block in the tree graph (wire contract; tree fixtures). `id`/`prev_id` are
/// response-local ids assigned by `materialize_tree`, not stored columns; `hash`
/// is display (reversed) hex. `placement_height`/`placement_approx` appear only on
/// anchor-mode orphan forks (serde-skipped otherwise).
#[derive(Debug, Clone, Serialize)]
pub struct TreeNode {
    pub id: usize,
    pub hash: String,
    pub height: Option<i32>,
    pub kind: &'static str,
    /// Derived refinement of `kind='unknown'` (see `block.btc_orphan_class`):
    /// `strict_btc_orphan` / `weak_btc_orphan` / `btc_stale_excluded`, or `null`
    /// for canonical/stale nodes and for pending/never-Core-checked unknowns.
    pub btc_orphan_class: Option<String>,
    pub prev_id: Option<usize>,
    pub prev_hash: String,
    pub bitcoin_miner_pool: PoolObject,
    /// Best-available miner for the node label: equals `bitcoin_miner_pool` when
    /// `display_miner_basis` is `bitcoin_coinbase`, otherwise the chain-agnostic
    /// child-inferred pool (or the unknown sentinel). The strict
    /// `bitcoin_miner_pool` fact is never overridden.
    pub display_miner_pool: PoolObject,
    /// `bitcoin_coinbase` | `child_inferred` | `unknown` (see
    /// `DisplayMinerBasis`).
    pub display_miner_basis: &'static str,
    pub source_summary: SourceSummary,
    pub child_chain_evidence: Vec<ChildChainEvidence>,
    pub branch: Option<TreeNodeBranch>,
    pub proof_state: ProofState,
    pub competition: Option<TreeCompetition>,
    /// Anchor-mode fork placement (see `anchor_projection`). A derived layout
    /// height for an orphan node whose stored `btc_height` is NULL, so the
    /// navigator can dangle it off the canonical chain at its own height. Present
    /// ONLY on a placed orphan node (serde-skipped elsewhere); it is a layout
    /// hint, not a stored block column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement_height: Option<i32>,
    /// True when `placement_height` is an approximation (the timestamp-selected
    /// DAA-epoch height for weak/excluded/pending orphans) rather than a validated
    /// strict BIP34 height. Drives the `~` label prefix. Serde-skipped when false.
    #[serde(skip_serializing_if = "is_false")]
    pub placement_approx: bool,
}

/// serde `skip_serializing_if` predicate: a `false` bool serializes to nothing,
/// so `placement_approx` appears only on an approximately-placed orphan node.
pub(super) fn is_false(value: &bool) -> bool {
    !*value
}

/// Per-node branch membership tag (wire contract). Carries the `branch_id` that
/// ties a node to its `TreeBranch` (`stale-<height>-<root>` or `orphan-<root>`).
#[derive(Debug, Clone, Serialize)]
pub struct TreeNodeBranch {
    pub branch_id: String,
}

/// One directed `prev -> node` edge in the tree graph (wire contract; tree
/// fixtures). `edge_kind` is from the legend vocabulary (canonical/stale_entry/
/// stale/hidden, plus orphan/orphan_approx in anchor mode);
/// `hidden_count` is present only on a `hidden` collapse edge.
#[derive(Debug, Clone, Serialize)]
pub struct TreeEdge {
    pub from_hash: String,
    pub to_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden_count: Option<u64>,
    pub edge_kind: &'static str,
}

/// A stale or orphan fork as a unit (wire contract; tree fixtures), assembled by
/// `build_branches`. `btc_height_min`/`max` rank by real `btc_height` for stale
/// branches and by derived `placement_height` for orphan branches; `depth` is the
/// member count; `canonical_competitor_hashes` are the canonical winners each
/// member competed against.
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

/// Render vocabularies echoed in every tree payload (wire contract; tree
/// fixtures): the node `kinds` and the `edge_kinds` the frontend knows how to draw.
/// Anchor mode extends `edge_kinds` with `orphan`/`orphan_approx` when an orphan
/// fork is placed.
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
        kinds: vec!["canonical", "stale", "unknown", "near"],
        edge_kinds: vec!["canonical", "stale_entry", "stale", "hidden"],
    }
}

fn orphan_tree_legend() -> TreeLegend {
    let mut legend = base_tree_legend();
    legend.edge_kinds.extend(["orphan", "orphan_approx"]);
    legend
}

/// `/api/v1/tree` projection entry and mode router. Dispatches in precedence
/// order: `unheighted_anchor` -> anchor mode (orphan-fork placement, no Core
/// hydration); `context=compact` -> the compact context tree; otherwise resolve
/// the height window and run the shared windowed projection. Read-only.
pub async fn tree(client: &Client, query: &TreeQuery) -> Result<TreePayload, ProjectionError> {
    if let Some(anchor) = query.unheighted_anchor.as_deref() {
        return anchor_tree(client, query, anchor).await;
    }
    if query.context.is_compact() {
        return compact::compact_tree(client, query).await;
    }
    let window = resolve_tree_window(client, query).await?;
    tree_projection(client, query, window).await
}

/// The three hash sets that keep a node renderable regardless of the
/// kind/min-sources filters: stale-branch members, their canonical attach
/// parents, and their canonical competitors.
struct NodeProtection {
    stale_branch_members: HashSet<String>,
    protected_stale_attach_parents: HashSet<String>,
    protected_competitors: HashSet<String>,
    protected_context: HashSet<String>,
}

/// Select the window's renderable candidates: canonical spine rows (with
/// protection for stale attach parents and competitors), in-branch stales,
/// block-backed unheighted unknowns inside the date window, and - when
/// include_unheighted is on - the direct-event projections gated by the
/// pending classification contract. Returns the candidates and the
/// unheighted count for the node-limit check.
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

    if query.include_unheighted {
        // A direct-event projection has no `block` read-model row, so it carries no
        // `btc_orphan_class` (pending by construction). Gate direct unknowns on the
        // classification filter so the include_unheighted view honors the same
        // orphan-class contract as the block-row path (`load_blocks_for_tree`):
        // under the default strict+weak filter they are excluded, and they appear
        // only when the caller asks for `pending`. Direct `near` projections, which
        // are not an orphan class, are unaffected.
        let include_pending_unknowns = query.classification.contains(&Classification::Pending);
        let direct = project_direct_events(all_events, direct_projection_hashes)?;
        for mut projection in direct {
            let is_selected = kind_selected(&query.kinds, projection.kind);
            let allowed_kind = (projection.kind == ParentKind::Unknown && include_pending_unknowns)
                || (projection.kind == ParentKind::Near && query.include_near);
            if allowed_kind
                && is_selected
                && projection.source_summary.distinct_sources >= query.min_sources
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

/// Reduce the candidate set, materialize nodes/edges, and assemble the
/// payload with the window echo and legend.
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

/// Default windowed `/tree` projection over an already-resolved height window:
/// ensure backbone coverage, load blocks + active events/proofs/sources/derived
/// competitions, seed the direct-projection de-dup so a classification-
/// filtered unknown is never re-leaked as pending, select protected candidates,
/// enforce the unheighted cap and the every-stale-has-a-competition invariant,
/// then reduce and assemble.
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
    // Exclude EVERY block-backed unheighted unknown in the window from direct
    // projection, not only those that passed the orphan-class filter in
    // `load_blocks_for_tree`. Otherwise a classified (strict/weak/excluded) block
    // that a narrower `classification` (e.g. pending-only) filtered out of `blocks`
    // would be re-materialized from its active events as a class-less (pending)
    // direct projection, leaking a classified row as pending.
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
