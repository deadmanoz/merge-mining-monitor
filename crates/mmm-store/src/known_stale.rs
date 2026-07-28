//! `known_stale_block` membership: operator-imported known-stale reads/writes.
//!
//! The `known_stale_block` table is a base table (operator-imported from the
//! upstream stale-blocks dataset), so its writer lives here in mmm-store.
//! See `is_known_stale_hash` for the membership boundary.

use anyhow::{Context, Result};
use tokio_postgres::GenericClient;

/// Upsert one known-stale membership row, idempotent on `hash`. On conflict the
/// existing `source_label` / `imported_at` provenance is kept (a re-import never
/// rewrites when a hash was first recorded), so re-running the importer is a
/// no-op for already-present hashes. Returns `true` when a NEW row was inserted.
pub async fn upsert_known_stale_block<C: GenericClient>(
    client: &C,
    hash: &[u8],
    btc_height: Option<i32>,
    source_label: &str,
    imported_at: i64,
) -> Result<bool> {
    let rows = client
        .execute(
            "INSERT INTO known_stale_block (hash, btc_height, source_label, imported_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (hash) DO NOTHING",
            &[&hash, &btc_height, &source_label, &imported_at],
        )
        .await
        .context("upsert known_stale_block")?;
    Ok(rows == 1)
}

/// The number of rows in `known_stale_block`. Used by the classify-path drivers
/// to detect the degraded EMPTY-membership state (the upstream dataset was never
/// imported) and warn or refuse rather than silently classifying known stales as
/// strict/weak orphans.
pub async fn count_known_stale_blocks<C: GenericClient>(client: &C) -> Result<i64> {
    let row = client
        .query_one("SELECT count(*)::bigint FROM known_stale_block", &[])
        .await
        .context("count known_stale_block")?;
    Ok(row.get(0))
}

/// Whether `hash` (internal byte order) is a known stale: present in the
/// operator-imported `known_stale_block` set. A single indexed PK lookup, so
/// this stays O(1) per parent even in the reconciler's per-parent path.
/// Mirrors the research classifier's `known_stale_hash` membership, whose
/// verdict is `excluded`. Deliberately NOT unioned with `block.kind = 'stale'`
/// rows: for a given hash that arm could only ever match the row under
/// classification itself, so a stale-to-unknown re-derivation would consult
/// the very state it is replacing and persist a self-referential `excluded`.
pub async fn is_known_stale_hash<C: GenericClient>(client: &C, hash: &[u8]) -> Result<bool> {
    let row = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM known_stale_block WHERE hash = $1)",
            &[&hash],
        )
        .await
        .context("check known-stale membership")?;
    Ok(row.get(0))
}
