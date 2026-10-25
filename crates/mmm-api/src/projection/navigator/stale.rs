use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::query::{NavigatorAxis, NavigatorCursor, NavigatorQuery, NavigatorTarget};

use super::super::ProjectionError;
use super::super::branch_summary::{BranchRow, branch_components};
use super::super::shared::{
    display_hash, load_max_complete_canonical_height, stored_hash_from_display,
};
use super::super::stale_navigation::{
    NavigationReadiness, NavigationSpan, load_navigation_readiness, navigation_for_span,
};
use super::height::height_blocks;
use super::keyset::apply_first_index;
use super::{
    BranchNavigatorSummary, NavigatorBranch, NavigatorFacets, NavigatorItem, NavigatorPayload,
    NavigatorPosition, NavigatorView, PageEdge, page_branch_summaries, page_cursors,
};

const STALE_ELIGIBLE: &str = "SELECT stale.btc_height AS axis, 0::bigint AS tiebreak, \
     stale.btc_header_hash AS hash, FALSE AS orphan, NULL::text AS orphan_class \
     FROM block stale \
     JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
     WHERE stale.kind = 'stale' AND canonical.kind = 'canonical'";

pub async fn stale_blocks(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::Stale);
    height_blocks(client, query, STALE_ELIGIBLE, &[], "Stale").await
}

#[derive(Debug, Clone)]
struct StaleBranchIndexRow {
    hash: Vec<u8>,
    prev_hash: Vec<u8>,
    height: i32,
}

#[derive(Debug, Clone)]
struct StaleBranchSummary {
    root_hash: Vec<u8>,
    member_hashes: Vec<Vec<u8>>,
    tip_hashes: Vec<Vec<u8>>,
    btc_height_min: i32,
    btc_height_max: i32,
    depth: usize,
}

