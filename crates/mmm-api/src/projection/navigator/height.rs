//! The single-block height-axis targets (stale, invalid-block, mixed-signal): one page, item and
//! view builder over each target's eligible set. A block with a height opens a tree window; a
//! mixed-signal orphan opens its unheighted anchor.

use anyhow::Result;
use tokio_postgres::Client;
use tokio_postgres::types::ToSql;

use crate::query::{NavigatorAxis, NavigatorCursor, NavigatorQuery};

use super::super::ProjectionError;
use super::super::shared::{display_hash, load_max_complete_canonical_height};
use super::super::stale_navigation::{
    NavigationReadiness, NavigationSpan, load_navigation_readiness, navigation_for_span,
};
use super::keyset::{HeightRow, apply_first_index, fetch_height_keyset};
use super::{
    NavigatorFacets, NavigatorItem, NavigatorOrphan, NavigatorPayload, NavigatorPosition,
    NavigatorView, page_cursors,
};

/// Page `query.target`'s `eligible` set (see [`fetch_height_keyset`]), labelling each item
/// `"{label} #{height}"`, or `"{label} orphan {header_time}"` for an orphan.
pub(super) async fn height_blocks(
    client: &Client,
    query: &NavigatorQuery,
    eligible: &str,
    eligible_params: &[&(dyn ToSql + Sync)],
    label: &str,
) -> Result<NavigatorPayload, ProjectionError> {
    let max_complete_height = load_max_complete_canonical_height(client).await?;
    let page = fetch_height_keyset(client, eligible, eligible_params, query).await?;
    let window_spans = page
        .rows
        .iter()
        .filter(|row| !row.orphan)
        .map(|row| block_span(row.height))
        .collect::<Vec<_>>();
    let readiness = load_navigation_readiness(client, max_complete_height, &window_spans).await?;
    let mut items = page
        .rows
        .iter()
        .map(|row| height_item(query, label, row, max_complete_height, readiness.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    apply_first_index(&mut items, page.first_index());
    let (next_cursor, prev_cursor) = page_cursors(&items, page.exists_older(), page.exists_newer());
    Ok(NavigatorPayload::new(
        query.target,
        items,
        page.total,
        NavigatorFacets::default(),
        next_cursor,
        prev_cursor,
    ))
}

fn block_span(height: i32) -> NavigationSpan {
    NavigationSpan {
        target_height: height,
        span_min: height,
        span_max: height,
        required_nodes: 1,
    }
}

fn height_item(
    query: &NavigatorQuery,
    label: &str,
    row: &HeightRow,
    max_complete_height: Option<i32>,
    readiness: Option<&NavigationReadiness>,
) -> Result<NavigatorItem> {
    let target = query.target;
    let hash = display_hash(&row.hash)?;
    let (view, view_error, label) = if row.orphan {
        let anchor = NavigatorView::UnheightedAnchor {
            anchor_hash: hash.clone(),
            select_hash: hash.clone(),
            center_hash: hash.clone(),
        };
        (
            Some(anchor),
            None,
            format!("{label} orphan {}", row.tiebreak),
        )
    } else {
        let (navigation, view_error) = navigation_for_span(
            block_span(row.height),
            hash.clone(),
            hash.clone(),
            max_complete_height,
            readiness,
        );
        (
            navigation.map(NavigatorView::from),
            view_error,
            format!("{label} #{}", row.height),
        )
    };
    let height = i64::from(row.height);
    Ok(NavigatorItem {
        id: format!("{}-{hash}", target.as_str()),
        kind: target.as_str(),
        primary_hash: hash.clone(),
        label,
        position: NavigatorPosition {
            axis: NavigatorAxis::Height,
            min: height,
            max: height,
        },
        index: 0,
        cursor: NavigatorCursor::new(target, NavigatorAxis::Height, height, height, hash)
            .with_tiebreak(row.tiebreak)
            .encode(),
        branch: None,
        orphan: row.orphan.then(|| NavigatorOrphan {
            btc_orphan_class: row.orphan_class.clone(),
            btc_header_time: row.tiebreak,
        }),
        view,
        view_error,
    })
}
