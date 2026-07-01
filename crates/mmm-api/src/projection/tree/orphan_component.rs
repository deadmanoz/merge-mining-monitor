//! The orphan-component placement engine for anchor mode: BFS the proven
//! `prev_hash`-linked orphan component, derive a pinned placement height for it,
//! and graft its members onto a canonical spine as a placed fork.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio_postgres::Client;
use tracing::warn;

use super::super::ProjectionError;
use super::super::materialize::{ParentProjection, project_blocks};
use super::super::shared::{
    BlockRow, classification_filter_params, display_hash, load_active_proofs_for_hashes,
    rows_to_blocks,
};
use super::window::{ResolvedTreeWindow, load_active_events_for_tree};
use super::{BLOCK_ROW_SELECT, TreeEdge, TreeNode, TreeNodeBranch};
use crate::normalize::{Classification, ParentKind};
use crate::query::TreeQuery;

/// Half-width of the canonical context window an anchored orphan is placed into.
pub(super) const ANCHOR_PLACEMENT_RADIUS: i32 = 16;

/// Derive a fork placement height for an anchor orphan, gated by its class:
/// `strict_btc_orphan` places by its validated BIP34 coinbase height (exact);
/// `weak_btc_orphan` / `btc_stale_excluded` / pending place by the
/// timestamp-selected DAA-epoch first-block height (approximate, `~`). The epoch
/// lookup is monotonic by construction and RPC-free, reusing the weak classifier's
/// own committed nBits table rather than binary-searching non-monotonic header
/// times. Returns `(height, approx)`, or `None` when no height can be derived (the
/// caller then falls back to the flat strip).
pub(super) async fn anchor_placement_height(
    client: &Client,
    orphan: &BlockRow,
) -> Result<Option<(i32, bool)>, ProjectionError> {
    if orphan.btc_orphan_class.as_deref() == Some("strict_btc_orphan")
        && let Some(height) =
            crate::projection::shared::load_strict_bip34_height(client, &orphan.hash).await?
    {
        return Ok(Some((height, false)));
    }
    let table = mmm_capture::nbits_table::table();
    // Above the committed table's horizon the table genuinely cannot place the
    // orphan (the same condition that makes it pending, not excluded). Leave it
    // unplaced (the caller falls back to the flat strip) rather than guessing it
    // near the local tip via the below-table fallback below.
    if let Some(covered_max_time) = table.covered_max_time()
        && orphan.header_time > covered_max_time
    {
        return Ok(None);
    }
    if let Some(height) = table.epoch_height_for_time(orphan.header_time) {
        return Ok(Some((height, true)));
    }
    // Defensive fallback ONLY for a timestamp BELOW the committed table's first
    // epoch (a degenerate pre-2009 time; above-horizon already returned None): the
    // local nearest-time canonical block, if any. Sparse local rows make this a
    // last resort, not the primary weak source.
    let row = client
        .query_opt(
            "SELECT btc_height FROM block \
             WHERE kind = 'canonical' AND btc_height IS NOT NULL \
             ORDER BY abs(btc_header_time - $1), btc_height \
             LIMIT 1",
            &[&orphan.header_time],
        )
        .await
        .context("nearest-time canonical placement fallback")?;
    Ok(row
        .and_then(|row| row.get::<_, Option<i32>>(0))
        .map(|height| (height, true)))
}

/// The canonical context window a placed orphan component spans: every member's
/// `placement_height`, padded by `ANCHOR_PLACEMENT_RADIUS` on each side.
pub(super) fn component_placement_window(min_height: i32, max_height: i32) -> ResolvedTreeWindow {
    ResolvedTreeWindow::explicit(
        (min_height - ANCHOR_PLACEMENT_RADIUS).max(0),
        max_height + ANCHOR_PLACEMENT_RADIUS,
    )
}

