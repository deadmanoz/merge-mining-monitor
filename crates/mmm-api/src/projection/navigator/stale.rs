use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::query::{
    NavigatorAxis, NavigatorCursor, NavigatorMode, NavigatorQuery, NavigatorTarget,
};

use super::super::ProjectionError;
use super::super::branch_summary::{BranchRow, branch_components};
use super::super::shared::{
    display_hash, load_max_complete_canonical_height, stored_hash_from_display,
};
use super::super::stale_navigation::{
    NavigationReadiness, NavigationSpan, load_navigation_readiness, navigation_for_span,
};
use super::keyset::{apply_first_index, fetch_height_keyset};
use super::{
    BranchNavigatorSummary, NavigatorBranch, NavigatorFacets, NavigatorItem, NavigatorPayload,
    NavigatorPosition, NavigatorView, PageEdge, is_newer_page, page_branch_summaries, page_cursors,
};

#[derive(Debug, Clone)]
struct StaleBlockRow {
    height: i32,
    hash: String,
}

const STALE_ELIGIBLE: &str = "SELECT stale.btc_height AS axis, stale.btc_header_hash AS hash \
     FROM block stale \
     JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
     WHERE stale.kind = 'stale' AND canonical.kind = 'canonical'";

pub async fn stale_blocks(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::Stale);
    let max_complete_height = load_max_complete_canonical_height(client).await?;
    let mut page = fetch_height_keyset(client, STALE_ELIGIBLE, query).await?;
    if is_newer_page(query) {
        page.rows.reverse();
    }
    let rows = page
        .rows
        .iter()
        .map(|row| {
            Ok(StaleBlockRow {
                height: row.height,
                hash: display_hash(&row.hash)?,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let spans = rows
        .iter()
        .map(|row| NavigationSpan {
            target_height: row.height,
            span_min: row.height,
            span_max: row.height,
            required_nodes: 1,
        })
        .collect::<Vec<_>>();
    let readiness = load_navigation_readiness(client, max_complete_height, &spans).await?;
    let mut items = rows
        .iter()
        .zip(spans.iter().copied())
        .map(|(row, span)| stale_block_item(row, span, max_complete_height, readiness.as_ref()))
        .collect::<Vec<_>>();
    apply_first_index(
        &mut items,
        page.first_index(matches!(query.mode, NavigatorMode::Latest)),
    );

    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::Stale,
        query,
        &items,
        page.has_more_scan,
        |cursor| exists_stale_block_across_edge(client, cursor, PageEdge::Older),
        |cursor| exists_stale_block_across_edge(client, cursor, PageEdge::Newer),
    )
    .await?;

    Ok(NavigatorPayload::new(
        NavigatorTarget::Stale,
        items,
        page.total,
        NavigatorFacets::default(),
        next_cursor,
        prev_cursor,
    ))
}

fn stale_block_item(
    row: &StaleBlockRow,
    span: NavigationSpan,
    max_complete_height: Option<i32>,
    readiness: Option<&NavigationReadiness>,
) -> NavigatorItem {
    let (navigation, view_error) = navigation_for_span(
        span,
        row.hash.clone(),
        row.hash.clone(),
        max_complete_height,
        readiness,
    );
    NavigatorItem {
        id: format!("stale-{}", row.hash),
        kind: NavigatorTarget::Stale.as_str(),
        primary_hash: row.hash.clone(),
        label: format!("Stale #{}", row.height),
        position: NavigatorPosition {
            axis: NavigatorAxis::Height,
            min: i64::from(row.height),
            max: i64::from(row.height),
        },
        index: 0,
        cursor: NavigatorCursor::new(
            NavigatorTarget::Stale,
            NavigatorAxis::Height,
            i64::from(row.height),
            i64::from(row.height),
            row.hash.clone(),
        )
        .encode(),
        branch: None,
        orphan: None,
        view: navigation.map(NavigatorView::from),
        view_error,
    }
}

async fn exists_stale_block_across_edge(
    client: &Client,
    cursor: NavigatorCursor,
    edge: PageEdge,
) -> Result<bool, ProjectionError> {
    let hash = stored_hash_from_display(&cursor.hash)?;
    let height = cursor.max as i32;
    let sql = match edge {
        PageEdge::Older => {
            "SELECT EXISTS ( \
                 SELECT 1 \
                 FROM block stale \
                 JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                 WHERE stale.kind = 'stale' \
                   AND canonical.kind = 'canonical' \
                   AND (stale.btc_height < $1 \
                        OR (stale.btc_height = $1 AND stale.btc_header_hash > $2)) \
             )"
        }
        PageEdge::Newer => {
            "SELECT EXISTS ( \
                 SELECT 1 \
                 FROM block stale \
                 JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                 WHERE stale.kind = 'stale' \
                   AND canonical.kind = 'canonical' \
                   AND (stale.btc_height > $1 \
                        OR (stale.btc_height = $1 AND stale.btc_header_hash < $2)) \
             )"
        }
    };
    let row = client
        .query_one(sql, &[&height, &hash])
        .await
        .context("probe stale navigator edge")?;
    Ok(row.get(0))
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
    apply_first_index(
        &mut items,
        page.rows.first().map(|_| page.start_offset as u64 + 1),
    );

    let older_summaries = &summaries;
    let newer_summaries = &summaries;
    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::StaleBranch,
        query,
        &items,
        page.has_more_scan,
        |cursor| async move { stale_branch_exists(older_summaries, &cursor, PageEdge::Older) },
        |cursor| async move { stale_branch_exists(newer_summaries, &cursor, PageEdge::Newer) },
    )
    .await?;

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

fn stale_branch_exists(
    summaries: &[StaleBranchSummary],
    cursor: &NavigatorCursor,
    edge: PageEdge,
) -> Result<bool, ProjectionError> {
    stored_hash_from_display(&cursor.hash)?;
    Ok(summaries
        .iter()
        .any(|summary| stale_branch_matches_cursor(summary, cursor, edge)))
}
