//! RSK miner-address identity reclassification for `reclassify-pools`.
//!
//! RSK already persists the child-chain miner beneficiary in
//! `rsk_merge_mining_evidence.rsk_miner`. This module reuses that persisted
//! value through the embedded `rsk_miner_address` registry, materializes the
//! event-level provenance rows that newer captures write live, and late-fills
//! the sidecar's `pool_identity_id`. It never fetches Rootstock blocks and
//! never treats explorer labels as authority.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::{Hash as _, sha256d};
use serde_json::Value;
use tokio_postgres::Client;
use tracing::{debug, info};

use mmm_capture::capture::EventPoolAttribution;
use mmm_capture::pool_resolver::{
    DEFAULT_RSK_MINER_REGISTRY_JSON, PoolIdentityRegistry, RSK_MINER_ADDRESS_NAMESPACE,
};
use mmm_capture::source_registry::RSK_SOURCE_CODE;
use mmm_store::{
    get_source_id, late_fill_rsk_pool_identity_id, load_rsk_reclassify_watermark,
    rsk_active_set_fingerprint, upsert_event_pool_attributions,
    upsert_rsk_pool_identities_with_policy, upsert_rsk_reclassify_watermark,
};

use crate::reclassify_pools::{ReclassifyPoolsConfig, ReclassifyPoolsStats};

/// One scanned event: its persisted `rsk_miner` address, the current sidecar
/// `pool_identity_id` (for one-way late-fill), the event's `observed_at`, and a
/// snapshot of the existing `rsk_miner_address` attribution to diff against.
#[derive(Debug)]
struct ReplayRow {
    event_id: i64,
    miner_address: String,
    sidecar_pool_identity_id: Option<i64>,
    observed_at: i64,
    current_attribution: ExistingRskMinerAttribution,
}

/// Snapshot of the existing `rsk_miner_address` attribution for one event.
/// `row_count` is the LATERAL `count(*)`: the per-field columns are only
/// meaningful (non-NULL) when exactly one row exists, so the write decision
/// must gate on `row_count == 1` before trusting them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExistingRskMinerAttribution {
    row_count: i64,
    pool_id: Option<i64>,
    pool_identity_id: Option<i64>,
    source: Option<String>,
    confidence: Option<String>,
    details: Option<Value>,
}

impl ExistingRskMinerAttribution {
    /// True when exactly one existing row already equals the candidate on every
    /// persisted field, so the upsert can be skipped as a no-op.
    fn matches(&self, attribution: &EventPoolAttribution) -> bool {
        self.row_count == 1
            && self.pool_id == attribution.pool_id
            && self.pool_identity_id == attribution.pool_identity_id
            && self.source.as_deref() == Some(attribution.source)
            && self.confidence.as_deref() == Some(attribution.confidence.as_db_str())
            && self.details.as_ref() == Some(&attribution.details)
    }

    /// True when the single existing row already carries a resolved `pool_id`:
    /// the guard that stops an unresolved candidate from demoting it.
    fn has_resolved_pool(&self) -> bool {
        self.row_count == 1 && self.pool_id.is_some()
    }
}

/// A registry hit for a miner address: both the `pool.id` and the
/// `pool_identity.id` are guaranteed present (the resolver bails otherwise).
#[derive(Debug, Clone, Copy)]
struct ResolvedRskMinerIdentity {
    pool_id: i64,
    pool_identity_id: i64,
}

/// One event's planned mutation. `write_attribution` and
/// `late_fill_pool_identity_id` are independent: a row may need only one of the
/// two, and a row needing neither is dropped before this is produced.
#[derive(Debug)]
struct PlannedRskMinerUpdate {
    event_id: i64,
    attribution: EventPoolAttribution,
    write_attribution: bool,
    /// Set only when the sidecar `pool_identity_id` is currently NULL: the column
    /// is filled once and never overwritten (one-way fill).
    late_fill_pool_identity_id: Option<i64>,
    observed_at: i64,
}