/// Backstop on orphan-component members for the anchor view. Chosen so the worst
/// case (a linear component spanning one height per member) stays well under the
/// tree's node budget: at most `2 * CAP + 2 * ANCHOR_PLACEMENT_RADIUS` nodes
/// (members plus the canonical placement window), i.e. ~288 at CAP = 128. A
/// component that hits the cap is log-warned AND the anchor view degrades to the
/// flat strip, rather than rendering a partial branch presented as the whole one.
pub(super) const ORPHAN_COMPONENT_CAP: usize = 128;

/// A placed orphan component: every member's derived `placement_height`, the
/// in-component parent links (a member hash -> the member whose hash is its
/// `prev_hash`, for the solid `orphan` member edges), the root, and whether
/// placement is approximate (true iff no member is a strict orphan).
pub(super) struct ComponentPlacement {
    pub(super) placements: HashMap<Vec<u8>, i32>,
    pub(super) parent_in_set: HashMap<Vec<u8>, Vec<u8>>,
    pub(super) root_hash: Vec<u8>,
    pub(super) approx: bool,
    pub(super) min_height: i32,
    pub(super) max_height: i32,
}

/// BFS the proven `prev_hash`-linked orphan component containing `anchor`, over the
/// orphan candidate set (`kind='unknown' AND pow_validated`, in the classification
/// set - matching `resolve_anchor_orphan` / `/orphans`, so revoked husks and
/// out-of-class rows are excluded). Each round expands both directions: a child (a
/// candidate whose `prev_hash` is a known member) and a parent (a candidate whose
/// hash is a member's `prev_hash`). Bounded by `ORPHAN_COMPONENT_CAP`.
pub(super) async fn load_orphan_component(
    client: &Client,
    anchor: &BlockRow,
    classification: &[Classification],
) -> Result<(Vec<BlockRow>, bool), ProjectionError> {
    let (class_values, include_pending) = classification_filter_params(classification);
    let mut members: HashMap<Vec<u8>, BlockRow> = HashMap::new();
    members.insert(anchor.hash.clone(), anchor.clone());
    let mut frontier = vec![anchor.clone()];
    let mut capped = false;
    while !frontier.is_empty() {
        let hashes = frontier.iter().map(|b| b.hash.clone()).collect::<Vec<_>>();
        let prevs = frontier
            .iter()
            .map(|b| b.prev_hash.clone())
            .collect::<Vec<_>>();
        let query = format!(
            "{BLOCK_ROW_SELECT} \
             WHERE b.kind = 'unknown' AND b.pow_validated \
               AND ( b.btc_orphan_class = ANY($1::text[]) \
                     OR ($2::boolean AND b.btc_orphan_class IS NULL) ) \
               AND ( b.btc_prev_header_hash = ANY($3::bytea[]) \
                     OR b.btc_header_hash = ANY($4::bytea[]) )"
        );
        let rows = client
            .query(&query, &[&class_values, &include_pending, &hashes, &prevs])
            .await
            .context("load orphan component")?;
        let mut next = Vec::new();
        for cand in rows_to_blocks(rows)? {
            if members.contains_key(&cand.hash) {
                continue;
            }
            if members.len() >= ORPHAN_COMPONENT_CAP {
                capped = true;
                break;
            }
            members.insert(cand.hash.clone(), cand.clone());
            next.push(cand);
        }
        if capped {
            break;
        }
        frontier = next;
    }
    if capped {
        warn!(
            anchor = %display_hash(&anchor.hash).unwrap_or_default(),
            cap = ORPHAN_COMPONENT_CAP,
            "orphan component truncated at cap"
        );
    }
    Ok((members.into_values().collect(), capped))
}

