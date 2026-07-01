//! Compact context tree projection for exact height/time lookups.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use tokio_postgres::Client;

use super::build::{
    attach_competitions, ensure_emitted_stale_competitions, load_competitions_for_hashes,
    stale_attach_parent_hashes, stale_branch_members_to_include, stale_member_competitor_hashes,
};
use super::reduction::NODE_LIMIT;
use super::window::{
    ResolvedTreeWindow, load_active_events_for_tree, load_complete_canonical_height_at_time,
    load_verified_canonical_ancestry,
};
use super::{
    BLOCK_ROW_SELECT, NodeProtection, TreePayload, base_tree_legend, reduce_and_assemble_tree,
    select_tree_candidates, tree_window_from_resolved,
};
use crate::error::ApiError;
use crate::normalize::ParentKind;
use crate::query::TreeQuery;

use super::super::ProjectionError;
use super::super::materialize::{ParentProjection, project_blocks};
use super::super::shared::{
    BlockRow, EventRow, SourceRecord, display_hash, load_active_proofs_for_hashes,
    load_max_complete_canonical_height, load_sources, rows_to_blocks,
};

/// Descending +/- height radii tried by the compact retry loop, widest first.
/// `compact_candidate_windows` walks these so a RangeTooLarge/BackboneUnsynced
/// failure at a wide radius re-attempts at the next narrower one, guaranteeing a
/// usable window down to +/- 16 blocks before giving up.
const COMPACT_RADIUS_SCHEDULE: [i32; 7] = [1_008, 512, 256, 128, 64, 32, 16];
/// Cap on non-collapsible (protected or evidence) nodes in a compact context
/// before the window is rejected as too dense. Checked pre-reduction by
/// `ensure_compact_candidate_limit`; distinct from `NODE_LIMIT` (the post-reduction
/// visible-node cap), though both happen to be 500.
const COMPACT_EVENT_NODE_LIMIT: usize = 500;
/// The resolved focus block of a compact lookup.
struct CompactTarget {
    height: i32,
}

/// Per-attempt compact working set: the target, the resolved [from,to] height
/// window, and the verified canonical ancestry map used to account hidden
/// (omitted-interior) edges without materializing every canonical block.
struct CompactContext<'a> {
    target: &'a CompactTarget,
    from_height: i32,
    to_height: i32,
    window: ResolvedTreeWindow,
    ancestry_by_hash: HashMap<String, String>,
}

/// `/api/v1/tree?context=compact` entry: render a tight context window around an
/// exact `at_height`/`at_time` target, retrying progressively narrower radii (and
/// RangeTooLarge/BackboneUnsynced/BackboneConflict until a window fits. Payload
/// is `TreePayload` (pinned by fixtures/api/tree.json).
pub(super) async fn compact_tree(
    client: &Client,
    query: &TreeQuery,
) -> Result<TreePayload, ProjectionError> {
    let Some(target) = resolve_compact_target(client, query).await? else {
        return Ok(empty_compact_time_lookup());
    };
    let max_height = load_max_complete_canonical_height(client)
        .await?
        .unwrap_or(target.height);
    let windows = compact_candidate_windows(target.height, max_height);
    for (index, (from_height, to_height)) in windows.iter().copied().enumerate() {
        let final_attempt = index + 1 == windows.len();
        match compact_tree_for_window(client, query, &target, from_height, to_height).await {
            Ok(payload) => return Ok(payload),
            Err(err) if !final_attempt && is_compact_retry_error(&err) => continue,
            Err(err) => return Err(err),
        }
    }
    unreachable!("compact candidate windows is never empty")
}

/// Materialize the radius schedule into clamped [from,to] height pairs around the
/// target, dropping consecutive duplicates produced when saturation at chain
/// bounds collapses two radii to the same window.
fn compact_candidate_windows(target_height: i32, max_height: i32) -> Vec<(i32, i32)> {
    let mut windows = Vec::new();
    for radius in COMPACT_RADIUS_SCHEDULE {
        let from_height = target_height.saturating_sub(radius).max(0);
        let to_height = target_height.saturating_add(radius).min(max_height);
        let duplicate =
            matches!(windows.last(), Some(previous) if *previous == (from_height, to_height));
        if !duplicate {
            windows.push((from_height, to_height));
        }
    }
    windows
}

