//! Anchor-centered orphan-component placement for the unheighted_anchor
//! tree mode.

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

/// Anchor mode (`unheighted_anchor`), read-only path (no Bitcoin Core hydration):
/// resolve the anchor orphan, derive its placement height, and dangle it as a fork
/// off the canonical chain in a `±ANCHOR_PLACEMENT_RADIUS` window. The request
/// path never fetches Bitcoin Core; sparse local context falls back to a flat
/// strip.
pub(super) async fn anchor_tree(
    client: &Client,
    query: &TreeQuery,
    anchor_display_hash: &str,
) -> Result<TreePayload, ProjectionError> {
    let orphan = resolve_anchor_orphan(client, anchor_display_hash, &query.classification).await?;
    let (members, truncated) =
        load_orphan_component(client, &orphan, &query.classification).await?;
    // A component too large to render as a bounded fork degrades to the flat strip
    // rather than a partial branch presented as the whole one.
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

/// Resolve and validate the anchor orphan as a single `BlockRow`. The hash must be
/// an existing PoW-valid `unknown` whose `btc_orphan_class` is in the requested
/// set, else `not_found` (so the navigator never lands on a pending/excluded block
/// in the default strict+weak view). Only `classification` gates eligibility; no
/// `source`/`kinds`/`min_sources` filtering applies in anchor mode.
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

/// Hydrate and project the canonical context spine over the placement
/// window (full bypass; canonical-only; deterministic order). Returns the
/// spine plus the shared source map, or None when no canonical context
/// exists to dangle the component from.
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
    // Canonical-only context (see the single-orphan rationale, preserved here): stale
    // forks and their competition / stale-branch machinery are out of scope for the
    // anchor view, and dropping stale keeps the fail-closed competition invariant.
    spine.retain(|projection| projection.kind == ParentKind::Canonical);
    if spine.is_empty() {
        // No canonical context in the window to dangle the component from.
        return Ok(None);
    }
    for projection in spine.iter_mut() {
        projection.evidence = true;
        projection.protected = true;
    }
    // `load_blocks_for_tree` has no `ORDER BY`, so sort the spine deterministically
    // (height then hash) before `materialize_tree` assigns response-local ids.
    sort_nodes(&mut spine);

    Ok(Some((spine, sources)))
}

/// Shared, read-only anchor projection for a whole orphan COMPONENT: load the
/// canonical `±ANCHOR_PLACEMENT_RADIUS` window spanning every member's placement
/// height (full filter bypass), project it as the context spine, then graft each
/// member as a placed fork. A proven `prev_hash` link between two members renders as
/// a solid `orphan` edge. The component root attaches to the spine: a strict root
/// whose stored `prev_hash` is the canonical at `placement - 1` gets a solid
/// `orphan` edge to that real predecessor; otherwise (an absent/mismatched
/// predecessor, or any approximate placement) a distinct `orphan_approx` edge to the
/// nearest in-window canonical, keeping `prev_hash` truthful. A depth-2+ component
/// carries an `orphan-<root_hash>` branch on every member (so `build_branches` emits
/// the orphan branch and the navigator can step it); a single orphan stays
/// branch-less (the depth-1 specialization). Falls back to the flat strip when the
/// window holds no canonical context to dangle the component from.
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

    // Canonical context spine over the placement window (full bypass). No tree
    // reduction, so a member's predecessor stays visible in the bounded window.
    let Some((spine, sources)) =
        load_spine_projections(client, &bypass, window, &no_filter).await?
    else {
        // No canonical context in the window to dangle the component from.
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

/// Flat-strip fallback for anchor mode: the original navigator landing that shows
/// the anchor orphan plus its nearest-in-time orphan neighbors (kind='unknown'
/// AND pow_validated, filtered by the orphan-class set, the same population as the
/// `/orphans` index), bounded to the node cap, with NO height spine and NO
/// `source`/`kinds`/`min_sources` filtering. Used only when no placement height
/// can be derived for the anchor AND no canonical context is available to dangle
/// it from (see `anchor_projection`); the normal path now places the orphan as a
/// fork at its own height. Nodes are ordered by `(btc_header_time,
/// btc_header_hash)` so the buildless frontend lays them out as a left-to-right
/// time strip.
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

    // Full filter bypass: an empty source filter loads all sources for the
    // selected unknowns (nodes AND their events / proofs / summaries), so an
    // incidental filter can neither drop the anchor nor strip its evidence.
    // `include_unheighted` is false in anchor mode, so
    // `load_active_events_for_tree` fires only its hash-IN clause.
    let no_filter: Vec<String> = Vec::new();
    let all_events = load_active_events_for_tree(client, &no_filter, query, &block_hashes).await?;
    let proofs = load_active_proofs_for_hashes(client, &no_filter, &block_hashes).await?;
    let sources = load_sources(client).await?;

    let mut projections = project_blocks(&blocks, &all_events, &proofs, &sources, &no_filter)?;
    // Every selected unknown is shown; the view is a flat detached set with no
    // reduction or stale-branch logic. Order by `(header_time, hash)` so the
    // frontend lays the nodes out as a left-to-right time strip; the shared
    // `sort_nodes` would instead order these null-height nodes by hash.
    for projection in projections.iter_mut() {
        projection.evidence = true;
        projection.protected = true;
    }
    // Tie-break on the STORED `btc_header_hash` bytes, not the reversed display
    // hex, so a same-`btc_header_time` ordering matches the orphan navigator
    // target and its `(btc_header_time, btc_header_hash)` keyset.
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

/// Resolve the anchor orphan and its nearest-in-time orphan neighbors as
/// `BlockRow`s. The anchor hash must be an existing PoW-valid unknown whose
/// `btc_orphan_class` is in the requested set, else `not_found` (so the navigator
/// never lands on a pending/excluded block in the default strict+weak view). The
/// neighborhood is the anchor plus the nearest `UNHEIGHTED_NODE_LIMIT - 1`
/// same-filtered orphans by the `(btc_header_time, btc_header_hash)` keyset,
/// balanced newer/older but FILLED from the populated side when the anchor sits
/// near an index edge (e.g. the Latest jump, where there are no newer orphans), so
/// the total stays at or under the node cap while still returning the closest
/// available neighbors.
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

    // Fetch up to the whole neighbor budget on each side (closest first), then
    // decide how many to keep per side so the budget is filled even at an edge.
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

    // Aim for a balanced split, then let each side absorb the budget the other
    // side could not fill.
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
