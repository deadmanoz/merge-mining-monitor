use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::query::{
    NavigatorAxis, NavigatorCursor, NavigatorMode, NavigatorQuery, NavigatorTarget,
};

use super::super::ProjectionError;
use super::super::shared::{
    display_hash, load_max_complete_canonical_height, stored_hash_from_display,
};
use super::super::stale_navigation::{
    NavigationReadiness, NavigationSpan, load_navigation_readiness, navigation_for_span,
};
use super::{
    NavigatorFacets, NavigatorItem, NavigatorPayload, NavigatorPosition, NavigatorView, PageEdge,
    anchor_hash, cursor_params, is_newer_page, page_cursors,
};

#[derive(Debug, Clone)]
struct ErrorBlockRow {
    height: i32,
    hash: String,
}

/// Navigate consensus-invalid, full-proof-of-work parents.
///
/// Structurally the stale navigator without its branch machinery: an error
/// block never raced, so it has no canonical competitor to join and never forms
/// a branch. It does need the same `(height, hash)` ordering, because the
/// error-block set is not one block per height — several Bitcoin heights carry
/// more than one error block, so height alone is not a unique cursor key
/// and stepping on height alone would skip or repeat group members.
///
/// Eligibility requires at least one distinct source, matching what `/tree`
/// will actually render: its candidate filter drops an error block below
/// `min_sources`, which defaults to 1 and is never overridden by the frontend.
/// Offering a sourceless row would advertise a window whose tree response omits
/// the very block it selects, and the UI would silently fall back to the tip.
pub async fn error_blocks(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::ErrorBlock);
    debug_assert!(query.classification.is_empty());
    let max_complete_height = load_max_complete_canonical_height(client).await?;
    let total = load_error_blocks_total(client).await?;
    let fetch_limit = (query.limit + 1) as i64;
    let mut rows = fetch_error_blocks(client, query, fetch_limit).await?;
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
        .map(|(row, span)| error_block_item(row, span, max_complete_height, readiness.as_ref()))
        .collect::<Vec<_>>();

    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::ErrorBlock,
        query,
        &items,
        has_more_scan,
        |cursor| exists_error_block_across_edge(client, cursor, PageEdge::Older),
        |cursor| exists_error_block_across_edge(client, cursor, PageEdge::Newer),
    )
    .await?;

    Ok(NavigatorPayload::new(
        NavigatorTarget::ErrorBlock,
        items,
        total,
        NavigatorFacets::default(),
        next_cursor,
        prev_cursor,
    ))
}

fn error_block_item(
    row: &ErrorBlockRow,
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
        id: format!("error-block-{}", row.hash),
        kind: NavigatorTarget::ErrorBlock.as_str(),
        primary_hash: row.hash.clone(),
        label: format!("Error block #{}", row.height),
        position: NavigatorPosition {
            axis: NavigatorAxis::Height,
            min: i64::from(row.height),
            max: i64::from(row.height),
        },
        cursor: NavigatorCursor::new(
            NavigatorTarget::ErrorBlock,
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

async fn load_error_blocks_total(client: &Client) -> Result<u64, ProjectionError> {
    let row = client
        .query_one(
            "SELECT count(*)::bigint FROM block \
             WHERE kind = 'error_block' AND distinct_sources >= 1",
            &[],
        )
        .await
        .context("count error-block navigator")?;
    Ok(row.get::<_, i64>(0).max(0) as u64)
}

async fn fetch_error_blocks(
    client: &Client,
    query: &NavigatorQuery,
    fetch_limit: i64,
) -> Result<Vec<ErrorBlockRow>, ProjectionError> {
    let rows = match (&query.mode, cursor_params(query), anchor_hash(query)) {
        (NavigatorMode::Anchor { hash }, _, _) => {
            let hash = stored_hash_from_display(hash)?;
            client
                .query(
                    "SELECT btc_height, btc_header_hash \
                     FROM block \
                     WHERE kind = 'error_block' \
                       AND distinct_sources >= 1 \
                       AND btc_header_hash = $1",
                    &[&hash],
                )
                .await
        }
        (_, Some((PageEdge::Older, cursor)), _) => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            client
                .query(
                    "SELECT btc_height, btc_header_hash \
                     FROM block \
                     WHERE kind = 'error_block' \
                       AND distinct_sources >= 1 \
                       AND (btc_height < $2 \
                            OR (btc_height = $2 AND btc_header_hash > $3)) \
                     ORDER BY btc_height DESC, btc_header_hash ASC \
                     LIMIT $1",
                    &[&fetch_limit, &(cursor.max as i32), &hash],
                )
                .await
        }
        (_, Some((PageEdge::Newer, cursor)), _) => {
            let hash = stored_hash_from_display(&cursor.hash)?;
            client
                .query(
                    "SELECT btc_height, btc_header_hash \
                     FROM block \
                     WHERE kind = 'error_block' \
                       AND distinct_sources >= 1 \
                       AND (btc_height > $2 \
                            OR (btc_height = $2 AND btc_header_hash < $3)) \
                     ORDER BY btc_height ASC, btc_header_hash DESC \
                     LIMIT $1",
                    &[&fetch_limit, &(cursor.max as i32), &hash],
                )
                .await
        }
        _ => {
            client
                .query(
                    "SELECT btc_height, btc_header_hash \
                     FROM block \
                     WHERE kind = 'error_block' \
                       AND distinct_sources >= 1 \
                     ORDER BY btc_height DESC, btc_header_hash ASC \
                     LIMIT $1",
                    &[&fetch_limit],
                )
                .await
        }
    }
    .context("load error-block navigator")?;

    rows.into_iter()
        .map(|row| {
            let hash_bytes = row.get::<_, Vec<u8>>(1);
            Ok(ErrorBlockRow {
                height: row
                    .get::<_, Option<i32>>(0)
                    .context("error block missing height")?,
                hash: display_hash(&hash_bytes)?,
            })
        })
        .collect::<Result<Vec<_>>>()
        .map_err(ProjectionError::from)
}

async fn exists_error_block_across_edge(
    client: &Client,
    cursor: NavigatorCursor,
    edge: PageEdge,
) -> Result<bool, ProjectionError> {
    let hash = stored_hash_from_display(&cursor.hash)?;
    let height = cursor.max as i32;
    let sql = match edge {
        PageEdge::Older => {
            "SELECT EXISTS ( \
                 SELECT 1 FROM block \
                 WHERE kind = 'error_block' \
                   AND distinct_sources >= 1 \
                   AND (btc_height < $1 \
                        OR (btc_height = $1 AND btc_header_hash > $2)) \
             )"
        }
        PageEdge::Newer => {
            "SELECT EXISTS ( \
                 SELECT 1 FROM block \
                 WHERE kind = 'error_block' \
                   AND distinct_sources >= 1 \
                   AND (btc_height > $1 \
                        OR (btc_height = $1 AND btc_header_hash < $2)) \
             )"
        }
    };
    let row = client
        .query_one(sql, &[&height, &hash])
        .await
        .context("probe error-block navigator edge")?;
    Ok(row.get(0))
}