/// True for the error classes a narrower compact window may resolve
/// (RangeTooLarge, BackboneUnsynced, BackboneConflict). Any other error aborts
/// the retry loop immediately rather than masking a real failure.
fn is_compact_retry_error(err: &ProjectionError) -> bool {
    matches!(
        err,
        ProjectionError::Api(
            ApiError::RangeTooLarge { .. }
                | ApiError::BackboneUnsynced { .. }
                | ApiError::BackboneConflict { .. }
        )
    )
}

/// Render one compact attempt at a fixed [from,to] window: load verified
/// canonical ancestry, build the canonical-context candidate set, enforce the
/// density caps, reduce/assemble, then check the post-reduction node cap.
async fn compact_tree_for_window(
    client: &Client,
    query: &TreeQuery,
    target: &CompactTarget,
    from_height: i32,
    to_height: i32,
) -> Result<TreePayload, ProjectionError> {
    let window = ResolvedTreeWindow::explicit(from_height, to_height);
    let ancestry_by_hash = load_verified_canonical_ancestry(client, window).await?;
    let context = CompactContext {
        target,
        from_height,
        to_height,
        window,
        ancestry_by_hash,
    };
    let candidates = load_compact_candidates(client, query, &context).await?;
    ensure_compact_candidate_limit(&candidates)?;
    ensure_emitted_stale_competitions(&candidates)?;

    let payload = reduce_and_assemble_tree(candidates, context.window, context.ancestry_by_hash)?;
    ensure_compact_payload_limit(&payload)?;
    Ok(payload)
}

/// Assemble the protected candidate set for a compact window: seed blocks at the
/// anchors plus in-window stales/event-bearing canonicals, backfill competition
/// context, project, then relax non-structural canonical backbone rows so dense
/// interiors stay collapsible.
async fn load_compact_candidates(
    client: &Client,
    query: &TreeQuery,
    context: &CompactContext<'_>,
) -> Result<Vec<ParentProjection>, ProjectionError> {
    let mut blocks = load_compact_seed_blocks(
        client,
        context.target.height,
        context.from_height,
        context.to_height,
    )
    .await?;
    append_competition_context_blocks(client, &mut blocks, context.from_height, context.to_height)
        .await?;

    let block_hashes = blocks
        .iter()
        .map(|block| block.hash.clone())
        .collect::<Vec<_>>();
    let all_events =
        load_active_events_for_tree(client, &query.source_filter, query, &block_hashes).await?;
    let sources = load_sources(client).await?;
    let mut projections =
        project_compact_blocks(client, query, &blocks, &all_events, &sources).await?;

    let competitions = load_competitions_for_hashes(client, &block_hashes).await?;
    attach_competitions(&mut projections, &competitions);
    let stale_branch_members = stale_branch_members_to_include(&projections, query);
    let protected_stale_attach_parents =
        stale_attach_parent_hashes(&projections, &stale_branch_members);
    let protected_competitors = stale_member_competitor_hashes(&projections, &stale_branch_members);
    let protected_context = compact_context_hashes(
        &blocks,
        context.target.height,
        context.from_height,
        context.to_height,
    )?;
    let direct_projection_hashes = block_hashes.iter().cloned().collect::<HashSet<_>>();
    let protection = NodeProtection {
        stale_branch_members,
        protected_stale_attach_parents,
        protected_competitors,
        protected_context,
    };
    let (mut candidates, _) = select_tree_candidates(
        query,
        projections,
        &all_events,
        &direct_projection_hashes,
        &protection,
    )?;
    relax_compact_canonical_backbone(
        &mut candidates,
        &protection.protected_stale_attach_parents,
        &protection.protected_competitors,
        &protection.protected_context,
    );
    Ok(candidates)
}

/// Demote canonical candidates that are not structurally required (not an attach
/// parent, competitor, or context anchor) back to unprotected/non-evidence so the
/// reducer can collapse long canonical interiors into hidden edges. Compact-only:
/// the wide-context loader protects far more canonical rows than the default view.
fn relax_compact_canonical_backbone(
    candidates: &mut [ParentProjection],
    protected_stale_attach_parents: &HashSet<String>,
    protected_competitors: &HashSet<String>,
    protected_context: &HashSet<String>,
) {
    for candidate in candidates
        .iter_mut()
        .filter(|candidate| candidate.kind == ParentKind::Canonical)
    {
        let structural = protected_stale_attach_parents.contains(&candidate.hash)
            || protected_competitors.contains(&candidate.hash)
            || protected_context.contains(&candidate.hash);
        if !structural {
            candidate.protected = false;
            candidate.evidence = false;
        }
    }
}