/// Reclassify every active RSK event's persisted `rsk_miner` through the
/// embedded registry: seed identity rows, then keyset-page over events (ordered
/// by `event_id`, paged on the last id) and apply the planned attribution
/// upserts and sidecar late-fills per batch in a transaction. Idempotent:
/// re-running only touches rows whose resolution actually changed. For
/// non-`--overwrite` runs, a skip watermark (embedded registry hash + active-set
/// fingerprint) short-circuits the whole scan when nothing it reads has changed
/// since the last successful pass.
pub(crate) async fn reresolve_rsk_miner_identities(
    client: &mut Client,
    registry: &PoolIdentityRegistry,
    pool_ids_by_slug: &HashMap<String, i64>,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Result<()> {
    let identity_ids_by_address = upsert_rsk_pool_identities_with_policy(
        client,
        registry,
        pool_ids_by_slug,
        config.overwrite,
    )
    .await
    .context("seed RSK miner pool identities")?;
    let rsk_source_id = get_source_id(client, RSK_SOURCE_CODE).await?;

    // Tier 1 skip watermark. Computed AFTER the seed above (which enforces
    // registry remap conflicts even at zero rows) and BEFORE the expensive scan.
    // Only honoured for non-`--overwrite` runs, since `--overwrite` deliberately
    // rewrites rows. The skip-check compares the live (start-of-pass) fingerprint
    // against the stored watermark; the watermark itself is (re)written from a
    // FRESH end-of-pass fingerprint below, not this one, so events processed
    // during the scan are reflected and an event revoked-then-restored mid-scan
    // is not wrongly claimed as covered.
    let registry_hash = sha256d::Hash::hash(DEFAULT_RSK_MINER_REGISTRY_JSON.as_bytes()).to_string();
    let start_fingerprint = rsk_active_set_fingerprint(&*client, rsk_source_id).await?;
    if !config.overwrite
        && let Some(watermark) = load_rsk_reclassify_watermark(&*client).await?
        && watermark.registry_hash == registry_hash
        && watermark.fingerprint == start_fingerprint
    {
        info!(
            phase = "rsk",
            active_event_count = start_fingerprint.active_event_count,
            "reclassify-pools: RSK pass skipped by watermark (registry + active set unchanged)"
        );
        return Ok(());
    }

    let mut cursor = None;
    let mut batch_index: u64 = 0;
    loop {
        let rows = load_rsk_miner_batch(client, rsk_source_id, cursor, config.batch_size).await?;
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|row| row.event_id);
        batch_index += 1;
        if batch_index.is_multiple_of(50) {
            debug!(
                phase = "rsk",
                batch_index,
                cursor = ?cursor,
                rows_scanned = stats.rsk_miner_rows_scanned,
                "reclassify-pools: RSK miner-identity scan progress"
            );
        }
        let mut updates = Vec::new();
        for row in &rows {
            let resolved = resolve_rsk_miner_identity(
                row,
                registry,
                pool_ids_by_slug,
                &identity_ids_by_address,
            )?;
            stats.rsk_miner_rows_scanned += 1;
            if resolved.is_some() {
                stats.rsk_miner_registry_resolved_rows += 1;
            } else {
                stats.rsk_miner_unresolved_rows += 1;
            }
            if let Some(update) = plan_rsk_miner_update(row, resolved, config.overwrite)? {
                updates.push(update);
            }
        }
        apply_rsk_miner_updates(client, &updates, stats).await?;
    }

    // Record the watermark for non-overwrite runs so an unchanged re-run skips
    // the whole pass next time. Re-reads a FRESH end-of-pass fingerprint: the scan
    // paged to exhaustion, so every currently-active event was either processed by
    // the loop or excluded because it is revoked. Writing the current state (rather
    // than the start-of-pass value) both avoids over-claiming an event revoked
    // mid-scan and avoids a needless rescan of events that arrived and were
    // processed during this pass.
    if !config.overwrite {
        let end_fingerprint = rsk_active_set_fingerprint(&*client, rsk_source_id).await?;
        let completed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0);
        upsert_rsk_reclassify_watermark(&*client, &registry_hash, end_fingerprint, completed_at)
            .await?;
    }

    Ok(())
}

