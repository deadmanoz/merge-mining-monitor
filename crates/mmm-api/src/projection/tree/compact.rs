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

/// Descending `+/-` height radii tried by the compact retry loop, widest first.
const COMPACT_RADIUS_SCHEDULE: [i32; 7] = [1_008, 512, 256, 128, 64, 32, 16];
/// Pre-reduction cap on protected or evidence nodes in a compact context window.
const COMPACT_EVENT_NODE_LIMIT: usize = 500;

struct CompactTarget {
    height: i32,
}

struct CompactContext<'a> {
    target: &'a CompactTarget,
    from_height: i32,
    to_height: i32,
    window: ResolvedTreeWindow,
    ancestry_by_hash: HashMap<String, String>,
}

/// Render a compact context window around `at_height`/`at_time`, retrying narrower radii on
/// `RangeTooLarge`, `BackboneUnsynced`, or `BackboneConflict`.
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

/// Demote non-structural canonical candidates so the reducer can collapse interior spans into
/// `hidden` edges.
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

/// Sparse seed loader: canonical blocks at `from/target/to`, all in-window stale or error blocks,
/// and any in-window canonical block with active merge-mining events.
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
            OR (b.kind = 'error_block' AND b.btc_height BETWEEN $2 AND $3) \
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
