use anyhow::{Context, Result};
use tokio_postgres::{Client, Row};

use crate::normalize::Classification;
use crate::query::{
    NavigatorAxis, NavigatorCursor, NavigatorMode, NavigatorQuery, NavigatorTarget,
};

use super::super::ProjectionError;
use super::super::branch_summary::{BranchRow, branch_components};
use super::super::shared::{classification_filter_params, display_hash, stored_hash_from_display};
use super::{
    BranchNavigatorSummary, NavigatorBranch, NavigatorFacets, NavigatorItem, NavigatorOrphan,
    NavigatorPayload, NavigatorPosition, NavigatorView, OrphanClassCounts, PageEdge, anchor_hash,
    cursor_params, is_newer_page, page_branch_summaries, page_cursors,
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
    let counts = load_orphan_class_counts(client).await?;
    let total = total_for_filter(&counts, &query.classification);
    let fetch_limit = (query.limit + 1) as i64;
    let mut rows =
        fetch_orphans(client, query, fetch_limit, &class_values, include_pending).await?;
    let has_more_scan = rows.len() > query.limit;
    rows.truncate(query.limit);
    if is_newer_page(query) {
        rows.reverse();
    }

    let items = rows.iter().map(orphan_item).collect::<Vec<_>>();
    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::Orphan,
        query,
        &items,
        has_more_scan,
        |cursor| {
            exists_orphan_across_edge(
                client,
                cursor,
                &class_values,
                include_pending,
                PageEdge::Older,
            )
        },
        |cursor| {
            exists_orphan_across_edge(
                client,
                cursor,
                &class_values,
                include_pending,
                PageEdge::Newer,
            )
        },
    )
    .await?;

    Ok(NavigatorPayload::new(
        NavigatorTarget::Orphan,
        items,
        total,
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
        branch: None,
        orphan: Some(NavigatorOrphan {
            btc_orphan_class: row.btc_orphan_class.clone(),
        }),
        view: Some(NavigatorView::UnheightedAnchor {
            anchor_hash: row.hash.clone(),
            select_hash: row.hash.clone(),
            center_hash: row.hash.clone(),
        }),
        view_error: None,
    }
}

async fn fetch_orphans(
    client: &Client,
    query: &NavigatorQuery,
    fetch_limit: i64,
    class_values: &[String],
    include_pending: bool,
) -> Result<Vec<OrphanRow>, ProjectionError> {
    let rows = match (&query.mode, cursor_params(query), anchor_hash(query)) {
        (NavigatorMode::Anchor { hash }, _, _) => {
            let hash = stored_hash_from_display(hash)?;
            client
                .query(
                    "SELECT b.btc_header_time, b.btc_header_hash, b.btc_orphan_class \
                     FROM block b \
                     WHERE b.kind = 'unknown' AND b.pow_validated \
                       AND ( \
                           b.btc_orphan_class = ANY($2::text[]) \
                           OR ($3::boolean AND b.btc_orphan_class IS NULL) \
                       ) \
                       AND b.btc_header_hash = $1",
                    &[&hash, &class_values, &include_pending],
                )
                .await
                .context("load orphan navigator")?
        }
        (_, Some((edge, cursor)), _) => {
            fetch_orphan_cursor_rows(
                client,
                fetch_limit,
                cursor,
                class_values,
                include_pending,
                edge,
            )
            .await?
        }
        _ => client
            .query(
                "SELECT b.btc_header_time, b.btc_header_hash, b.btc_orphan_class \
                     FROM block b \
                     WHERE b.kind = 'unknown' AND b.pow_validated \
                       AND ( \
                           b.btc_orphan_class = ANY($2::text[]) \
                           OR ($3::boolean AND b.btc_orphan_class IS NULL) \
                       ) \
                     ORDER BY b.btc_header_time DESC, b.btc_header_hash DESC \
                     LIMIT $1",
                &[&fetch_limit, &class_values, &include_pending],
            )
            .await
            .context("load orphan navigator")?,
    };

    rows.into_iter()
        .map(|row| {
            Ok(OrphanRow {
                header_time: row.get(0),
                hash: display_hash(&row.get::<_, Vec<u8>>(1))?,
                btc_orphan_class: row.get(2),
            })
        })
        .collect::<Result<Vec<_>>>()
        .map_err(ProjectionError::from)
}

