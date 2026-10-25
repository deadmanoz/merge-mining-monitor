use tokio_postgres::Client;

use crate::query::{NavigatorQuery, NavigatorTarget};

use super::super::ProjectionError;
use super::NavigatorPayload;
use super::height::height_blocks;

/// Ordered by `(height, hash)` because multiple invalid blocks can share a
/// height, and filtered to `distinct_sources >= 1` to match `/tree`'s default
/// `min_sources` candidate filter.
const INVALID_BLOCK_ELIGIBLE: &str = "SELECT btc_height AS axis, 0::bigint AS tiebreak, \
     btc_header_hash AS hash, FALSE AS orphan, NULL::text AS orphan_class \
     FROM block \
     WHERE kind = 'invalid_block' AND distinct_sources >= 1";

pub async fn invalid_blocks(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::InvalidBlock);
    debug_assert!(query.classification.is_empty());
    height_blocks(client, query, INVALID_BLOCK_ELIGIBLE, &[], "Invalid block").await
}