pub async fn stale_branches(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::StaleBranch);
    debug_assert!(query.classification.is_empty());
    let max_complete_height = load_max_complete_canonical_height(client).await?;
    let mut summaries = load_stale_branch_summaries(client).await?;
    summaries.sort_by(sort_stale_branch_desc);
    let total = summaries.len() as u64;

    let page = page_branch_summaries(&summaries, query)?;
    let spans = page
        .rows
        .iter()
        .map(|summary| NavigationSpan {
            target_height: summary.btc_height_min,
            span_min: summary.btc_height_min,
            span_max: summary.btc_height_max,
            required_nodes: summary.depth,
        })
        .collect::<Vec<_>>();
    let readiness = load_navigation_readiness(client, max_complete_height, &spans).await?;
    let mut items = page
        .rows
        .iter()
        .map(|summary| stale_branch_item(summary, max_complete_height, readiness.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    apply_first_index(&mut items, page.first_index());
    let (next_cursor, prev_cursor) = page_cursors(&items, page.exists_older, page.exists_newer);

    Ok(NavigatorPayload::new(
        NavigatorTarget::StaleBranch,
        items,
        total,
        NavigatorFacets::default(),
        next_cursor,
        prev_cursor,
    ))
}

async fn load_stale_branch_summaries(
    client: &Client,
) -> Result<Vec<StaleBranchSummary>, ProjectionError> {
    let rows = client
        .query(
            "SELECT stale.btc_header_hash, stale.btc_prev_header_hash, stale.btc_height \
             FROM block stale \
             JOIN block winning ON winning.btc_header_hash = stale.canonical_competitor_hash \
             WHERE stale.kind = 'stale' \
               AND winning.kind = 'canonical'",
            &[],
        )
        .await
        .context("load stale branch candidates")?;

    let rows = rows
        .into_iter()
        .map(|row| {
            Ok(StaleBranchIndexRow {
                hash: row.get(0),
                prev_hash: row.get(1),
                height: row
                    .get::<_, Option<i32>>(2)
                    .context("stale branch candidate missing height")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(branch_components(rows)
        .into_iter()
        .map(|component| StaleBranchSummary {
            root_hash: component.root_hash,
            member_hashes: component.member_hashes,
            tip_hashes: component.tip_hashes,
            btc_height_min: component.key_min as i32,
            btc_height_max: component.key_max as i32,
            depth: component.depth,
        })
        .collect())
}

impl BranchRow for StaleBranchIndexRow {
    fn hash(&self) -> &[u8] {
        &self.hash
    }

    fn prev_hash(&self) -> &[u8] {
        &self.prev_hash
    }

    fn order_key(&self) -> i64 {
        i64::from(self.height)
    }
}

fn stale_branch_item(
    summary: &StaleBranchSummary,
    max_complete_height: Option<i32>,
    readiness: Option<&NavigationReadiness>,
) -> Result<NavigatorItem> {
    let root_hash = display_hash(&summary.root_hash)?;
    let tip_hashes = summary
        .tip_hashes
        .iter()
        .map(|hash| display_hash(hash))
        .collect::<Result<Vec<_>>>()?;
    let (navigation, view_error) = navigation_for_span(
        NavigationSpan {
            target_height: summary.btc_height_min,
            span_min: summary.btc_height_min,
            span_max: summary.btc_height_max,
            required_nodes: summary.depth,
        },
        root_hash.clone(),
        root_hash.clone(),
        max_complete_height,
        readiness,
    );
    let branch_id = format!("stale-{}-{}", summary.btc_height_min, root_hash);
    Ok(NavigatorItem {
        id: branch_id.clone(),
        kind: NavigatorTarget::StaleBranch.as_str(),
        primary_hash: root_hash.clone(),
        label: format!(
            "Stale branch #{}-#{}",
            summary.btc_height_min, summary.btc_height_max
        ),
        position: NavigatorPosition {
            axis: NavigatorAxis::Height,
            min: i64::from(summary.btc_height_min),
            max: i64::from(summary.btc_height_max),
        },
        cursor: NavigatorCursor::new(
            NavigatorTarget::StaleBranch,
            NavigatorAxis::Height,
            i64::from(summary.btc_height_min),
            i64::from(summary.btc_height_max),
            root_hash.clone(),
        )
        .encode(),
        index: 0,
        branch: Some(NavigatorBranch {
            branch_id,
            root_hash,
            tip_hashes,
            depth: summary.depth,
        }),
        orphan: None,
        view: navigation.map(NavigatorView::from),
        view_error,
    })
}

impl BranchNavigatorSummary for StaleBranchSummary {
    fn member_hashes(&self) -> &[Vec<u8>] {
        &self.member_hashes
    }

    fn matches_cursor(&self, cursor: &NavigatorCursor, edge: PageEdge) -> bool {
        stale_branch_matches_cursor(self, cursor, edge)
    }
}

fn sort_stale_branch_desc(a: &StaleBranchSummary, b: &StaleBranchSummary) -> std::cmp::Ordering {
    b.btc_height_max
        .cmp(&a.btc_height_max)
        .then_with(|| b.btc_height_min.cmp(&a.btc_height_min))
        .then_with(|| a.root_hash.cmp(&b.root_hash))
}

fn stale_branch_matches_cursor(
    summary: &StaleBranchSummary,
    cursor: &NavigatorCursor,
    edge: PageEdge,
) -> bool {
    let Ok(root_hash) = stored_hash_from_display(&cursor.hash) else {
        return false;
    };
    let max = i64::from(summary.btc_height_max);
    let min = i64::from(summary.btc_height_min);
    match edge {
        PageEdge::Older => {
            max < cursor.max
                || (max == cursor.max && min < cursor.min)
                || (max == cursor.max && min == cursor.min && summary.root_hash > root_hash)
        }
        PageEdge::Newer => {
            max > cursor.max
                || (max == cursor.max && min > cursor.min)
                || (max == cursor.max && min == cursor.min && summary.root_hash < root_hash)
        }
    }
}
