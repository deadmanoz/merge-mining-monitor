//! Unified navigator projection for stale blocks, stale branches, catalogued
//! invalid blocks, BTC orphans, orphan branches, and mixed-signal blocks.

use anyhow::Result;
use serde::Serialize;
use tokio_postgres::Client;

use crate::query::{
    NavigatorAxis, NavigatorCursor, NavigatorDirection, NavigatorMode, NavigatorQuery,
    NavigatorTarget,
};

use super::ProjectionError;
use super::shared::stored_hash_from_display;
use super::stale_navigation::{NavigationError, TreeNavigation};

mod height;
mod invalid_block;
mod keyset;
mod mixed_signal;
mod orphan;
mod stale;

pub async fn navigator(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    match query.target {
        NavigatorTarget::Stale => stale::stale_blocks(client, query).await,
        NavigatorTarget::StaleBranch => stale::stale_branches(client, query).await,
        NavigatorTarget::InvalidBlock => invalid_block::invalid_blocks(client, query).await,
        NavigatorTarget::Orphan => orphan::orphans(client, query).await,
        NavigatorTarget::OrphanBranch => orphan::orphan_branches(client, query).await,
        NavigatorTarget::MixedSignal => mixed_signal::mixed_signals(client, query).await,
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigatorPayload {
    pub target: &'static str,
    pub items: Vec<NavigatorItem>,
    pub total: u64,
    pub facets: NavigatorFacets,
    pub next_cursor: Option<String>,
    pub prev_cursor: Option<String>,
}

impl NavigatorPayload {
    fn new(
        target: NavigatorTarget,
        items: Vec<NavigatorItem>,
        total: u64,
        facets: NavigatorFacets,
        next_cursor: Option<String>,
        prev_cursor: Option<String>,
    ) -> Self {
        Self {
            target: target.as_str(),
            total: total.max(items.len() as u64),
            items,
            facets,
            next_cursor,
            prev_cursor,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NavigatorFacets {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphan_classes: Option<OrphanClassCounts>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct OrphanClassCounts {
    pub strict: u64,
    pub weak: u64,
    pub excluded: u64,
    pub pending: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigatorItem {
    pub id: String,
    pub kind: &'static str,
    pub primary_hash: String,
    pub label: String,
    pub position: NavigatorPosition,
    pub index: u64,
    pub cursor: String,
    pub branch: Option<NavigatorBranch>,
    pub orphan: Option<NavigatorOrphan>,
    pub view: Option<NavigatorView>,
    pub view_error: Option<NavigationError>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigatorPosition {
    pub axis: NavigatorAxis,
    pub min: i64,
    pub max: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigatorBranch {
    pub branch_id: String,
    pub root_hash: String,
    pub tip_hashes: Vec<String>,
    pub depth: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct NavigatorOrphan {
    pub btc_orphan_class: Option<String>,
    pub btc_header_time: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum NavigatorView {
    TreeWindow {
        target_height: i32,
        tree_from: i32,
        tree_to: i32,
        select_hash: String,
        center_hash: String,
    },
    UnheightedAnchor {
        anchor_hash: String,
        select_hash: String,
        center_hash: String,
    },
}

impl From<TreeNavigation> for NavigatorView {
    fn from(nav: TreeNavigation) -> Self {
        Self::TreeWindow {
            target_height: nav.target_height,
            tree_from: nav.tree_from,
            tree_to: nav.tree_to,
            select_hash: nav.select_hash,
            center_hash: nav.center_hash,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PageEdge {
    Older,
    Newer,
}

impl From<NavigatorDirection> for PageEdge {
    fn from(direction: NavigatorDirection) -> Self {
        match direction {
            NavigatorDirection::Older => Self::Older,
            NavigatorDirection::Newer => Self::Newer,
        }
    }
}

trait BranchNavigatorSummary: Clone {
    fn member_hashes(&self) -> &[Vec<u8>];
    fn matches_cursor(&self, cursor: &NavigatorCursor, edge: PageEdge) -> bool;
}

struct BranchPage<S> {
    rows: Vec<S>,
    start_offset: usize,
    exists_older: bool,
    exists_newer: bool,
}

impl<S> BranchPage<S> {
    fn first_index(&self) -> Option<u64> {
        (!self.rows.is_empty()).then_some(self.start_offset as u64 + 1)
    }
}

fn page_branch_summaries<S>(
    summaries: &[S],
    query: &NavigatorQuery,
) -> Result<BranchPage<S>, ProjectionError>
where
    S: BranchNavigatorSummary,
{
    let mut indexed = match &query.mode {
        NavigatorMode::Anchor { hash } => {
            let hash = stored_hash_from_display(hash)?;
            summaries
                .iter()
                .enumerate()
                .filter(|(_, summary)| summary.member_hashes().iter().any(|member| member == &hash))
                .map(|(index, summary)| (index, summary.clone()))
                .collect::<Vec<_>>()
        }
        NavigatorMode::Page { direction, cursor } => {
            let edge = PageEdge::from(*direction);
            summaries
                .iter()
                .enumerate()
                .filter(|(_, summary)| summary.matches_cursor(cursor, edge))
                .map(|(index, summary)| (index, summary.clone()))
                .collect::<Vec<_>>()
        }
        NavigatorMode::Latest => summaries.iter().cloned().enumerate().collect::<Vec<_>>(),
    };
    if is_newer_page(query) {
        indexed.reverse();
    }
    indexed.truncate(query.limit);
    if is_newer_page(query) {
        indexed.reverse();
    }
    // A page is one contiguous run of the sorted summaries.
    let start_offset = indexed.first().map(|(index, _)| *index).unwrap_or(0);
    Ok(BranchPage {
        exists_older: start_offset + indexed.len() < summaries.len(),
        exists_newer: start_offset > 0,
        rows: indexed.into_iter().map(|(_, summary)| summary).collect(),
        start_offset,
    })
}

fn is_newer_page(query: &NavigatorQuery) -> bool {
    matches!(
        query.mode,
        NavigatorMode::Page {
            direction: NavigatorDirection::Newer,
            ..
        }
    )
}

/// The page's next (older) and previous (newer) cursors: its oldest and newest items' own.
fn page_cursors(
    items: &[NavigatorItem],
    exists_older: bool,
    exists_newer: bool,
) -> (Option<String>, Option<String>) {
    (
        items
            .last()
            .filter(|_| exists_older)
            .map(|item| item.cursor.clone()),
        items
            .first()
            .filter(|_| exists_newer)
            .map(|item| item.cursor.clone()),
    )
}
