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
use super::{
    BranchNavigatorSummary, NavigatorBranch, NavigatorFacets, NavigatorItem, NavigatorPayload,
    NavigatorPosition, NavigatorView, PageEdge, anchor_hash, cursor_params, is_newer_page,
    page_branch_summaries, page_cursors,
};
use crate::query::NavigatorMode;

#[derive(Debug, Clone)]
struct StaleBlockRow {
    height: i32,
    hash: String,
}

pub async fn stale_blocks(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::Stale);
    let max_complete_height = load_max_complete_canonical_height(client).await?;
    let total = load_stales_total(client).await?;
    let fetch_limit = (query.limit + 1) as i64;
    let mut rows = fetch_stale_blocks(client, query, fetch_limit).await?;
    let has_more_scan = rows.len() > query.limit;
    rows.truncate(query.limit);
    if is_newer_page(query) {
        rows.reverse();
    }

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
    let items = rows
        .iter()
        .zip(spans.iter().copied())
        .map(|(row, span)| stale_block_item(row, span, max_complete_height, readiness.as_ref()))
        .collect::<Vec<_>>();

    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::Stale,
        query,
        &items,
        has_more_scan,
        |cursor| exists_stale_block_across_edge(client, cursor, PageEdge::Older),
        |cursor| exists_stale_block_across_edge(client, cursor, PageEdge::Newer),
    )
    .await?;

    Ok(NavigatorPayload::new(
        NavigatorTarget::Stale,
        items,
        total,
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

async fn load_stales_total(client: &Client) -> Result<u64, ProjectionError> {
    let row = client
        .query_one(
            "SELECT count(*)::bigint \
             FROM block stale \
             JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
             WHERE stale.kind = 'stale' \
               AND canonical.kind = 'canonical'",
            &[],
        )
        .await
        .context("count stale navigator")?;
    Ok(row.get::<_, i64>(0).max(0) as u64)
}

async fn fetch_stale_blocks(
    client: &Client,
    query: &NavigatorQuery,
    fetch_limit: i64,
) -> Result<Vec<StaleBlockRow>, ProjectionError> {
    let rows = match (&query.mode, cursor_params(query), anchor_hash(query)) {
        (NavigatorMode::Anchor { hash }, _, _) => {
            let hash = stored_hash_from_display(hash)?;
            client
                .query(
                    "SELECT stale.btc_height, stale.btc_header_hash \
                     FROM block stale \
                     JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                     WHERE stale.kind = 'stale' \
                       AND canonical.kind = 'canonical' \
                       AND stale.btc_header_hash = $1",
                    &[&hash],
                )
                .await
        }
        (_, Some((PageEdge::Older, cursor)), _) => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            client
                .query(
                    "SELECT stale.btc_height, stale.btc_header_hash \
                     FROM block stale \
                     JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                     WHERE stale.kind = 'stale' \
                       AND canonical.kind = 'canonical' \
                       AND (stale.btc_height < $2 \
                            OR (stale.btc_height = $2 AND stale.btc_header_hash > $3)) \
                     ORDER BY stale.btc_height DESC, stale.btc_header_hash ASC \
                     LIMIT $1",
                    &[&fetch_limit, &(cursor.max as i32), &hash],
                )
                .await
        }
        (_, Some((PageEdge::Newer, cursor)), _) => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            client
                .query(
                    "SELECT stale.btc_height, stale.btc_header_hash \
                     FROM block stale \
                     JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                     WHERE stale.kind = 'stale' \
                       AND canonical.kind = 'canonical' \
                       AND (stale.btc_height > $2 \
                            OR (stale.btc_height = $2 AND stale.btc_header_hash < $3)) \
                     ORDER BY stale.btc_height ASC, stale.btc_header_hash DESC \
                     LIMIT $1",
                    &[&fetch_limit, &(cursor.max as i32), &hash],
                )
                .await
        }
        _ => {
            client
                .query(
                    "SELECT stale.btc_height, stale.btc_header_hash \
                     FROM block stale \
                     JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
                     WHERE stale.kind = 'stale' \
                       AND canonical.kind = 'canonical' \
                     ORDER BY stale.btc_height DESC, stale.btc_header_hash ASC \
                     LIMIT $1",
                    &[&fetch_limit],
                )
                .await
        }
    }
    .context("load stale navigator")?;

    rows.into_iter()
        .map(|row| {
            let hash_bytes = row.get::<_, Vec<u8>>(1);
            Ok(StaleBlockRow {
                height: row.get(0),
                hash: display_hash(&hash_bytes)?,
            })
        })
        .collect::<Result<Vec<_>>>()
        .map_err(ProjectionError::from)
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

    let mut page = page_branch_summaries(&summaries, query)?;
    let has_more_scan = page.len() > query.limit;
    page.truncate(query.limit);
    if is_newer_page(query) {
        page.reverse();
    }

    let spans = page
        .iter()
        .map(|summary| NavigationSpan {
            target_height: summary.btc_height_min,
            span_min: summary.btc_height_min,
            span_max: summary.btc_height_max,
            required_nodes: summary.depth,
        })
        .collect::<Vec<_>>();
    let readiness = load_navigation_readiness(client, max_complete_height, &spans).await?;
    let items = page
        .iter()
        .map(|summary| stale_branch_item(summary, max_complete_height, readiness.as_ref()))
        .collect::<Result<Vec<_>>>()?;

    let older_summaries = &summaries;
    let newer_summaries = &summaries;
    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::StaleBranch,
        query,
        &items,
        has_more_scan,
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
