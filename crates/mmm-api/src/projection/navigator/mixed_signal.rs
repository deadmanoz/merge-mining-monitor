//! The `mixed-signal` target: blocks whose stored `relationship_summary` names two or more known
//! pools, in step-height order. A block with a height steps at it; an orphan steps at the height
//! of the canonical block with the latest header time at or before its own, or `0` when it
//! predates every stored canonical block. Blocks at one step height go newest header time first.

use tokio_postgres::Client;

use crate::query::{NavigatorQuery, NavigatorTarget};

use super::super::ProjectionError;
use super::super::shared::classification_filter_params;
use super::NavigatorPayload;
use super::height::height_blocks;

/// The mixed blocks `/tree` shows at its default filters: every canonical block, stale and
/// invalid blocks with a source, and the orphan target's orphans within the `$1`/`$2` class
/// filter. Each branch reads only its own summary rows, by their stored height.
const MIXED_SIGNAL_ELIGIBLE: &str = "\
SELECT s.btc_height AS axis, s.btc_header_time AS tiebreak, s.btc_header_hash AS hash, \
       FALSE AS orphan, NULL::text AS orphan_class \
  FROM relationship_summary s JOIN block b ON b.btc_header_hash = s.btc_header_hash \
 WHERE s.mixed AND s.btc_height IS NOT NULL \
   AND (b.kind = 'canonical' \
        OR (b.kind IN ('stale', 'invalid_block') AND b.distinct_sources >= 1)) \
UNION ALL \
SELECT COALESCE(step.btc_height, 0), s.btc_header_time, s.btc_header_hash, \
       TRUE, b.btc_orphan_class \
  FROM relationship_summary s JOIN block b ON b.btc_header_hash = s.btc_header_hash \
  LEFT JOIN LATERAL ( \
      SELECT c.btc_height FROM block c \
       WHERE c.kind = 'canonical' AND c.btc_header_time <= s.btc_header_time \
       ORDER BY c.btc_header_time DESC, c.btc_height DESC LIMIT 1) step ON TRUE \
 WHERE s.mixed AND s.btc_height IS NULL AND b.kind = 'unknown' AND b.pow_validated \
   AND (b.btc_orphan_class = ANY($1::text[]) \
        OR ($2::boolean AND b.btc_orphan_class IS NULL))";

pub async fn mixed_signals(
    client: &Client,
    query: &NavigatorQuery,
) -> Result<NavigatorPayload, ProjectionError> {
    debug_assert_eq!(query.target, NavigatorTarget::MixedSignal);
    let (class_values, include_pending) = classification_filter_params(&query.classification);
    height_blocks(
        client,
        query,
        MIXED_SIGNAL_ELIGIBLE,
        &[&class_values, &include_pending],
        "Mixed signals",
    )
    .await
}
