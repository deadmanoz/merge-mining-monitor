//! Elastos producer base-table SQL: the identity re-resolution page load.

use anyhow::{Context, Result};
use tokio_postgres::GenericClient;

#[derive(Debug, Clone)]
pub struct ElastosIdentityReresolveRow {
    pub event_id: i64,
    pub confirmed_at: i64,
    pub attributions: serde_json::Value,
}

/// Keyset-paginated active Elastos events (`e.id > cursor_event_id`) whose unresolved
/// `child_block` identity attributions in `namespaces` now match a `pool_identity` row.
pub async fn load_elastos_identity_reresolve_batch<C: GenericClient>(
    client: &C,
    source_id: i64,
    cursor_event_id: Option<i64>,
    batch_size: i64,
    namespaces: &[&str],
) -> Result<Vec<ElastosIdentityReresolveRow>> {
    let namespace_values = namespaces
        .iter()
        .map(|namespace| (*namespace).to_owned())
        .collect::<Vec<_>>();
    let rows = client
        .query(
            "SELECT e.id, e.confirmed_at, \
                    jsonb_agg( \
                        jsonb_build_object( \
                            'namespace', a.namespace, \
                            'match_kind', a.match_kind, \
                            'matched_value', a.matched_value, \
                            'details', a.details, \
                            'pool_id', pi.pool_id, \
                            'pool_identity_id', pi.id \
                        ) ORDER BY a.namespace, a.matched_value \
                    ) AS attributions \
             FROM merge_mining_event e \
             JOIN event_pool_attribution a \
               ON a.event_id = e.id \
              AND a.side = 'child_block' \
              AND a.namespace = ANY($4::text[]) \
              AND a.pool_id IS NULL \
             JOIN pool_identity pi \
               ON pi.namespace = a.namespace \
              AND pi.identifier = a.matched_value \
             WHERE e.source_id = $1 \
               AND e.revoked_at IS NULL \
               AND ($2::bigint IS NULL OR e.id > $2) \
             GROUP BY e.id, e.confirmed_at \
             ORDER BY e.id \
             LIMIT $3",
            &[&source_id, &cursor_event_id, &batch_size, &namespace_values],
        )
        .await
        .context("load Elastos identity re-resolution batch")?;

    Ok(rows
        .into_iter()
        .map(|row| ElastosIdentityReresolveRow {
            event_id: row.get(0),
            confirmed_at: row.get(1),
            attributions: row.get(2),
        })
        .collect())
}
