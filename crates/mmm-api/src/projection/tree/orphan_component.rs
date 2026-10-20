//! Orphan-component BFS, placement height derivation, and canonical-spine grafting for anchor mode.

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

pub(super) const ANCHOR_PLACEMENT_RADIUS: i32 = 16;

/// Derive fork placement height for an anchor orphan: exact BIP34 height for `strict_btc_orphan`,
/// or approximate DAA-epoch height from the persisted Core header cache for weak/excluded/pending.
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
    let table = match mmm_store::load_bitcoin_core_nbits_table_if_present(client).await {
        Ok(Some(table)) => table,
        Ok(None) => return Ok(None),
        Err(error) => {
            warn!(error = %error, "Core header cache unavailable for orphan placement");
            return Ok(None);
        }
    };
    // Above the persisted Core cache horizon the table cannot place the orphan; return None
    // rather than falling back to the local nearest-time query below.
    if orphan.header_time > table.horizon_time() {
        return Ok(None);
    }
    if let Some(height) = table.epoch_height_for_time(orphan.header_time) {
        return Ok(Some((height, true)));
    }
    // Pre-2009 timestamp below the Core cache's first epoch: fall back to nearest-time canonical block.
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

pub(super) fn component_placement_window(min_height: i32, max_height: i32) -> ResolvedTreeWindow {
    ResolvedTreeWindow::explicit(
        (min_height - ANCHOR_PLACEMENT_RADIUS).max(0),
        max_height + ANCHOR_PLACEMENT_RADIUS,
    )
}

/// Cap on orphan-component members before anchor mode degrades to the flat strip.
pub(super) const ORPHAN_COMPONENT_CAP: usize = 128;

pub(super) struct ComponentPlacement {
    pub(super) placements: HashMap<Vec<u8>, i32>,
    pub(super) parent_in_set: HashMap<Vec<u8>, Vec<u8>>,
    pub(super) root_hash: Vec<u8>,
    pub(super) approx: bool,
    pub(super) min_height: i32,
    pub(super) max_height: i32,
}

/// Bidirectional BFS over `prev_hash`-linked PoW-valid `unknown` blocks matching `classification`,
/// bounded by `ORPHAN_COMPONENT_CAP`.
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

/// Pin the component root height and derive every member as `root_abs + path_offset`.
/// Exact when strict members agree on implied root height; degrades to approximate at `min` on
/// conflict, or uses the root's approximate DAA-epoch height when no strict member exists.
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

/// Full-bypass `TreeQuery` for loading the canonical context window in anchor mode.
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
        ],
        source_filter: Vec::new(),
        classification: query.classification.clone(),
        include_unheighted: false,
        unheighted_from: None,
        unheighted_to: None,
        unheighted_anchor: None,
        min_sources: 1,
        query: query.query.clone(),
    }
}

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

/// Emit each placed component member as a `TreeNode` and attach its incoming `orphan` or
/// `orphan_approx` edge while keeping `prev_id`/`prev_hash` truthful.
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
