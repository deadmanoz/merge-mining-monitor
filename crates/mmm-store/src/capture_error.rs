//! Producer-owned capture-error state (capture_error table).
//!
//! A producer that cannot capture a height records it here and clears it only
//! when that same height is reprocessed successfully. The poll cursor cannot
//! carry this: it is monotonic and has no error column, so once it is past a
//! height the gap is otherwise invisible. `source_health` is derived state
//! owned by `mmm-read-model`, so a producer cannot write it either.

use anyhow::{Context, Result};
use tokio_postgres::Client;

/// The `error_kind` for a height whose block claimed a merge-mining proof that
/// failed to decode. The only kind in the `capture_error` CHECK domain today.
pub const CAPTURE_ERROR_MALFORMED_AUXPOW_PROOF: &str = "malformed_auxpow_proof";

/// Record (or refresh) the capture error at one height. `first_seen_at` is
/// preserved across re-observations so the row keeps showing how long the gap
/// has been open; `last_seen_at`, `block_hash`, and `detail` are refreshed.
pub async fn record_capture_error(
    client: &Client,
    source_id: i64,
    height: i32,
    block_hash: Option<&[u8]>,
    error_kind: &str,
    detail: Option<&str>,
    observed_at_epoch: i64,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO capture_error \
               (source_id, height, block_hash, error_kind, detail, first_seen_at, last_seen_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $6) \
             ON CONFLICT (source_id, height) DO UPDATE SET \
               block_hash = EXCLUDED.block_hash, \
               error_kind = EXCLUDED.error_kind, \
               detail = EXCLUDED.detail, \
               last_seen_at = GREATEST(capture_error.last_seen_at, EXCLUDED.last_seen_at)",
            &[
                &source_id,
                &height,
                &block_hash,
                &error_kind,
                &detail,
                &observed_at_epoch,
            ],
        )
        .await
        .with_context(|| format!("record capture error for source {source_id} height {height}"))?;
    Ok(())
}

/// Clear the capture error at one height. Returns true when a row was removed,
/// so the caller can log the recovery rather than every clean height.
pub async fn clear_capture_error(client: &Client, source_id: i64, height: i32) -> Result<bool> {
    let removed = client
        .execute(
            "DELETE FROM capture_error WHERE source_id = $1 AND height = $2",
            &[&source_id, &height],
        )
        .await
        .with_context(|| format!("clear capture error for source {source_id} height {height}"))?;
    Ok(removed > 0)
}
