//! Shared newest-first page + rank helpers for the SQL-backed navigator targets.

use anyhow::Context;
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row};

use crate::query::{NavigatorDirection, NavigatorMode, NavigatorQuery};

use super::super::shared::stored_hash_from_display;
use super::{NavigatorItem, OrphanClassCounts, ProjectionError, is_newer_page};

/// One newest-first page and its rank: `newer_count` counts the eligible rows newer than the
/// page's newest row (none in latest mode), from the same statement as `total`.
pub(super) struct KeysetPage<T> {
    pub rows: Vec<T>,
    pub total: u64,
    pub newer_count: u64,
    pub facets: Option<OrphanClassCounts>,
}

impl<T> KeysetPage<T> {
    pub(super) fn first_index(&self) -> Option<u64> {
        (!self.rows.is_empty()).then_some(self.newer_count + 1)
    }

    /// Whether an eligible row sorts before the page's newest row.
    pub(super) fn exists_newer(&self) -> bool {
        self.newer_count > 0
    }

    /// Whether an eligible row sorts after the page's oldest row; a page is one contiguous run.
    pub(super) fn exists_older(&self) -> bool {
        self.total > self.newer_count + self.rows.len() as u64
    }
}

pub(super) fn assign_page_indexes(items: &mut [NavigatorItem], first: u64) {
    for (offset, item) in items.iter_mut().enumerate() {
        item.index = first + offset as u64;
    }
}

pub(super) fn apply_first_index(items: &mut [NavigatorItem], first: Option<u64>) {
    if let Some(first) = first {
        assign_page_indexes(items, first);
    }
}

pub(super) struct HeightRow {
    pub height: i32,
    pub tiebreak: i64,
    pub hash: Vec<u8>,
    pub orphan: bool,
    pub orphan_class: Option<String>,
}

pub(super) struct TimeRow {
    pub header_time: i64,
    pub hash: Vec<u8>,
    pub btc_orphan_class: Option<String>,
}

const TIME_NEWER_COUNT: &str = "(\
    SELECT count(*)::bigint FROM eligible e, boundary b \
    WHERE (e.axis, e.hash) > (b.axis, b.hash)\
)";

struct ScanSql {
    scan_where: String,
    scan_order: &'static str,
    newer_count: String,
}

const HEIGHT_NEWEST_FIRST: &str = "axis DESC, tiebreak DESC, hash ASC";
const HEIGHT_OLDEST_FIRST: &str = "axis ASC, tiebreak ASC, hash DESC";

/// `row`'s rows strictly older (`older`) or newer than `(axis, tiebreak, hash)` in height order,
/// where one height's rows go newest `tiebreak` first, then stored hash ascending.
fn height_edge_predicate(older: bool, row: &str, [axis, tiebreak, hash]: [&str; 3]) -> String {
    let (before, after) = if older { ("<", ">") } else { (">", "<") };
    format!(
        "{row}axis {before} {axis} OR ({row}axis = {axis} AND ({row}tiebreak {before} {tiebreak} \
         OR ({row}tiebreak = {tiebreak} AND {row}hash {after} {hash})))"
    )
}

fn height_scan_sql(mode: &NavigatorMode, base: usize) -> ScanSql {
    let cursor = [base + 2, base + 3, base + 4].map(|slot| format!("${slot}"));
    let newer_count = format!(
        "(SELECT count(*)::bigint FROM eligible e, boundary b WHERE {})",
        height_edge_predicate(false, "e.", ["b.axis", "b.tiebreak", "b.hash"])
    );
    match mode {
        NavigatorMode::Latest => ScanSql {
            scan_where: String::new(),
            scan_order: HEIGHT_NEWEST_FIRST,
            newer_count: "0::bigint".into(),
        },
        NavigatorMode::Page { direction, .. } => {
            let older = matches!(direction, NavigatorDirection::Older);
            let [axis, tiebreak, hash] = &cursor;
            ScanSql {
                scan_where: format!(
                    "WHERE {}",
                    height_edge_predicate(older, "", [axis, tiebreak, hash])
                ),
                scan_order: if older {
                    HEIGHT_NEWEST_FIRST
                } else {
                    HEIGHT_OLDEST_FIRST
                },
                newer_count,
            }
        }
        NavigatorMode::Anchor { .. } => ScanSql {
            scan_where: format!("WHERE hash = {}", cursor[0]),
            scan_order: HEIGHT_NEWEST_FIRST,
            newer_count,
        },
    }
}

