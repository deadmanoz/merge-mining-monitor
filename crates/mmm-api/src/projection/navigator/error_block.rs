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
use super::keyset::{apply_first_index, fetch_height_keyset};
use super::{
    NavigatorFacets, NavigatorItem, NavigatorPayload, NavigatorPosition, NavigatorView, PageEdge,
    is_newer_page, page_cursors,
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
const ERROR_BLOCK_ELIGIBLE: &str = "SELECT btc_height AS axis, btc_header_hash AS hash \
     FROM block \
     WHERE kind = 'error_block' AND distinct_sources >= 1";

pub async fn error_blocks(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::ErrorBlock);
    debug_assert!(query.classification.is_empty());
    let max_complete_height = load_max_complete_canonical_height(client).await?;
    let mut page = fetch_height_keyset(client, ERROR_BLOCK_ELIGIBLE, query).await?;
    if is_newer_page(query) {
        page.rows.reverse();
    }
    let rows = page
        .rows
        .iter()
        .map(|row| {
            Ok(ErrorBlockRow {
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
        .map(|(row, span)| error_block_item(row, span, max_complete_height, readiness.as_ref()))
        .collect::<Vec<_>>();
    apply_first_index(
        &mut items,
        page.first_index(matches!(query.mode, NavigatorMode::Latest)),
    );

    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::ErrorBlock,
        query,
        &items,
        page.has_more_scan,
        |cursor| exists_error_block_across_edge(client, cursor, PageEdge::Older),
        |cursor| exists_error_block_across_edge(client, cursor, PageEdge::Newer),
    )
    .await?;

    Ok(NavigatorPayload::new(
        NavigatorTarget::ErrorBlock,
        items,
        page.total,
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
        index: 0,
        branch: None,
        orphan: None,
        view: navigation.map(NavigatorView::from),
        view_error,
    }
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
