use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::normalize::Classification;
use crate::query::{NavigatorAxis, NavigatorCursor, NavigatorQuery, NavigatorTarget};

use super::super::ProjectionError;
use super::super::branch_summary::{BranchRow, branch_components};
use super::super::shared::{classification_filter_params, display_hash, stored_hash_from_display};
use super::keyset::{apply_first_index, fetch_time_keyset};
use super::{
    BranchNavigatorSummary, NavigatorBranch, NavigatorFacets, NavigatorItem, NavigatorOrphan,
    NavigatorPayload, NavigatorPosition, NavigatorView, PageEdge, page_branch_summaries,
    page_cursors,
};

#[derive(Debug, Clone)]
struct OrphanRow {
    header_time: i64,
    hash: String,
    btc_orphan_class: Option<String>,
}

pub async fn orphans(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::Orphan);
    let (class_values, include_pending) = classification_filter_params(&query.classification);
    let page = fetch_time_keyset(client, query, &class_values, include_pending).await?;
    let rows = page
        .rows
        .iter()
        .map(|row| {
            Ok(OrphanRow {
                header_time: row.header_time,
                hash: display_hash(&row.hash)?,
                btc_orphan_class: row.btc_orphan_class.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut items = rows.iter().map(orphan_item).collect::<Vec<_>>();
    apply_first_index(&mut items, page.first_index());
    let counts = page.facets.clone().unwrap_or_default();
    let (next_cursor, prev_cursor) = page_cursors(&items, page.exists_older(), page.exists_newer());

    Ok(NavigatorPayload::new(
        NavigatorTarget::Orphan,
        items,
        page.total,
        NavigatorFacets {
            orphan_classes: Some(counts),
        },
        next_cursor,
        prev_cursor,
    ))
}

fn orphan_item(row: &OrphanRow) -> NavigatorItem {
    NavigatorItem {
        id: format!("orphan-{}", row.hash),
        kind: NavigatorTarget::Orphan.as_str(),
        primary_hash: row.hash.clone(),
        label: format!("Orphan {}", row.header_time),
        position: NavigatorPosition {
            axis: NavigatorAxis::Time,
            min: row.header_time,
            max: row.header_time,
        },
        cursor: NavigatorCursor::new(
            NavigatorTarget::Orphan,
            NavigatorAxis::Time,
            row.header_time,
            row.header_time,
            row.hash.clone(),
        )
        .encode(),
        index: 0,
        branch: None,
        orphan: Some(NavigatorOrphan {
            btc_orphan_class: row.btc_orphan_class.clone(),
            btc_header_time: row.header_time,
        }),
        view: Some(NavigatorView::UnheightedAnchor {
            anchor_hash: row.hash.clone(),
            select_hash: row.hash.clone(),
            center_hash: row.hash.clone(),
        }),
        view_error: None,
    }
}

#[derive(Debug, Clone)]
struct OrphanBranchIndexRow {
    hash: Vec<u8>,
    prev_hash: Vec<u8>,
    header_time: i64,
}

#[derive(Debug, Clone)]
struct OrphanBranchSummary {
    root_hash: Vec<u8>,
    member_hashes: Vec<Vec<u8>>,
    tip_hashes: Vec<Vec<u8>>,
    header_time_min: i64,
    header_time_max: i64,
    depth: usize,
}

pub async fn orphan_branches(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::OrphanBranch);
    let mut summaries = load_orphan_branch_summaries(client, &query.classification).await?;
    summaries.sort_by(sort_orphan_branch_desc);
    let total = summaries.len() as u64;

    let page = page_branch_summaries(&summaries, query)?;
    let mut items = page
        .rows
        .iter()
        .map(orphan_branch_item)
        .collect::<Result<Vec<_>>>()?;
    apply_first_index(&mut items, page.first_index());
    let (next_cursor, prev_cursor) = page_cursors(&items, page.exists_older, page.exists_newer);

    Ok(NavigatorPayload::new(
        NavigatorTarget::OrphanBranch,
        items,
        total,
        NavigatorFacets::default(),
        next_cursor,
        prev_cursor,
    ))
}

async fn load_orphan_branch_summaries(
    client: &Client,
    classification: &[Classification],
) -> Result<Vec<OrphanBranchSummary>, ProjectionError> {
    let (class_values, include_pending) = classification_filter_params(classification);
    let rows = client
        .query(
            "WITH cand AS ( \
                 SELECT btc_header_hash, btc_prev_header_hash, btc_header_time \
                 FROM block \
                 WHERE kind = 'unknown' AND pow_validated \
                   AND ( btc_orphan_class = ANY($1::text[]) \
                         OR ($2::boolean AND btc_orphan_class IS NULL) ) \
             ) \
             SELECT c.btc_header_hash, c.btc_prev_header_hash, c.btc_header_time \
             FROM cand c \
             WHERE EXISTS (SELECT 1 FROM cand p WHERE p.btc_header_hash = c.btc_prev_header_hash) \
                OR EXISTS (SELECT 1 FROM cand ch WHERE ch.btc_prev_header_hash = c.btc_header_hash)",
            &[&class_values, &include_pending],
        )
        .await
        .context("load orphan branch candidates")?;
    let rows = rows
        .into_iter()
        .map(|row| OrphanBranchIndexRow {
            hash: row.get(0),
            prev_hash: row.get(1),
            header_time: row.get(2),
        })
        .collect::<Vec<_>>();

    Ok(branch_components(rows)
        .into_iter()
        .map(|component| OrphanBranchSummary {
            root_hash: component.root_hash,
            member_hashes: component.member_hashes,
            tip_hashes: component.tip_hashes,
            header_time_min: component.key_min,
            header_time_max: component.key_max,
            depth: component.depth,
        })
        .collect())
}

impl BranchRow for OrphanBranchIndexRow {
    fn hash(&self) -> &[u8] {
        &self.hash
    }

    fn prev_hash(&self) -> &[u8] {
        &self.prev_hash
    }

    fn order_key(&self) -> i64 {
        self.header_time
    }
}

fn orphan_branch_item(summary: &OrphanBranchSummary) -> Result<NavigatorItem> {
    let root_hash = display_hash(&summary.root_hash)?;
    let tip_hashes = summary
        .tip_hashes
        .iter()
        .map(|hash| display_hash(hash))
        .collect::<Result<Vec<_>>>()?;
    let branch_id = format!("orphan-{root_hash}");
    let cursor = NavigatorCursor::new(
        NavigatorTarget::OrphanBranch,
        NavigatorAxis::Time,
        summary.header_time_min,
        summary.header_time_max,
        root_hash.clone(),
    )
    .encode();
    Ok(NavigatorItem {
        id: branch_id.clone(),
        kind: NavigatorTarget::OrphanBranch.as_str(),
        primary_hash: root_hash.clone(),
        label: format!("Orphan branch {}", summary.header_time_max),
        position: NavigatorPosition {
            axis: NavigatorAxis::Time,
            min: summary.header_time_min,
            max: summary.header_time_max,
        },
        index: 0,
        cursor,
        branch: Some(NavigatorBranch {
            branch_id,
            root_hash: root_hash.clone(),
            tip_hashes,
            depth: summary.depth,
        }),
        orphan: None,
        view: Some(NavigatorView::UnheightedAnchor {
            anchor_hash: root_hash.clone(),
            select_hash: root_hash.clone(),
            center_hash: root_hash,
        }),
        view_error: None,
    })
}

impl BranchNavigatorSummary for OrphanBranchSummary {
    fn member_hashes(&self) -> &[Vec<u8>] {
        &self.member_hashes
    }

    fn matches_cursor(&self, cursor: &NavigatorCursor, edge: PageEdge) -> bool {
        orphan_branch_matches_cursor(self, cursor, edge)
    }
}

fn sort_orphan_branch_desc(a: &OrphanBranchSummary, b: &OrphanBranchSummary) -> std::cmp::Ordering {
    b.header_time_max
        .cmp(&a.header_time_max)
        .then_with(|| b.header_time_min.cmp(&a.header_time_min))
        .then_with(|| a.root_hash.cmp(&b.root_hash))
}

fn orphan_branch_matches_cursor(
    summary: &OrphanBranchSummary,
    cursor: &NavigatorCursor,
    edge: PageEdge,
) -> bool {
    let Ok(root_hash) = stored_hash_from_display(&cursor.hash) else {
        return false;
    };
    match edge {
        PageEdge::Older => {
            summary.header_time_max < cursor.max
                || (summary.header_time_max == cursor.max && summary.header_time_min < cursor.min)
                || (summary.header_time_max == cursor.max
                    && summary.header_time_min == cursor.min
                    && summary.root_hash > root_hash)
        }
        PageEdge::Newer => {
            summary.header_time_max > cursor.max
                || (summary.header_time_max == cursor.max && summary.header_time_min > cursor.min)
                || (summary.header_time_max == cursor.max
                    && summary.header_time_min == cursor.min
                    && summary.root_hash < root_hash)
        }
    }
}