fn time_scan_sql(mode: &NavigatorMode) -> ScanSql {
    match mode {
        NavigatorMode::Latest => ScanSql {
            scan_where: String::new(),
            scan_order: "axis DESC, hash DESC",
            newer_count: "0::bigint".into(),
        },
        NavigatorMode::Page {
            direction: NavigatorDirection::Older,
            ..
        } => ScanSql {
            scan_where: "WHERE (axis, hash) < ($4, $5)".into(),
            scan_order: "axis DESC, hash DESC",
            newer_count: TIME_NEWER_COUNT.into(),
        },
        NavigatorMode::Page {
            direction: NavigatorDirection::Newer,
            ..
        } => ScanSql {
            scan_where: "WHERE (axis, hash) > ($4, $5)".into(),
            scan_order: "axis ASC, hash ASC",
            newer_count: TIME_NEWER_COUNT.into(),
        },
        NavigatorMode::Anchor { .. } => ScanSql {
            scan_where: "WHERE hash = $4".into(),
            scan_order: "axis DESC, hash DESC",
            newer_count: TIME_NEWER_COUNT.into(),
        },
    }
}

fn height_keyset_sql(eligible: &str, base: usize, mode: &NavigatorMode) -> String {
    let scan = height_scan_sql(mode, base);
    let limit = base + 1;
    format!(
        "WITH eligible AS NOT MATERIALIZED ( {eligible} ), \
         limited_scan AS ( \
             SELECT axis, tiebreak, hash, orphan, orphan_class FROM eligible {scan_where} \
             ORDER BY {scan_order} LIMIT ${limit} \
         ), \
         scan AS ( \
             SELECT *, ROW_NUMBER() OVER (ORDER BY {scan_order}) AS scan_ord \
             FROM limited_scan \
         ), \
         boundary AS ( SELECT * FROM scan ORDER BY {HEIGHT_NEWEST_FIRST} LIMIT 1 ), \
         meta AS ( \
             SELECT (SELECT count(*)::bigint FROM eligible) AS total, \
                    {newer_count} AS newer_count \
         ) \
         SELECT meta.total, meta.newer_count, \
                r.axis, r.hash, r.tiebreak, r.orphan, r.orphan_class \
         FROM meta LEFT JOIN scan r ON TRUE \
         ORDER BY r.scan_ord NULLS FIRST",
        scan_where = scan.scan_where,
        scan_order = scan.scan_order,
        newer_count = scan.newer_count,
    )
}

fn time_keyset_sql(mode: &NavigatorMode) -> String {
    let scan = time_scan_sql(mode);
    format!(
        "WITH classes AS ( \
             SELECT btc_orphan_class, count(*)::bigint AS n \
             FROM block \
             WHERE kind = 'unknown' AND pow_validated \
             GROUP BY btc_orphan_class \
         ), \
         eligible AS NOT MATERIALIZED ( \
             SELECT b.btc_header_time AS axis, b.btc_header_hash AS hash, \
                    b.btc_orphan_class AS class \
             FROM block b \
             WHERE b.kind = 'unknown' AND b.pow_validated \
               AND (b.btc_orphan_class = ANY($2::text[]) \
                    OR ($3::boolean AND b.btc_orphan_class IS NULL)) \
         ), \
         limited_scan AS ( \
             SELECT axis, hash, class FROM eligible {scan_where} \
             ORDER BY {scan_order} LIMIT $1 \
         ), \
         scan AS ( \
             SELECT axis, hash, class, \
                    ROW_NUMBER() OVER (ORDER BY {scan_order}) AS scan_ord \
             FROM limited_scan \
         ), \
         boundary AS ( SELECT * FROM scan ORDER BY axis DESC, hash DESC LIMIT 1 ), \
         meta AS ( \
             SELECT (SELECT COALESCE(sum(n), 0)::bigint FROM classes \
                     WHERE btc_orphan_class = ANY($2::text[]) \
                        OR ($3::boolean AND btc_orphan_class IS NULL)) AS total, \
                    {newer_count} AS newer_count, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class = 'strict_btc_orphan'), 0) AS strict, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class = 'weak_btc_orphan'), 0) AS weak, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class = 'excluded'), 0) AS excluded, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class IS NULL), 0) AS pending \
         ) \
         SELECT meta.total, meta.newer_count, \
                meta.strict, meta.weak, meta.excluded, meta.pending, \
                r.axis, r.hash, r.class \
         FROM meta LEFT JOIN scan r ON TRUE \
         ORDER BY r.scan_ord NULLS FIRST",
        scan_where = scan.scan_where,
        scan_order = scan.scan_order,
        newer_count = scan.newer_count,
    )
}

