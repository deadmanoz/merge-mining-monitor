//! Shared newest-first page + rank helpers for the SQL-backed navigator targets.

use anyhow::Context;
use tokio_postgres::{Client, Row};

use crate::query::{NavigatorDirection, NavigatorMode, NavigatorQuery};

use super::super::shared::stored_hash_from_display;
use super::{NavigatorItem, OrphanClassCounts, ProjectionError};

pub(super) struct KeysetPage<T> {
    pub rows: Vec<T>,
    pub total: u64,
    pub newer_count: u64,
    pub has_more_scan: bool,
    pub facets: Option<OrphanClassCounts>,
}

impl<T> KeysetPage<T> {
    pub(super) fn first_index(&self, latest: bool) -> Option<u64> {
        if self.rows.is_empty() {
            None
        } else if latest {
            Some(1)
        } else {
            Some(self.newer_count + 1)
        }
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
    pub hash: Vec<u8>,
}

pub(super) struct TimeRow {
    pub header_time: i64,
    pub hash: Vec<u8>,
    pub btc_orphan_class: Option<String>,
}

const HEIGHT_NEWER_COUNT: &str = "(\
    SELECT count(*)::bigint FROM eligible e, boundary b \
    WHERE e.axis > b.axis OR (e.axis = b.axis AND e.hash < b.hash)\
)";

const TIME_NEWER_COUNT: &str = "(\
    SELECT count(*)::bigint FROM eligible e, boundary b \
    WHERE (e.axis, e.hash) > (b.axis, b.hash)\
)";

struct ScanSql {
    scan_where: &'static str,
    scan_order: &'static str,
    newer_count: &'static str,
}

fn height_scan_sql(mode: &NavigatorMode) -> ScanSql {
    match mode {
        NavigatorMode::Latest => ScanSql {
            scan_where: "",
            scan_order: "axis DESC, hash ASC",
            newer_count: "0::bigint",
        },
        NavigatorMode::Page {
            direction: NavigatorDirection::Older,
            ..
        } => ScanSql {
            scan_where: "WHERE axis < $3 OR (axis = $3 AND hash > $4)",
            scan_order: "axis DESC, hash ASC",
            newer_count: HEIGHT_NEWER_COUNT,
        },
        NavigatorMode::Page {
            direction: NavigatorDirection::Newer,
            ..
        } => ScanSql {
            scan_where: "WHERE axis > $3 OR (axis = $3 AND hash < $4)",
            scan_order: "axis ASC, hash DESC",
            newer_count: HEIGHT_NEWER_COUNT,
        },
        NavigatorMode::Anchor { .. } => ScanSql {
            scan_where: "WHERE hash = $3",
            scan_order: "axis DESC, hash ASC",
            newer_count: HEIGHT_NEWER_COUNT,
        },
    }
}

fn time_scan_sql(mode: &NavigatorMode) -> ScanSql {
    match mode {
        NavigatorMode::Latest => ScanSql {
            scan_where: "",
            scan_order: "axis DESC, hash DESC",
            newer_count: "0::bigint",
        },
        NavigatorMode::Page {
            direction: NavigatorDirection::Older,
            ..
        } => ScanSql {
            scan_where: "WHERE (axis, hash) < ($5, $6)",
            scan_order: "axis DESC, hash DESC",
            newer_count: TIME_NEWER_COUNT,
        },
        NavigatorMode::Page {
            direction: NavigatorDirection::Newer,
            ..
        } => ScanSql {
            scan_where: "WHERE (axis, hash) > ($5, $6)",
            scan_order: "axis ASC, hash ASC",
            newer_count: TIME_NEWER_COUNT,
        },
        NavigatorMode::Anchor { .. } => ScanSql {
            scan_where: "WHERE hash = $5",
            scan_order: "axis DESC, hash DESC",
            newer_count: TIME_NEWER_COUNT,
        },
    }
}

fn height_keyset_sql(eligible: &str, mode: &NavigatorMode) -> String {
    let scan = height_scan_sql(mode);
    format!(
        "WITH eligible AS NOT MATERIALIZED ( {eligible} ), \
         limited_scan AS ( \
             SELECT axis, hash FROM eligible {scan_where} \
             ORDER BY {scan_order} LIMIT $1 \
         ), \
         scan AS ( \
             SELECT axis, hash, ROW_NUMBER() OVER (ORDER BY {scan_order}) AS scan_ord \
             FROM limited_scan \
         ), \
         retained AS ( SELECT * FROM scan WHERE scan_ord <= $2 ), \
         boundary AS ( SELECT * FROM retained ORDER BY axis DESC, hash ASC LIMIT 1 ), \
         meta AS ( \
             SELECT (SELECT count(*)::bigint FROM eligible) AS total, \
                    {newer_count} AS newer_count, \
                    (SELECT count(*) FROM scan) > $2 AS has_more_scan \
         ) \
         SELECT meta.total, meta.newer_count, meta.has_more_scan, \
                r.axis, r.hash \
         FROM meta LEFT JOIN retained r ON TRUE \
         ORDER BY r.scan_ord NULLS FIRST",
        eligible = eligible,
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
               AND (b.btc_orphan_class = ANY($3::text[]) \
                    OR ($4::boolean AND b.btc_orphan_class IS NULL)) \
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
         retained AS ( SELECT * FROM scan WHERE scan_ord <= $2 ), \
         boundary AS ( SELECT * FROM retained ORDER BY axis DESC, hash DESC LIMIT 1 ), \
         meta AS ( \
             SELECT (SELECT COALESCE(sum(n), 0)::bigint FROM classes \
                     WHERE btc_orphan_class = ANY($3::text[]) \
                        OR ($4::boolean AND btc_orphan_class IS NULL)) AS total, \
                    {newer_count} AS newer_count, \
                    (SELECT count(*) FROM scan) > $2 AS has_more_scan, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class = 'strict_btc_orphan'), 0) AS strict, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class = 'weak_btc_orphan'), 0) AS weak, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class = 'excluded'), 0) AS excluded, \
                    COALESCE((SELECT n FROM classes WHERE btc_orphan_class IS NULL), 0) AS pending \
         ) \
         SELECT meta.total, meta.newer_count, meta.has_more_scan, \
                meta.strict, meta.weak, meta.excluded, meta.pending, \
                r.axis, r.hash, r.class \
         FROM meta LEFT JOIN retained r ON TRUE \
         ORDER BY r.scan_ord NULLS FIRST",
        scan_where = scan.scan_where,
        scan_order = scan.scan_order,
        newer_count = scan.newer_count,
    )
}

