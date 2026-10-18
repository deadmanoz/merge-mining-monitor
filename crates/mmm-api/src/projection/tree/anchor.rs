//! Anchor-centered orphan-component placement for `unheighted_anchor` tree mode.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio_postgres::Client;

use super::super::ProjectionError;
use super::super::materialize::{ParentProjection, project_blocks};
use super::super::shared::{
    BlockRow, classification_filter_params, load_active_proofs_for_hashes, load_sources,
    rows_to_blocks, stored_hash_from_display,
};
use super::build::{build_branches, materialize_tree, sort_nodes};
use super::orphan_component::{
    ComponentPlacement, MemberRender, bypass_window_query, component_placement_window,
    load_member_projections, load_orphan_component, place_orphan_component,
    prepare_ordered_member_render, push_member_nodes,
};
use super::window::{ResolvedTreeWindow, load_active_events_for_tree, load_blocks_for_tree};
use super::{
    BLOCK_ROW_SELECT, TreePayload, UNHEIGHTED_NODE_LIMIT, base_tree_legend, orphan_tree_legend,
    tree_window_from_resolved,
};
use crate::error::ApiError;
use crate::normalize::{Classification, ParentKind};
use crate::query::TreeQuery;

/// Resolve the anchor orphan, derive its component placement height, and dangle it off the
/// canonical spine, degrading to the flat strip when truncated or lacking canonical context.
pub(super) async fn anchor_tree(
    client: &Client,
    query: &TreeQuery,
    anchor_display_hash: &str,
) -> Result<TreePayload, ProjectionError> {
    let orphan = resolve_anchor_orphan(client, anchor_display_hash, &query.classification).await?;
    let (members, truncated) =
        load_orphan_component(client, &orphan, &query.classification).await?;
    if truncated {
        return anchor_strip(client, query, anchor_display_hash).await;
    }
    match place_orphan_component(client, &members).await? {
        Some(placement) => {
            anchor_component_projection(client, query, anchor_display_hash, &members, &placement)
                .await
        }
        None => anchor_strip(client, query, anchor_display_hash).await,
    }
}

/// Resolve and validate the anchor orphan (`kind = 'unknown' AND pow_validated` within `classification`).
pub(super) async fn resolve_anchor_orphan(
    client: &Client,
    anchor_display_hash: &str,
    classification: &[Classification],
) -> Result<BlockRow, ProjectionError> {
    let anchor_bytes = stored_hash_from_display(anchor_display_hash)?;
    let (class_values, include_pending) = classification_filter_params(classification);
    let query = format!(
        "{BLOCK_ROW_SELECT} \
         WHERE b.kind = 'unknown' AND b.pow_validated AND b.btc_header_hash = $1 \
           AND ( \
               b.btc_orphan_class = ANY($2::text[]) \
               OR ($3::boolean AND b.btc_orphan_class IS NULL) \
           )"
    );
    let rows = client
        .query(&query, &[&anchor_bytes, &class_values, &include_pending])
        .await
        .context("resolve anchor orphan")?;
    rows_to_blocks(rows)?
        .pop()
        .ok_or_else(|| ProjectionError::Api(ApiError::not_found(anchor_display_hash)))
}

async fn load_spine_projections(
    client: &Client,
    bypass: &TreeQuery,
    window: ResolvedTreeWindow,
    no_filter: &[String],
) -> Result<
    Option<(
        Vec<ParentProjection>,
        HashMap<String, super::super::shared::SourceRecord>,
    )>,
    ProjectionError,
