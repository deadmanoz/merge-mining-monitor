//! Source-id lookup and the live-poll cursor (poll_cursor table).

use anyhow::{Context, Result};
use tokio_postgres::Client;

/// Resolve a `source.code` (e.g. `"namecoin"`) to its numeric `source.id`.
/// Errors if the code is not registered; the foundational lookup every producer
/// and read-model path runs before touching `merge_mining_event`.
pub async fn get_source_id(client: &Client, code: &str) -> Result<i64> {
    let row = client
        .query_one("SELECT id FROM source WHERE code = $1", &[&code])
        .await
        .with_context(|| format!("load source id for {code}"))?;
    Ok(row.get(0))
}

/// Load the persisted live-poll cursor for a source, or `None` when the source
/// has no `poll_cursor` row yet (a genuinely fresh live start).
pub async fn load_poll_cursor(client: &Client, source_id: i64) -> Result<Option<i32>> {
    let row = client
        .query_opt(
            "SELECT cursor_height FROM poll_cursor WHERE source_id = $1",
            &[&source_id],
        )
        .await
        .context("load poll cursor")?;
    Ok(row.map(|row| row.get(0)))
}

/// Persist the live-poll cursor and optional observed target height for a source.
///
/// Monotonic: the stored `cursor_height` is never lowered (`GREATEST`), so an
/// explicit lower-height replay re-captures events without downgrading live
/// progress (lowering is a deliberate admin/reset action, not a side effect of an
/// override). `updated_at` remains progress time: it changes on insert or when
/// `cursor_height` advances, not when only `target_height` changes.
pub async fn upsert_poll_cursor_with_target(
    client: &Client,
    source_id: i64,
    cursor_height: i32,
    target_height: Option<i32>,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO poll_cursor (source_id, cursor_height, target_height) \
             VALUES ($1, $2, $3) \
             ON CONFLICT (source_id) DO UPDATE SET \
               cursor_height = GREATEST(poll_cursor.cursor_height, EXCLUDED.cursor_height), \
               target_height = COALESCE(EXCLUDED.target_height, poll_cursor.target_height), \
               updated_at = CASE \
                   WHEN EXCLUDED.cursor_height > poll_cursor.cursor_height THEN now() \
                   ELSE poll_cursor.updated_at \
               END",
            &[&source_id, &cursor_height, &target_height],
        )
        .await
        .context("upsert poll cursor")?;
    Ok(())
}