/// Load one keyset page of active RSK events past `cursor` (NULL on the first
/// page), each joined to its single `rsk_miner_address` attribution via a
/// LATERAL count so the caller can diff against [`ExistingRskMinerAttribution`].
/// Ordered by `event_id` so the last id seeds the next page.
async fn load_rsk_miner_batch(
    client: &Client,
    rsk_source_id: i64,
    cursor: Option<i64>,
    batch_size: i64,
) -> Result<Vec<ReplayRow>> {
    let rows = client
        .query(
            r"
SELECT e.id,
       encode(r.rsk_miner, 'hex') AS miner,
       r.pool_identity_id AS sidecar_pool_identity_id,
       e.confirmed_at,
       attr.row_count,
       attr.pool_id,
       attr.pool_identity_id,
       attr.source,
       attr.confidence,
       attr.details
FROM rsk_merge_mining_evidence r
JOIN merge_mining_event e ON e.id = r.event_id
LEFT JOIN LATERAL (
    SELECT count(*)::bigint AS row_count,
           CASE WHEN count(*) = 1 THEN min(a.pool_id) ELSE NULL END AS pool_id,
           CASE WHEN count(*) = 1 THEN min(a.pool_identity_id) ELSE NULL END AS pool_identity_id,
           CASE WHEN count(*) = 1 THEN min(a.source) ELSE NULL END AS source,
           CASE WHEN count(*) = 1 THEN min(a.confidence) ELSE NULL END AS confidence,
           CASE WHEN count(*) = 1 THEN (array_agg(a.details ORDER BY a.id))[1] ELSE NULL END AS details
    FROM event_pool_attribution a
    WHERE a.event_id = e.id
      AND a.side = 'child_block'
      AND a.namespace = $2
      AND a.match_kind = 'miner_address'
      AND a.matched_value = encode(r.rsk_miner, 'hex')
) attr ON true
WHERE e.source_id = $1
  AND e.revoked_at IS NULL
  AND ($3::bigint IS NULL OR e.id > $3)
ORDER BY e.id
LIMIT $4",
            &[
                &rsk_source_id,
                &RSK_MINER_ADDRESS_NAMESPACE,
                &cursor,
                &batch_size,
            ],
        )
        .await
        .context("load RSK miner identity reclassification batch")?;

    Ok(rows
        .into_iter()
        .map(|row| ReplayRow {
            event_id: row.get(0),
            miner_address: row.get(1),
            sidecar_pool_identity_id: row.get(2),
            observed_at: row.get(3),
            current_attribution: ExistingRskMinerAttribution {
                row_count: row.get(4),
                pool_id: row.get(5),
                pool_identity_id: row.get(6),
                source: row.get(7),
                confidence: row.get(8),
                details: row.get(9),
            },
        })
        .collect())
}

/// Resolve a row's miner address to its registry identity. `Ok(None)` when the
/// address is not in the registry; `Err` only when the registry references a
/// pool slug or address with no seeded row (a reseed invariant violation, not a
/// per-row miss).
fn resolve_rsk_miner_identity(
    row: &ReplayRow,
    registry: &PoolIdentityRegistry,
    pool_ids_by_slug: &HashMap<String, i64>,
    identity_ids_by_address: &HashMap<String, i64>,
) -> Result<Option<ResolvedRskMinerIdentity>> {
    let Some(identity_match) = registry.resolve_rsk_miner(&row.miner_address) else {
        return Ok(None);
    };
    let pool_id = pool_ids_by_slug
        .get(&identity_match.entry.pool_slug)
        .copied()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "RSK miner registry references pool slug {slug} that is not in the pool table",
                slug = identity_match.entry.pool_slug
            )
        })?;
    let pool_identity_id = identity_ids_by_address
        .get(&row.miner_address)
        .copied()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "RSK miner registry address {address} has no pool_identity row",
                address = row.miner_address
            )
        })?;
    Ok(Some(ResolvedRskMinerIdentity {
        pool_id,
        pool_identity_id,
    }))
}

/// Decide what (if anything) to mutate for one row. Builds the candidate
/// attribution (resolved or unresolved), bails on a resolved-to-resolved pool
/// remap unless `overwrite` (the operator must validate the remap first), and
/// computes the one-way sidecar late-fill. Returns `None` when neither the
/// attribution nor the sidecar needs to change.
fn plan_rsk_miner_update(
    row: &ReplayRow,
    resolved: Option<ResolvedRskMinerIdentity>,
    overwrite: bool,
) -> Result<Option<PlannedRskMinerUpdate>> {
    let attribution = match resolved {
        Some(identity) => EventPoolAttribution::rsk_miner_address(
            row.miner_address.clone(),
            Some(identity.pool_id),
            Some(identity.pool_identity_id),
            true,
        ),
        None => {
            EventPoolAttribution::rsk_miner_address(row.miner_address.clone(), None, None, false)
        }
    };
    let write_attribution =
        should_write_rsk_attribution(&row.current_attribution, &attribution, overwrite);
    if !overwrite && resolved_pool_conflict(&row.current_attribution, &attribution) {
        bail!(
            "RSK miner reclassification would remap event_id {event_id} miner {miner} from pool_id {old_pool} \
             to pool_id {new_pool}; rerun with --overwrite after validating the remap evidence",
            event_id = row.event_id,
            miner = row.miner_address,
            old_pool = row.current_attribution.pool_id.unwrap_or_default(),
            new_pool = attribution.pool_id.unwrap_or_default()
        );
    }
    let late_fill_pool_identity_id = resolved
        .map(|identity| identity.pool_identity_id)
        .filter(|_| row.sidecar_pool_identity_id.is_none());

    if !write_attribution && late_fill_pool_identity_id.is_none() {
        return Ok(None);
    }

    Ok(Some(PlannedRskMinerUpdate {
        event_id: row.event_id,
        attribution,
        write_attribution,
        late_fill_pool_identity_id,
        observed_at: row.observed_at,
    }))
}