> {
    let spine_blocks = load_blocks_for_tree(client, bypass, window).await?;
    let spine_hashes = spine_blocks
        .iter()
        .map(|block| block.hash.clone())
        .collect::<Vec<_>>();
    let spine_events =
        load_active_events_for_tree(client, no_filter, bypass, &spine_hashes).await?;
    let spine_proofs = load_active_proofs_for_hashes(client, no_filter, &spine_hashes).await?;
    let sources = load_sources(client).await?;
    let mut spine = project_blocks(
        &spine_blocks,
        &spine_events,
        &spine_proofs,
        &sources,
        no_filter,
    )?;
    spine.retain(|projection| projection.kind == ParentKind::Canonical);
    if spine.is_empty() {
        return Ok(None);
    }
    for projection in spine.iter_mut() {
        projection.evidence = true;
        projection.protected = true;
    }
    sort_nodes(&mut spine);

    Ok(Some((spine, sources)))
}

/// Project an orphan component onto the canonical placement window, falling back to `anchor_strip`
/// when no canonical context exists in the window.
pub(super) async fn anchor_component_projection(
    client: &Client,
    query: &TreeQuery,
    anchor_display_hash: &str,
    members: &[BlockRow],
    placement: &ComponentPlacement,
) -> Result<TreePayload, ProjectionError> {
    let window = component_placement_window(placement.min_height, placement.max_height);
    let (from_height, to_height) = window
        .bounds()
        .expect("explicit placement window always has bounds");
    let bypass = bypass_window_query(query, from_height, to_height);
    let no_filter: Vec<String> = Vec::new();

    let Some((spine, sources)) =
        load_spine_projections(client, &bypass, window, &no_filter).await?
    else {
        return anchor_strip(client, query, anchor_display_hash).await;
    };

    let projection_by_hash =
        load_member_projections(client, members, &bypass, &sources, &no_filter).await?;

    let ancestry_by_hash = spine
        .iter()
        .map(|projection| (projection.hash.clone(), projection.prev_hash.clone()))
        .collect::<HashMap<_, _>>();
    let (mut nodes, mut edges, hidden_count) = materialize_tree(spine, &ancestry_by_hash)?;

    let render_members = prepare_ordered_member_render(members, placement, nodes.len())?;
    let multi = members.len() >= 2;

    push_member_nodes(
        &MemberRender {
            ordered: &render_members.ordered,
            projection_by_hash: &projection_by_hash,
            member_id: &render_members.member_id,
            placement,
            multi,
            branch_id: &render_members.branch_id,
        },
        &mut nodes,
        &mut edges,
    )?;

    let branches = build_branches(&nodes);
    Ok(TreePayload {
        window: tree_window_from_resolved(window, hidden_count),
        nodes,
        edges,
        branches,
        legend: orphan_tree_legend(),
    })
}

/// Flat-strip fallback showing the anchor orphan and its nearest-in-time orphan neighbors
/// ordered by `(btc_header_time, btc_header_hash)` without a height spine.
pub(super) async fn anchor_strip(
    client: &Client,
    query: &TreeQuery,
    anchor_display_hash: &str,
) -> Result<TreePayload, ProjectionError> {
    let blocks =
        select_anchor_unknown_blocks(client, anchor_display_hash, &query.classification).await?;
    let block_hashes = blocks
        .iter()
        .map(|block| block.hash.clone())
        .collect::<Vec<_>>();

    let no_filter: Vec<String> = Vec::new();
    let all_events = load_active_events_for_tree(client, &no_filter, query, &block_hashes).await?;
    let proofs = load_active_proofs_for_hashes(client, &no_filter, &block_hashes).await?;
    let sources = load_sources(client).await?;

    let mut projections = project_blocks(&blocks, &all_events, &proofs, &sources, &no_filter)?;
    for projection in projections.iter_mut() {
        projection.evidence = true;
        projection.protected = true;
    }
    // Tie-break on stored `btc_header_hash` bytes to match the orphan navigator keyset.
    projections.sort_by(|a, b| {
        a.header_time
            .cmp(&b.header_time)
            .then_with(|| a.hash_bytes.cmp(&b.hash_bytes))
    });

    let ancestry_by_hash = projections
        .iter()
        .map(|node| (node.hash.clone(), node.prev_hash.clone()))
        .collect::<HashMap<_, _>>();
    let (nodes, edges, hidden_count) = materialize_tree(projections, &ancestry_by_hash)?;
    let branches = build_branches(&nodes);
    let window = ResolvedTreeWindow::for_anchor();
    Ok(TreePayload {
        window: tree_window_from_resolved(window, hidden_count),
        nodes,
        edges,
        branches,
        legend: base_tree_legend(),
    })
}