pub(super) async fn fetch_height_keyset(
    client: &Client,
    eligible: &str,
    query: &NavigatorQuery,
) -> Result<KeysetPage<HeightRow>, ProjectionError> {
    let fetch_limit = (query.limit + 1) as i64;
    let limit = query.limit as i64;
    let sql = height_keyset_sql(eligible, &query.mode);
    let rows = query_height_rows(client, &sql, query, fetch_limit, limit).await?;
    decode_height_page(rows)
}

async fn query_height_rows(
    client: &Client,
    sql: &str,
    query: &NavigatorQuery,
    fetch_limit: i64,
    limit: i64,
) -> Result<Vec<Row>, ProjectionError> {
    match &query.mode {
        NavigatorMode::Latest => client.query(sql, &[&fetch_limit, &limit]).await,
        NavigatorMode::Page { cursor, .. } => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            let height = cursor.max as i32;
            client
                .query(sql, &[&fetch_limit, &limit, &height, &hash])
                .await
        }
        NavigatorMode::Anchor { hash } => {
            let hash = stored_hash_from_display(hash)?;
            client.query(sql, &[&fetch_limit, &limit, &hash]).await
        }
    }
    .context("load height navigator")
    .map_err(ProjectionError::from)
}

fn decode_height_page(rows: Vec<Row>) -> Result<KeysetPage<HeightRow>, ProjectionError> {
    let Some(head) = rows.first() else {
        return Ok(empty_page());
    };
    let mut items = Vec::new();
    for row in &rows {
        let Some(height) = row.get::<_, Option<i32>>(3) else {
            continue;
        };
        let Some(hash) = row.get::<_, Option<Vec<u8>>>(4) else {
            continue;
        };
        items.push(HeightRow { height, hash });
    }
    Ok(page_from_head(head, items, None))
}

pub(super) async fn fetch_time_keyset(
    client: &Client,
    query: &NavigatorQuery,
    class_values: &[String],
    include_pending: bool,
) -> Result<KeysetPage<TimeRow>, ProjectionError> {
    let fetch_limit = (query.limit + 1) as i64;
    let limit = query.limit as i64;
    let sql = time_keyset_sql(&query.mode);
    let rows = query_time_rows(
        client,
        &sql,
        query,
        fetch_limit,
        limit,
        class_values,
        include_pending,
    )
    .await?;
    decode_time_page(rows)
}

async fn query_time_rows(
    client: &Client,
    sql: &str,
    query: &NavigatorQuery,
    fetch_limit: i64,
    limit: i64,
    class_values: &[String],
    include_pending: bool,
) -> Result<Vec<Row>, ProjectionError> {
    match &query.mode {
        NavigatorMode::Latest => {
            client
                .query(
                    sql,
                    &[&fetch_limit, &limit, &class_values, &include_pending],
                )
                .await
        }
        NavigatorMode::Page { cursor, .. } => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            client
                .query(
                    sql,
                    &[
                        &fetch_limit,
                        &limit,
                        &class_values,
                        &include_pending,
                        &cursor.max,
                        &hash,
                    ],
                )
                .await
        }
        NavigatorMode::Anchor { hash } => {
            let hash = stored_hash_from_display(hash)?;
            client
                .query(
                    sql,
                    &[&fetch_limit, &limit, &class_values, &include_pending, &hash],
                )
                .await
        }
    }
    .context("load orphan navigator")
    .map_err(ProjectionError::from)
}

fn decode_time_page(rows: Vec<Row>) -> Result<KeysetPage<TimeRow>, ProjectionError> {
    let Some(head) = rows.first() else {
        return Ok(empty_page());
    };
    let facets = Some(OrphanClassCounts {
        strict: head.get::<_, i64>(3).max(0) as u64,
        weak: head.get::<_, i64>(4).max(0) as u64,
        excluded: head.get::<_, i64>(5).max(0) as u64,
        pending: head.get::<_, i64>(6).max(0) as u64,
    });
    let mut items = Vec::new();
    for row in &rows {
        let Some(header_time) = row.get::<_, Option<i64>>(7) else {
            continue;
        };
        let Some(hash) = row.get::<_, Option<Vec<u8>>>(8) else {
            continue;
        };
        items.push(TimeRow {
            header_time,
            hash,
            btc_orphan_class: row.get(9),
        });
    }
    Ok(page_from_head(head, items, facets))
}

fn empty_page<T>() -> KeysetPage<T> {
    KeysetPage {
        rows: Vec::new(),
        total: 0,
        newer_count: 0,
        has_more_scan: false,
        facets: None,
    }
}

fn page_from_head<T>(head: &Row, rows: Vec<T>, facets: Option<OrphanClassCounts>) -> KeysetPage<T> {
    KeysetPage {
        rows,
        total: head.get::<_, i64>(0).max(0) as u64,
        newer_count: head.get::<_, i64>(1).max(0) as u64,
        has_more_scan: head.get(2),
        facets,
    }
}