/// Place an orphan component as a unit. The proven `prev_hash` path gives exact
/// relative heights (+1 per link), so pin ONE absolute anchor and derive every
/// member as `anchor + path_offset`. The anchor is exact when any member is a strict
/// orphan (its validated BIP34 height, offset back to the root); otherwise it is the
/// root's timestamp-selected DAA-epoch height (approximate). Returns `None` when no
/// height can be derived (caller falls back to the flat strip).
pub(super) async fn place_orphan_component(
    client: &Client,
    members: &[BlockRow],
) -> Result<Option<ComponentPlacement>, ProjectionError> {
    if members.is_empty() {
        return Ok(None);
    }
    let by_hash: HashMap<&[u8], &BlockRow> =
        members.iter().map(|m| (m.hash.as_slice(), m)).collect();
    let mut parent_in_set: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
    let mut children_by_parent: HashMap<Vec<u8>, Vec<Vec<u8>>> = HashMap::new();
    for m in members {
        if by_hash.contains_key(m.prev_hash.as_slice()) {
            parent_in_set.insert(m.hash.clone(), m.prev_hash.clone());
            children_by_parent
                .entry(m.prev_hash.clone())
                .or_default()
                .push(m.hash.clone());
        }
    }
    // The root is the unique member whose prev is not a member (each member has <=1
    // in-set parent and the chain is acyclic). A fork off a shared ABSENT parent
    // never lands in one component (the absent parent is not a member, so siblings
    // are not prev-linked through the member set), so a single root holds.
    let mut roots = members
        .iter()
        .filter(|m| !by_hash.contains_key(m.prev_hash.as_slice()))
        .collect::<Vec<_>>();
    roots.sort_by(|a, b| a.hash.cmp(&b.hash));
    let Some(root) = roots.first().copied() else {
        return Ok(None);
    };
    if roots.len() > 1 {
        warn!(
            roots = roots.len(),
            "orphan component has multiple roots; using the lowest-hash root"
        );
    }

    // Offsets along the proven path: root = 0, each child = parent + 1.
    let mut offset: HashMap<Vec<u8>, i32> = HashMap::new();
    offset.insert(root.hash.clone(), 0);
    let mut stack = vec![root.hash.clone()];
    while let Some(hash) = stack.pop() {
        let depth = offset[&hash];
        if let Some(children) = children_by_parent.get(&hash) {
            for child in children {
                if !offset.contains_key(child) {
                    offset.insert(child.clone(), depth + 1);
                    stack.push(child.clone());
                }
            }
        }
    }

    // Pin the absolute root height. A strict member at offset k implies a root
    // height of (its BIP34 height) - k. Strict members MUST AGREE: consistent strict
    // evidence pins the component EXACTLY (`placement_approx = false`). Conflicting
    // strict heights (corrupt or inconsistent evidence) cannot be exact, so the
    // component degrades to APPROXIMATE placement at the lowest implied height
    // (the `~` label and a dashed root edge) rather than silently rendering at a
    // height that contradicts a member. With no strict member, the root's
    // timestamp-selected DAA-epoch height is the approximate pin.
    let mut implied_roots: Vec<i32> = Vec::new();
    for m in members {
        if m.btc_orphan_class.as_deref() == Some("strict_btc_orphan")
            && let Some(height) =
                crate::projection::shared::load_strict_bip34_height(client, &m.hash).await?
        {
            implied_roots.push(height - offset.get(&m.hash).copied().unwrap_or(0));
        }
    }
    let (root_abs, approx) = match (
        implied_roots.iter().min().copied(),
        implied_roots.iter().max().copied(),
    ) {
        (Some(min), Some(max)) if min == max => (min, false),
        (Some(min), Some(max)) => {
            warn!(
                root = %display_hash(&root.hash).unwrap_or_default(),
                min,
                max,
                "conflicting strict orphan heights in component; degrading to approximate placement"
            );
            (min, true)
        }
        _ => match anchor_placement_height(client, root).await? {
            Some((height, _)) => (height, true),
            None => return Ok(None),
        },
    };

    let mut placements = HashMap::new();
    let mut min_height = i32::MAX;
    let mut max_height = i32::MIN;
    for m in members {
        let height = root_abs + offset.get(&m.hash).copied().unwrap_or(0);
        placements.insert(m.hash.clone(), height);
        min_height = min_height.min(height);
        max_height = max_height.max(height);
    }
    Ok(Some(ComponentPlacement {
        placements,
        parent_in_set,
        root_hash: root.hash.clone(),
        approx,
        min_height,
        max_height,
    }))
}

