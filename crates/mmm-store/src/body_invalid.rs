//! `body_invalid_stale` annotations: operator-imported body-invalid reads/writes.
//!
//! The `body_invalid_stale` table is a base table (operator-imported from the
//! pinned research overlay mirror), so its writer lives here in mmm-store. It
//! is a display annotation joined at API projection time only: nothing in
//! classification, orphan derivation, or reconciliation consults it, and
//! membership never promotes a row away from `kind='stale'`.

use anyhow::{Context, Result};
use tokio_postgres::GenericClient;

/// Upsert one body-invalid annotation row, idempotent on `hash`. Unlike the
/// neighbouring `known_stale_block` (first-write-wins provenance), a conflict
/// REPLACES the row: the table mirrors a pinned research artifact whose rule or
/// evidence URL can be corrected across pins, and the newest import must win.
/// Returns `true` when a NEW row was inserted (`false` for an update).
pub async fn upsert_body_invalid_stale<C: GenericClient>(
    client: &C,
    hash: &[u8],
    btc_height: Option<i32>,
    rule: &str,
    evidence_url: Option<&str>,
    source_label: &str,
    imported_at: i64,
) -> Result<bool> {
    let row = client
        .query_one(
            "INSERT INTO body_invalid_stale \
                 (hash, btc_height, rule, evidence_url, source_label, imported_at) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (hash) DO UPDATE SET \
                 btc_height = EXCLUDED.btc_height, \
                 rule = EXCLUDED.rule, \
                 evidence_url = EXCLUDED.evidence_url, \
                 source_label = EXCLUDED.source_label, \
                 imported_at = EXCLUDED.imported_at \
             RETURNING (xmax = 0)",
            &[
                &hash,
                &btc_height,
                &rule,
                &evidence_url,
                &source_label,
                &imported_at,
            ],
        )
        .await
        .context("upsert body_invalid_stale")?;
    Ok(row.get(0))
}

/// Delete every `body_invalid_stale` row whose hash is NOT in `keep`. The
/// pinned mirror is an authoritative snapshot: an annotation the newest pin
/// withdrew must not keep surfacing in block or tree responses. Returns the
/// number of pruned rows.
pub async fn delete_body_invalid_stales_not_in<C: GenericClient>(
    client: &C,
    keep: &[Vec<u8>],
) -> Result<u64> {
    let rows = client
        .execute(
            "DELETE FROM body_invalid_stale WHERE NOT (hash = ANY($1))",
            &[&keep],
        )
        .await
        .context("prune body_invalid_stale")?;
    Ok(rows)
}

/// The number of rows in `body_invalid_stale`. Used by the importer to refuse
/// recording an empty annotation set and by diagnostics.
pub async fn count_body_invalid_stales<C: GenericClient>(client: &C) -> Result<i64> {
    let row = client
        .query_one("SELECT count(*)::bigint FROM body_invalid_stale", &[])
        .await
        .context("count body_invalid_stale")?;
    Ok(row.get(0))
}