async fn fetch_orphan_cursor_rows(
    client: &Client,
    fetch_limit: i64,
    cursor: &NavigatorCursor,
    class_values: &[String],
    include_pending: bool,
    edge: PageEdge,
) -> Result<Vec<Row>, ProjectionError> {
    let hash = stored_hash_from_display(&cursor.hash)?;
    let (comparison, order) = match edge {
        PageEdge::Older => ("<", "DESC"),
        PageEdge::Newer => (">", "ASC"),
    };
    let sql = format!(
        "SELECT b.btc_header_time, b.btc_header_hash, b.btc_orphan_class \
         FROM block b \
         WHERE b.kind = 'unknown' AND b.pow_validated \
           AND ( \
               b.btc_orphan_class = ANY($4::text[]) \
               OR ($5::boolean AND b.btc_orphan_class IS NULL) \
           ) \
           AND (b.btc_header_time, b.btc_header_hash) {comparison} ($2, $3) \
         ORDER BY b.btc_header_time {order}, b.btc_header_hash {order} \
         LIMIT $1"
    );
    client
        .query(
            &sql,
            &[
                &fetch_limit,
                &cursor.max,
                &hash,
                &class_values,
                &include_pending,
            ],
        )
        .await
        .context("load orphan navigator")
        .map_err(ProjectionError::from)
}

async fn load_orphan_class_counts(client: &Client) -> Result<OrphanClassCounts, ProjectionError> {
    let rows = client
        .query(
            "SELECT btc_orphan_class, count(*)::bigint FROM block \
             WHERE kind = 'unknown' AND pow_validated \
             GROUP BY btc_orphan_class",
            &[],
        )
        .await
        .context("count orphan classes")?;
    let mut counts = OrphanClassCounts {
        strict: 0,
        weak: 0,
        excluded: 0,
        pending: 0,
    };
    for row in rows {
        let class: Option<String> = row.get(0);
        let n = row.get::<_, i64>(1).max(0) as u64;
        match class.as_deref() {
            Some("strict_btc_orphan") => counts.strict = n,
            Some("weak_btc_orphan") => counts.weak = n,
            Some("btc_stale_excluded") => counts.excluded = n,
            None => counts.pending = n,
            Some(_) => {}
        }
    }
    Ok(counts)
}

fn total_for_filter(counts: &OrphanClassCounts, classification: &[Classification]) -> u64 {
    classification
        .iter()
        .map(|class| match class {
            Classification::StrictBtcOrphan => counts.strict,
            Classification::WeakBtcOrphan => counts.weak,
            Classification::BtcStaleExcluded => counts.excluded,
            Classification::Pending => counts.pending,
        })
        .sum()
}

async fn exists_orphan_across_edge(
    client: &Client,
    cursor: NavigatorCursor,
    class_values: &[String],
    include_pending: bool,
    edge: PageEdge,
) -> Result<bool, ProjectionError> {
    let hash = stored_hash_from_display(&cursor.hash)?;
    let time = cursor.max;
    let sql = match edge {
        PageEdge::Older => {
            "SELECT EXISTS (SELECT 1 FROM block \
             WHERE kind = 'unknown' AND pow_validated \
               AND ( \
                   btc_orphan_class = ANY($3::text[]) \
                   OR ($4::boolean AND btc_orphan_class IS NULL) \
               ) \
               AND (btc_header_time, btc_header_hash) < ($1, $2))"
        }
        PageEdge::Newer => {
            "SELECT EXISTS (SELECT 1 FROM block \
             WHERE kind = 'unknown' AND pow_validated \
               AND ( \
                   btc_orphan_class = ANY($3::text[]) \
                   OR ($4::boolean AND btc_orphan_class IS NULL) \
               ) \
               AND (btc_header_time, btc_header_hash) > ($1, $2))"
        }
    };
    let row = client
        .query_one(sql, &[&time, &hash, &class_values, &include_pending])
        .await
        .context("probe orphan navigator edge")?;
    Ok(row.get(0))
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

    let mut page = page_branch_summaries(&summaries, query)?;
    let has_more_scan = page.len() > query.limit;
    page.truncate(query.limit);
    if is_newer_page(query) {
        page.reverse();
    }

    let items = page
        .iter()
        .map(orphan_branch_item)
        .collect::<Result<Vec<_>>>()?;
    let older_summaries = &summaries;
    let newer_summaries = &summaries;
    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::OrphanBranch,
        query,
        &items,
        has_more_scan,
        |cursor| async move { orphan_branch_exists(older_summaries, &cursor, PageEdge::Older) },
        |cursor| async move { orphan_branch_exists(newer_summaries, &cursor, PageEdge::Newer) },
    )
    .await?;

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

fn orphan_branch_exists(
    summaries: &[OrphanBranchSummary],
    cursor: &NavigatorCursor,
    edge: PageEdge,
) -> Result<bool, ProjectionError> {
    stored_hash_from_display(&cursor.hash)?;
    Ok(summaries
        .iter()
        .any(|summary| orphan_branch_matches_cursor(summary, cursor, edge)))
}
