//! The base writers' `relationship_refresh_queue` rows: one compact row per change, never
//! expanded inside the writer's transaction. The read model queues its own headers, expands
//! identity rows and re-derives each header's `relationship_summary`.

use anyhow::{Context, Result};
use tokio_postgres::GenericClient;

/// Queue the parent header of `event_id` after its attribution rows changed.
pub(crate) async fn enqueue_relationship_event_parent<C: GenericClient>(
    client: &C,
    event_id: i64,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO relationship_refresh_queue (btc_header_hash) \
             SELECT e.btc_parent_header_hash FROM merge_mining_event e \
              WHERE e.id = $1 \
                AND EXISTS (SELECT 1 FROM block b WHERE b.btc_header_hash = e.btc_parent_header_hash)",
            &[&event_id],
        )
        .await
        .with_context(|| format!("queue the relationship summary of event {event_id}"))?;
    Ok(())
}

/// A data-modifying CTE that queues the `(namespace, identifier)` rows a preceding CTE named
/// `changed` returns: the pool identities, keyed as `pool_identity` and protocol declarations key
/// them, whose pool was added, changed or removed. One statement, so the change and its queue row
/// commit together.
pub(crate) const QUEUE_CHANGED_IDENTITIES: &str = "queued AS ( \
     INSERT INTO relationship_refresh_queue (identity_namespace, identity_identifier) \
     SELECT namespace, identifier FROM changed)";