/// A full-bypass query for loading the canonical context window: empty source
/// filter, all kinds, `min_sources = 1`, no unheighted/anchor recursion. Preserves
/// the anchor-mode contract that `source`/`kinds`/`min_sources` never filter the
/// canonical spine (only `classification` gates anchor eligibility, applied in
/// `resolve_anchor_orphan`).
pub(super) fn bypass_window_query(
    query: &TreeQuery,
    from_height: i32,
    to_height: i32,
) -> TreeQuery {
    TreeQuery {
        from_height: Some(from_height),
        to_height: Some(to_height),
        at_height: None,
        at_time: None,
        context: crate::query::TreeContextPolicy::Exact,
        kinds: vec![
            ParentKind::Canonical,
            ParentKind::Stale,
            ParentKind::Unknown,
            ParentKind::Near,
        ],
        source_filter: Vec::new(),
        classification: query.classification.clone(),
        include_near: true,
        include_unheighted: false,
        unheighted_from: None,
        unheighted_to: None,
        unheighted_anchor: None,
        min_sources: 1,
        query: query.query.clone(),
    }
}

/// Read-only inputs for rendering one placed orphan component's members onto an
/// already-materialized spine: the placement, the member ids, the per-member
/// projections, and the branch tag.
pub(super) struct MemberRender<'a> {
    pub(super) ordered: &'a [&'a BlockRow],
    pub(super) projection_by_hash: &'a HashMap<Vec<u8>, ParentProjection>,
    pub(super) member_id: &'a HashMap<Vec<u8>, usize>,
    pub(super) placement: &'a ComponentPlacement,
    pub(super) multi: bool,
    pub(super) branch_id: &'a str,
}

pub(super) struct OrderedMemberRender<'a> {
    pub(super) ordered: Vec<&'a BlockRow>,
    pub(super) member_id: HashMap<Vec<u8>, usize>,
    pub(super) branch_id: String,
}

pub(super) fn prepare_ordered_member_render<'a>(
    members: &'a [BlockRow],
    placement: &ComponentPlacement,
    base_id: usize,
) -> Result<OrderedMemberRender<'a>, ProjectionError> {
    let mut ordered = members.iter().collect::<Vec<_>>();
    ordered.sort_by(|a, b| {
        let ha = placement
            .placements
            .get(&a.hash)
            .copied()
            .unwrap_or_default();
        let hb = placement
            .placements
            .get(&b.hash)
            .copied()
            .unwrap_or_default();
        ha.cmp(&hb).then_with(|| a.hash.cmp(&b.hash))
    });
    let member_id = ordered
        .iter()
        .enumerate()
        .map(|(index, member)| (member.hash.clone(), base_id + index + 1))
        .collect::<HashMap<Vec<u8>, usize>>();
    let root_display = display_hash(&placement.root_hash)?;
    let branch_id = format!("orphan-{root_display}");
    Ok(OrderedMemberRender {
        ordered,
        member_id,
        branch_id,
    })
}