/// Page a height-axis eligible set that selects `axis`, `tiebreak`, `hash`, `orphan` and
/// `orphan_class`, binding `eligible_params` to its `$1..`. One height's rows go newest
/// `tiebreak` first, then stored hash ascending; a target without a tiebreak selects a constant.
pub(super) async fn fetch_height_keyset(
    client: &Client,
    eligible: &str,
    eligible_params: &[&(dyn ToSql + Sync)],
    query: &NavigatorQuery,
) -> Result<KeysetPage<HeightRow>, ProjectionError> {
    let limit = query.limit as i64;
    let sql = height_keyset_sql(eligible, eligible_params.len(), &query.mode);
    let mut params = eligible_params.to_vec();
    params.push(&limit);
    let rows = match &query.mode {
        NavigatorMode::Latest => client.query(&sql, &params).await,
        NavigatorMode::Page { cursor, .. } => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            let height = cursor.max as i32;
            params.extend([&height as &(dyn ToSql + Sync), &cursor.tiebreak, &hash]);
            client.query(&sql, &params).await
        }
        NavigatorMode::Anchor { hash } => {
            let hash = stored_hash_from_display(hash)?;
            params.push(&hash);
            client.query(&sql, &params).await
        }
    }
    .context("load height navigator")?;
    let mut page = decode_height_page(rows);
    newest_first(&mut page, query);
    Ok(page)
}

fn decode_height_page(rows: Vec<Row>) -> KeysetPage<HeightRow> {
    let Some(head) = rows.first() else {
        return empty_page();
    };
    let mut items = Vec::new();
    for row in &rows {
        let Some(height) = row.get::<_, Option<i32>>(2) else {
            continue;
        };
        let Some(hash) = row.get::<_, Option<Vec<u8>>>(3) else {
            continue;
        };
        items.push(HeightRow {
            height,
            tiebreak: row.get(4),
            hash,
            orphan: row.get(5),
            orphan_class: row.get(6),
        });
    }
    page_from_head(head, items, None)
}

pub(super) async fn fetch_time_keyset(
    client: &Client,
    query: &NavigatorQuery,
    class_values: &[String],
    include_pending: bool,
) -> Result<KeysetPage<TimeRow>, ProjectionError> {
    let limit = query.limit as i64;
    let sql = time_keyset_sql(&query.mode);
    let rows = match &query.mode {
        NavigatorMode::Latest => {
            client
                .query(&sql, &[&limit, &class_values, &include_pending])
                .await
        }
        NavigatorMode::Page { cursor, .. } => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            client
                .query(
                    &sql,
                    &[&limit, &class_values, &include_pending, &cursor.max, &hash],
                )
                .await
        }
        NavigatorMode::Anchor { hash } => {
            let hash = stored_hash_from_display(hash)?;
            client
                .query(&sql, &[&limit, &class_values, &include_pending, &hash])
                .await
        }
    }
    .context("load orphan navigator")?;
    let mut page = decode_time_page(rows);
    newest_first(&mut page, query);
    Ok(page)
}

fn decode_time_page(rows: Vec<Row>) -> KeysetPage<TimeRow> {
    let Some(head) = rows.first() else {
        return empty_page();
    };
    let facets = Some(OrphanClassCounts {
        strict: head.get::<_, i64>(2).max(0) as u64,
        weak: head.get::<_, i64>(3).max(0) as u64,
        excluded: head.get::<_, i64>(4).max(0) as u64,
        pending: head.get::<_, i64>(5).max(0) as u64,
    });
    let mut items = Vec::new();
    for row in &rows {
        let Some(header_time) = row.get::<_, Option<i64>>(6) else {
            continue;
        };
        let Some(hash) = row.get::<_, Option<Vec<u8>>>(7) else {
            continue;
        };
        items.push(TimeRow {
            header_time,
            hash,
            btc_orphan_class: row.get(8),
        });
    }
    page_from_head(head, items, facets)
}

/// A newer page scans oldest first; present it newest first like every other page.
fn newest_first<T>(page: &mut KeysetPage<T>, query: &NavigatorQuery) {
    if is_newer_page(query) {
        page.rows.reverse();
    }
}

fn empty_page<T>() -> KeysetPage<T> {
    KeysetPage {
        rows: Vec::new(),
        total: 0,
        newer_count: 0,
        facets: None,
    }
}

fn page_from_head<T>(head: &Row, rows: Vec<T>, facets: Option<OrphanClassCounts>) -> KeysetPage<T> {
    KeysetPage {
        rows,
        total: head.get::<_, i64>(0).max(0) as u64,
        newer_count: head.get::<_, i64>(1).max(0) as u64,
        facets,
    }
}