/// Backfill the canonical winners and stale attach-parents referenced by any
/// loaded block's competition/prev_hash but not yet present, so every emitted
/// stale has its competitor and attach point in the window.
async fn append_competition_context_blocks(
    client: &Client,
    blocks: &mut Vec<BlockRow>,
    from_height: i32,
    to_height: i32,
) -> Result<(), ProjectionError> {
    let block_hashes = blocks
        .iter()
        .map(|block| block.hash.clone())
        .collect::<Vec<_>>();
    let competitions = load_competitions_for_hashes(client, &block_hashes).await?;
    let mut protected_hashes = competitions
        .iter()
        .flat_map(|competition| {
            [
                competition.canonical_hash.clone(),
                competition.stale_hash.clone(),
            ]
        })
        .collect::<Vec<_>>();
    protected_hashes.extend(
        blocks
            .iter()
            .filter(|block| block.kind == ParentKind::Stale)
            .map(|block| block.prev_hash.clone()),
    );
    append_missing_blocks(client, blocks, &protected_hashes, from_height, to_height).await?;
    Ok(())
}

/// Load proofs for the compact block set and run the shared `project_blocks`
/// pipeline. Small compact-side adapter onto the neutral materialize helper;
/// carries no compact-specific logic.
async fn project_compact_blocks(
    client: &Client,
    query: &TreeQuery,
    blocks: &[BlockRow],
    all_events: &[EventRow],
    sources: &HashMap<String, SourceRecord>,
) -> Result<Vec<ParentProjection>, ProjectionError> {
    let block_hashes = blocks
        .iter()
        .map(|block| block.hash.clone())
        .collect::<Vec<_>>();
    let proofs = load_active_proofs_for_hashes(client, &query.source_filter, &block_hashes).await?;
    Ok(project_blocks(
        blocks,
        all_events,
        &proofs,
        sources,
        &query.source_filter,
    )?)
}

/// Reject a compact window whose protected+evidence node count exceeds
/// `COMPACT_EVENT_NODE_LIMIT` before reduction, so an over-dense context returns
/// a retryable `range_too_large("compact_event_nodes")` instead of a giant tree.
fn ensure_compact_candidate_limit(candidates: &[ParentProjection]) -> Result<(), ProjectionError> {
    let protected_count = candidates
        .iter()
        .filter(|candidate| candidate.protected || candidate.evidence)
        .count();
    if protected_count > COMPACT_EVENT_NODE_LIMIT {
        return Err(ProjectionError::Api(ApiError::range_too_large(
            "compact_event_nodes",
            COMPACT_EVENT_NODE_LIMIT as u64,
            protected_count as u64,
            "compact context contains too many non-collapsible event or branch nodes; use a narrower target or navigator",
        )));
    }
    Ok(())
}

/// Final post-reduction cap: reject if visible nodes exceed `NODE_LIMIT` with a
/// retryable `range_too_large("node_count")`.
fn ensure_compact_payload_limit(payload: &TreePayload) -> Result<(), ProjectionError> {
    if payload.nodes.len() > NODE_LIMIT {
        return Err(ProjectionError::Api(ApiError::range_too_large(
            "node_count",
            NODE_LIMIT as u64,
            payload.nodes.len() as u64,
            "compact context returns too many visible nodes; use a narrower target or navigator",
        )));
    }
    Ok(())
}

/// Resolve the compact focus block from `at_height` or `at_time` (latter via the
/// complete-canonical height-at-time lookup), verifying canonical ancestry at the
/// exact height. `None` => time lookup found no complete canonical at/before the
/// time (renders the empty payload).
async fn resolve_compact_target(
    client: &Client,
    query: &TreeQuery,
) -> Result<Option<CompactTarget>, ProjectionError> {
    let height = if let Some(height) = query.at_height {
        height
    } else if let Some(at_time) = query.at_time {
        let Some(height) = load_complete_canonical_height_at_time(client, at_time).await? else {
            return Ok(None);
        };
        height
    } else {
        return Err(ProjectionError::Api(ApiError::invalid_query(
            "context=compact requires at_height or at_time",
            serde_json::json!({ "context": query.context.as_str() }),
        )));
    };
    let exact_window = ResolvedTreeWindow::explicit(height, height);
    load_verified_canonical_ancestry(client, exact_window).await?;
    Ok(Some(CompactTarget { height }))
}

