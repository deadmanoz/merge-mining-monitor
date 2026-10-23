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
struct InvalidBlockRow {
    height: i32,
    hash: String,
}

/// Ordered by `(height, hash)` because multiple invalid blocks can share a
/// height, and filtered to `distinct_sources >= 1` to match `/tree`'s default
/// `min_sources` candidate filter.
const INVALID_BLOCK_ELIGIBLE: &str = "SELECT btc_height AS axis, btc_header_hash AS hash \
     FROM block \
     WHERE kind = 'invalid_block' AND distinct_sources >= 1";

pub async fn invalid_blocks(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::InvalidBlock);
    debug_assert!(query.classification.is_empty());
    let max_complete_height = load_max_complete_canonical_height(client).await?;
    let mut page = fetch_height_keyset(client, INVALID_BLOCK_ELIGIBLE, query).await?;
    if is_newer_page(query) {
        page.rows.reverse();
    }
    let rows = page
        .rows
        .iter()
        .map(|row| {
            Ok(InvalidBlockRow {
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
        .map(|(row, span)| invalid_block_item(row, span, max_complete_height, readiness.as_ref()))
        .collect::<Vec<_>>();
    apply_first_index(
        &mut items,
        page.first_index(matches!(query.mode, NavigatorMode::Latest)),
    );

    let (next_cursor, prev_cursor) = page_cursors(
        NavigatorTarget::InvalidBlock,
        query,
        &items,
        page.has_more_scan,
        |cursor| exists_invalid_block_across_edge(client, cursor, PageEdge::Older),
        |cursor| exists_invalid_block_across_edge(client, cursor, PageEdge::Newer),
    )
    .await?;

    Ok(NavigatorPayload::new(
        NavigatorTarget::InvalidBlock,
        items,
        page.total,
        NavigatorFacets::default(),
        next_cursor,
        prev_cursor,
    ))
}

fn invalid_block_item(
    row: &InvalidBlockRow,
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
        id: format!("invalid-block-{}", row.hash),
        kind: NavigatorTarget::InvalidBlock.as_str(),
        primary_hash: row.hash.clone(),
        label: format!("Invalid block #{}", row.height),
        position: NavigatorPosition {
            axis: NavigatorAxis::Height,
            min: i64::from(row.height),
            max: i64::from(row.height),
        },
        cursor: NavigatorCursor::new(
            NavigatorTarget::InvalidBlock,
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

async fn exists_invalid_block_across_edge(
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
                 WHERE kind = 'invalid_block' \
                   AND distinct_sources >= 1 \
                   AND (btc_height < $1 \
                        OR (btc_height = $1 AND btc_header_hash > $2)) \
             )"
        }
        PageEdge::Newer => {
            "SELECT EXISTS ( \
                 SELECT 1 FROM block \
                 WHERE kind = 'invalid_block' \
                   AND distinct_sources >= 1 \
                   AND (btc_height > $1 \
                        OR (btc_height = $1 AND btc_header_hash < $2)) \
             )"
        }
    };
    let row = client
        .query_one(sql, &[&height, &hash])
        .await
        .context("probe invalid-block navigator edge")?;
    Ok(row.get(0))
}