/// Emit each placed component member as a `TreeNode` plus its incoming edge: a
/// proven member-to-member link is a solid `orphan` edge; the root attaches to the
/// spine with a solid `orphan` edge when it is a strict root whose stored
/// `prev_hash` is the canonical at `placement-1`, else a dashed `orphan_approx`
/// edge to the nearest in-window canonical. Keeps `prev_id`/`prev_hash` truthful.
pub(super) fn push_member_nodes(
    render: &MemberRender<'_>,
    nodes: &mut Vec<TreeNode>,
    edges: &mut Vec<TreeEdge>,
) -> Result<(), ProjectionError> {
    let MemberRender {
        ordered,
        projection_by_hash,
        member_id,
        placement,
        multi,
        branch_id,
    } = *render;
    for member in ordered {
        let projection = projection_by_hash
            .get(&member.hash)
            .cloned()
            .ok_or_else(|| {
                ProjectionError::Internal(anyhow::anyhow!("anchor member projected to no node"))
            })?;
        let id = member_id[&member.hash];
        let placement_height = placement.placements.get(&member.hash).copied();

        // A proven member-to-member link is a solid `orphan` edge; the component root
        // attaches to the spine (solid for a verified strict root, else dashed to the
        // nearest in-window canonical), keeping `prev_hash` truthful.
        let (prev_id, edge) = if let Some(parent_hash) = placement.parent_in_set.get(&member.hash) {
            let parent_display = display_hash(parent_hash)?;
            let prev_id = member_id.get(parent_hash).copied();
            (
                prev_id,
                prev_id.map(|_| TreeEdge {
                    from_hash: parent_display,
                    to_hash: projection.hash.clone(),
                    hidden_count: None,
                    edge_kind: "orphan",
                }),
            )
        } else {
            let placed = placement_height.unwrap_or(placement.min_height);
            let strict_verified = !placement.approx
                && nodes.iter().any(|node| {
                    node.kind == "canonical"
                        && node.height == Some(placed - 1)
                        && node.hash == projection.prev_hash
                });
            let attach = if strict_verified {
                nodes
                    .iter()
                    .find(|node| node.kind == "canonical" && node.height == Some(placed - 1))
            } else {
                nodes
                    .iter()
                    .filter(|node| node.kind == "canonical" && node.height.is_some())
                    .min_by_key(|node| {
                        let height = node.height.unwrap_or(placed);
                        ((height - placed).abs(), height)
                    })
            };
            match attach {
                Some(attach) => {
                    let edge_kind = if strict_verified {
                        "orphan"
                    } else {
                        "orphan_approx"
                    };
                    let prev_id = (attach.hash == projection.prev_hash).then_some(attach.id);
                    (
                        prev_id,
                        Some(TreeEdge {
                            from_hash: attach.hash.clone(),
                            to_hash: projection.hash.clone(),
                            hidden_count: None,
                            edge_kind,
                        }),
                    )
                }
                None => (None, None),
            }
        };
        if let Some(edge) = edge {
            edges.push(edge);
        }
        nodes.push(TreeNode {
            id,
            hash: projection.hash,
            height: None,
            kind: "unknown",
            btc_orphan_class: projection.btc_orphan_class,
            prev_id,
            prev_hash: projection.prev_hash,
            bitcoin_miner_pool: projection.bitcoin_miner_pool,
            display_miner_pool: projection.display_miner_pool,
            display_miner_basis: projection.display_miner_basis,
            source_summary: projection.source_summary,
            child_chain_evidence: projection.child_chain_evidence,
            branch: multi.then(|| TreeNodeBranch {
                branch_id: branch_id.to_owned(),
            }),
            proof_state: projection.proof_state,
            competition: None,
            placement_height,
            placement_approx: placement.approx,
        });
    }
    Ok(())
}

/// Hydrate every component member from its own evidence and project it, keyed
/// by stored hash. Anchor mode passes an empty source filter for its documented
/// bypass.
pub(super) async fn load_member_projections(
    client: &Client,
    members: &[BlockRow],
    bypass: &TreeQuery,
    sources: &HashMap<String, super::super::shared::SourceRecord>,
    source_filter: &[String],
) -> Result<HashMap<Vec<u8>, ParentProjection>, ProjectionError> {
    let member_hashes = members
        .iter()
        .map(|member| member.hash.clone())
        .collect::<Vec<_>>();
    let member_events =
        load_active_events_for_tree(client, source_filter, bypass, &member_hashes).await?;
    let member_proofs =
        load_active_proofs_for_hashes(client, source_filter, &member_hashes).await?;
    let projection_by_hash = project_blocks(
        members,
        &member_events,
        &member_proofs,
        sources,
        source_filter,
    )?
    .into_iter()
    .map(|projection| (projection.hash_bytes.clone(), projection))
    .collect::<HashMap<Vec<u8>, ParentProjection>>();

    Ok(projection_by_hash)
}