/// The empty `TreePayload` returned when a `context=compact&at_time` lookup finds
/// no complete canonical block at/before the time. Sets `empty_reason =
/// no_complete_canonical_at_or_before_time`; legend kinds/edge_kinds match the
/// non-empty payload so the wire format is stable (fixtures/api/tree.json).
fn empty_compact_time_lookup() -> TreePayload {
    let window = ResolvedTreeWindow::empty_lookup("no_complete_canonical_at_or_before_time");
    TreePayload {
        window: tree_window_from_resolved(window, 0),
        nodes: Vec::new(),
        edges: Vec::new(),
        branches: Vec::new(),
        legend: base_tree_legend(),
    }
}

/// Seed SELECT for a compact window: canonical blocks at the three anchor heights
/// (from/target/to), every in-window stale, and any in-window canonical that is
/// an active merge-mining parent. The sparse loader, it does NOT materialize
/// every canonical interior (those become hidden edges via ancestry).
async fn load_compact_seed_blocks(
    client: &Client,
    target_height: i32,
    from_height: i32,
    to_height: i32,
) -> Result<Vec<BlockRow>> {
    let anchor_heights = vec![from_height, target_height, to_height];
    let query = format!(
        "{BLOCK_ROW_SELECT} \
         WHERE (b.kind = 'canonical' AND b.btc_height = ANY($1::int[])) \
            OR (b.kind = 'stale' AND b.btc_height BETWEEN $2 AND $3) \
            OR ( \
                b.kind = 'canonical' \
                AND b.btc_height BETWEEN $2 AND $3 \
                AND EXISTS ( \
                    SELECT 1 FROM merge_mining_event e \
                    WHERE e.revoked_at IS NULL \
                      AND e.btc_parent_kind = 'canonical' \
                      AND e.btc_parent_header_hash = b.btc_header_hash \
                ) \
            )"
    );
    let rows = client
        .query(&query, &[&anchor_heights, &from_height, &to_height])
        .await
        .context("load compact seed blocks")?;
    rows_to_blocks(rows)
}

/// Fetch any of `hashes` not already loaded, bounded to [from,to], and extend
/// `blocks`. The building block for pulling protected context (competitors and
/// attach parents) into the sparse seed set.
async fn append_missing_blocks(
    client: &Client,
    blocks: &mut Vec<BlockRow>,
    hashes: &[Vec<u8>],
    from_height: i32,
    to_height: i32,
) -> Result<(), ProjectionError> {
    let loaded = blocks
        .iter()
        .map(|block| block.hash.clone())
        .collect::<HashSet<_>>();
    let missing = hashes
        .iter()
        .filter(|hash| !loaded.contains(*hash))
        .cloned()
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    let query = format!(
        "{BLOCK_ROW_SELECT} \
         WHERE b.btc_header_hash = ANY($1::bytea[]) \
           AND b.btc_height BETWEEN $2 AND $3"
    );
    let rows = client
        .query(&query, &[&missing, &from_height, &to_height])
        .await
        .context("load compact protected blocks")?;
    blocks.extend(rows_to_blocks(rows)?);
    Ok(())
}

/// Display-hash set of canonical anchors (target/from/to), marked
/// protected_context so the window's three boundary columns always render even
/// under kind/source filters.
fn compact_context_hashes(
    blocks: &[BlockRow],
    target_height: i32,
    from_height: i32,
    to_height: i32,
) -> Result<HashSet<String>, ProjectionError> {
    let mut protected = HashSet::new();
    for block in blocks.iter().filter(|block| {
        block.kind == ParentKind::Canonical
            && matches!(block.height, Some(height) if height == target_height || height == from_height || height == to_height)
    }) {
            protected.insert(display_hash(&block.hash)?);
    }
    Ok(protected)
}