/// Resolve the anchor orphan and up to `UNHEIGHTED_NODE_LIMIT - 1` nearest neighbors by
/// `(btc_header_time, btc_header_hash)`, balancing newer/older and filling from the other side at edges.
pub(super) async fn select_anchor_unknown_blocks(
    client: &Client,
    anchor_display_hash: &str,
    classification: &[Classification],
) -> Result<Vec<BlockRow>, ProjectionError> {
    let anchor_bytes = stored_hash_from_display(anchor_display_hash)?;
    let (class_values, include_pending) = classification_filter_params(classification);
    let anchor_row = client
        .query_opt(
            "SELECT btc_header_time FROM block \
             WHERE kind = 'unknown' AND pow_validated AND btc_header_hash = $1 \
               AND ( \
                   btc_orphan_class = ANY($2::text[]) \
                   OR ($3::boolean AND btc_orphan_class IS NULL) \
               )",
            &[&anchor_bytes, &class_values, &include_pending],
        )
        .await
        .context("resolve unheighted anchor")?;
    let Some(anchor_row) = anchor_row else {
        return Err(ProjectionError::Api(ApiError::not_found(
            anchor_display_hash,
        )));
    };
    let anchor_time: i64 = anchor_row.get(0);

    let budget = UNHEIGHTED_NODE_LIMIT - 1;
    let budget_param = budget as i64;
    let newer = client
        .query(
            "SELECT btc_header_hash FROM block \
             WHERE kind = 'unknown' AND pow_validated \
               AND (btc_header_time, btc_header_hash) > ($1, $2) \
               AND ( \
                   btc_orphan_class = ANY($4::text[]) \
                   OR ($5::boolean AND btc_orphan_class IS NULL) \
               ) \
             ORDER BY btc_header_time ASC, btc_header_hash ASC LIMIT $3",
            &[
                &anchor_time,
                &anchor_bytes,
                &budget_param,
                &class_values,
                &include_pending,
            ],
        )
        .await
        .context("load anchor newer unknowns")?;
    let older = client
        .query(
            "SELECT btc_header_hash FROM block \
             WHERE kind = 'unknown' AND pow_validated \
               AND (btc_header_time, btc_header_hash) < ($1, $2) \
               AND ( \
                   btc_orphan_class = ANY($4::text[]) \
                   OR ($5::boolean AND btc_orphan_class IS NULL) \
               ) \
             ORDER BY btc_header_time DESC, btc_header_hash DESC LIMIT $3",
            &[
                &anchor_time,
                &anchor_bytes,
                &budget_param,
                &class_values,
                &include_pending,
            ],
        )
        .await
        .context("load anchor older unknowns")?;

    let mut newer_take = newer.len().min(budget.div_ceil(2));
    let older_take = older.len().min(budget - newer_take);
    newer_take = newer.len().min(budget - older_take);

    let mut hashes: Vec<Vec<u8>> = Vec::with_capacity(1 + newer_take + older_take);
    hashes.push(anchor_bytes);
    hashes.extend(
        newer
            .iter()
            .take(newer_take)
            .map(|row| row.get::<_, Vec<u8>>(0)),
    );
    hashes.extend(
        older
            .iter()
            .take(older_take)
            .map(|row| row.get::<_, Vec<u8>>(0)),
    );

    let rows = client
        .query(
            &format!("{BLOCK_ROW_SELECT} WHERE b.btc_header_hash = ANY($1::bytea[])"),
            &[&hashes],
        )
        .await
        .context("load anchor unknown neighborhood")?;
    Ok(rows_to_blocks(rows)?)
}