/// True when both the existing single row and the candidate carry a resolved
/// pool and they disagree: the remap that requires `--overwrite`.
fn resolved_pool_conflict(
    current: &ExistingRskMinerAttribution,
    attribution: &EventPoolAttribution,
) -> bool {
    current.row_count == 1
        && current.pool_id.is_some()
        && attribution.pool_id.is_some()
        && current.pool_id != attribution.pool_id
}

/// The attribution write policy, in precedence order: write when none exists;
/// skip an exact match; never let an unresolved candidate demote a resolved row;
/// always promote unresolved -> resolved and fill a missing `pool_identity_id`
/// within the same pool; otherwise defer to `overwrite`. The
/// never-demote-resolved rule is the key invariant transient misses must honor.
fn should_write_rsk_attribution(
    current: &ExistingRskMinerAttribution,
    attribution: &EventPoolAttribution,
    overwrite: bool,
) -> bool {
    if current.row_count == 0 {
        return true;
    }
    if current.matches(attribution) {
        return false;
    }
    if attribution.pool_id.is_none() && current.has_resolved_pool() {
        return false;
    }
    if current.pool_id.is_none() && attribution.pool_id.is_some() {
        return true;
    }
    if current.pool_id == attribution.pool_id
        && attribution.pool_identity_id.is_some()
        && current.pool_identity_id != attribution.pool_identity_id
    {
        return true;
    }
    overwrite
}

/// Apply one batch's planned updates in a single transaction: per row, upsert
/// the attribution when flagged and late-fill the sidecar `pool_identity_id`
/// when set, bumping the matching stat counters only on rows that actually
/// changed. A no-op batch returns early without opening a transaction.
async fn apply_rsk_miner_updates(
    client: &mut Client,
    updates: &[PlannedRskMinerUpdate],
    stats: &mut ReclassifyPoolsStats,
) -> Result<()> {
    if updates.is_empty() {
        return Ok(());
    }
    let txn = client
        .transaction()
        .await
        .context("begin RSK miner identity reclassification transaction")?;
    for update in updates {
        if update.write_attribution {
            upsert_event_pool_attributions(
                &txn,
                update.event_id,
                std::slice::from_ref(&update.attribution),
                update.observed_at,
            )
            .await?;
            stats.rsk_miner_attribution_updates += 1;
        }
        if let Some(pool_identity_id) = update.late_fill_pool_identity_id
            && late_fill_rsk_pool_identity_id(&txn, update.event_id, pool_identity_id).await?
        {
            stats.rsk_miner_sidecar_late_fills += 1;
        }
    }
    txn.commit()
        .await
        .context("commit RSK miner identity reclassification transaction")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSK_MINER: &str = "12d3178a62ef1f520944534ed04504609f7307a1";

    fn rsk_attr(
        pool_id: Option<i64>,
        pool_identity_id: Option<i64>,
        registry_match: bool,
    ) -> EventPoolAttribution {
        EventPoolAttribution::rsk_miner_address(
            RSK_MINER.to_owned(),
            pool_id,
            pool_identity_id,
            registry_match,
        )
    }

    fn existing_from(attribution: &EventPoolAttribution) -> ExistingRskMinerAttribution {
        ExistingRskMinerAttribution {
            row_count: 1,
            pool_id: attribution.pool_id,
            pool_identity_id: attribution.pool_identity_id,
            source: Some(attribution.source.to_owned()),
            confidence: Some(attribution.confidence.as_db_str().to_owned()),
            details: Some(attribution.details.clone()),
        }
    }

    #[test]
    fn rsk_attribution_policy_promotes_repairs_and_never_demotes() {
        let current = ExistingRskMinerAttribution::default();
        let unresolved = rsk_attr(None, None, false);
        assert!(should_write_rsk_attribution(&current, &unresolved, false));

        let current = existing_from(&unresolved);
        let resolved = rsk_attr(Some(7), Some(8), true);
        assert!(should_write_rsk_attribution(&current, &resolved, false));

        let partial = rsk_attr(Some(7), None, true);
        let current = existing_from(&partial);
        assert!(should_write_rsk_attribution(&current, &resolved, false));

        let current = existing_from(&resolved);
        assert!(!should_write_rsk_attribution(&current, &unresolved, true));

        let changed = rsk_attr(Some(9), Some(10), true);
        assert!(!should_write_rsk_attribution(&current, &changed, false));
        assert!(should_write_rsk_attribution(&current, &changed, true));
    }
}
