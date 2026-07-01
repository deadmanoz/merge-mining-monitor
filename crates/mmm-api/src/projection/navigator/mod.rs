//! Unified navigator projection for stale blocks, stale branches, BTC orphans,
//! and orphan branches.

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

mod orphan;
mod stale;

pub async fn navigator(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    match query.target {
        NavigatorTarget::Stale => stale::stale_blocks(client, query).await,
        NavigatorTarget::StaleBranch => stale::stale_branches(client, query).await,
        NavigatorTarget::Orphan => orphan::orphans(client, query).await,
        NavigatorTarget::OrphanBranch => orphan::orphan_branches(client, query).await,
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

#[derive(Debug, Clone, Serialize)]
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
    pub cursor: String,
    pub branch: Option<NavigatorBranch>,
    pub orphan: Option<NavigatorOrphan>,
    pub view: Option<NavigatorView>,
    pub view_error: Option<NavigationError>,
}

impl NavigatorItem {
    fn cursor(&self, target: NavigatorTarget) -> NavigatorCursor {
        NavigatorCursor::new(
            target,
            self.position.axis,
            self.position.min,
            self.position.max,
            self.cursor_hash(),
        )
    }

    fn cursor_hash(&self) -> String {
        self.branch
            .as_ref()
            .map(|branch| branch.root_hash.clone())
            .unwrap_or_else(|| self.primary_hash.clone())
    }
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

fn cursor_params(query: &NavigatorQuery) -> Option<(PageEdge, &NavigatorCursor)> {
    match &query.mode {
        NavigatorMode::Page { direction, cursor } => Some(((*direction).into(), cursor)),
        _ => None,
    }
}

fn anchor_hash(query: &NavigatorQuery) -> Option<&str> {
    match &query.mode {
        NavigatorMode::Anchor { hash } => Some(hash),
        _ => None,
    }
}

trait BranchNavigatorSummary: Clone {
    fn member_hashes(&self) -> &[Vec<u8>];
    fn matches_cursor(&self, cursor: &NavigatorCursor, edge: PageEdge) -> bool;
}

fn page_branch_summaries<S>(
    summaries: &[S],
    query: &NavigatorQuery,
) -> Result<Vec<S>, ProjectionError>
where
    S: BranchNavigatorSummary,
{
    let mut rows = match &query.mode {
        NavigatorMode::Anchor { hash } => {
            let hash = stored_hash_from_display(hash)?;
            summaries
                .iter()
                .filter(|summary| summary.member_hashes().iter().any(|member| member == &hash))
                .cloned()
                .collect()
        }
        NavigatorMode::Page { direction, cursor } => {
            let edge = PageEdge::from(*direction);
            summaries
                .iter()
                .filter(|summary| summary.matches_cursor(cursor, edge))
                .cloned()
                .collect()
        }
        NavigatorMode::Latest => summaries.to_vec(),
    };
    if is_newer_page(query) {
        rows.reverse();
    }
    rows.truncate(query.limit + 1);
    Ok(rows)
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

async fn page_cursors<ExistsOlder, ExistsNewer, FutOlder, FutNewer>(
    target: NavigatorTarget,
    query: &NavigatorQuery,
    items: &[NavigatorItem],
    has_more_scan: bool,
    exists_older: ExistsOlder,
    exists_newer: ExistsNewer,
) -> Result<(Option<String>, Option<String>), ProjectionError>
where
    ExistsOlder: FnOnce(NavigatorCursor) -> FutOlder,
    ExistsNewer: FnOnce(NavigatorCursor) -> FutNewer,
    FutOlder: std::future::Future<Output = Result<bool, ProjectionError>>,
    FutNewer: std::future::Future<Output = Result<bool, ProjectionError>>,
{
    let mut next_cursor = None;
    let mut prev_cursor = None;
    let (Some(newest), Some(oldest)) = (items.first(), items.last()) else {
        return Ok((next_cursor, prev_cursor));
    };
    let newest_cursor = newest.cursor(target);
    let oldest_cursor = oldest.cursor(target);

    match &query.mode {
        NavigatorMode::Latest => {
            if has_more_scan {
                next_cursor = Some(oldest_cursor.encode());
            }
        }
        NavigatorMode::Anchor { .. } => {
            if exists_older(oldest_cursor.clone()).await? {
                next_cursor = Some(oldest_cursor.encode());
            }
            if exists_newer(newest_cursor.clone()).await? {
                prev_cursor = Some(newest_cursor.encode());
            }
        }
        NavigatorMode::Page {
            direction: NavigatorDirection::Older,
            ..
        } => {
            if has_more_scan {
                next_cursor = Some(oldest_cursor.encode());
            }
            if exists_newer(newest_cursor.clone()).await? {
                prev_cursor = Some(newest_cursor.encode());
            }
        }
        NavigatorMode::Page {
            direction: NavigatorDirection::Newer,
            ..
        } => {
            if exists_older(oldest_cursor.clone()).await? {
                next_cursor = Some(oldest_cursor.encode());
            }
            if has_more_scan {
                prev_cursor = Some(newest_cursor.encode());
            }
        }
    }

    Ok((next_cursor, prev_cursor))
}
