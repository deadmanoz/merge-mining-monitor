//! Elastos identity re-resolution for `reclassify-pools` (no RPC).
//!
//! Elastos events do not persist the decoded child transaction vector, so fresh
//! identity discovery happens in the capture/backfill path. But once an
//! attribution row exists, its `matched_value` (the minerinfo label or reward
//! address) is persisted. So a registry addition can be applied to existing rows
//! WITHOUT re-fetching the chain: this promotes registry-matchable yet unresolved
//! Elastos child attributions (`pool_id IS NULL` but `(namespace, matched_value)`
//! now has a `pool_identity`) by re-emitting them with the pool attached, through
//! the shared `upsert_event_pool_attributions_without_stale_cleanup` (which
//! refreshes `merge_mining_event.child_miner_pool_id`). This is the Elastos
//! analogue of the Hathor reward-address replay tail: keyset-paginated,
//! idempotent, no RPC.

use anyhow::{Context, Result};
use tokio_postgres::Client;

use crate::chains::elastos::identity::{
    ELASTOS_MINERINFO_NAMESPACE, ELASTOS_REWARD_ADDRESS_NAMESPACE,
};
use crate::reclassify_pools::{ReclassifyPoolsConfig, ReclassifyPoolsStats};
use mmm_capture::capture::{
    CHILD_PAYOUT_REGISTRY_SOURCE, EventPoolAttribution, PoolAttributionConfidence,
    PoolAttributionSide,
};
use mmm_capture::source_registry::ELASTOS_SOURCE_CODE;
use mmm_store::{
    get_source_id, load_elastos_identity_reresolve_batch,
    upsert_event_pool_attributions_without_stale_cleanup,
};

/// Promote registry-matchable yet unresolved Elastos child identity attributions
/// for both Elastos namespaces, keyset-paginated by `event_id`. Each loaded
/// attribution is already joined to its `pool_identity`, so this re-emits it with
/// the pool attached and the source upgraded to the registry source; the upsert
/// refreshes the derived `child_miner_pool_id`. Idempotent (a re-run finds no
/// remaining `pool_id IS NULL` matchable rows). Bumps `stats.elastos_identity_updates`.
pub(crate) async fn reresolve_elastos_identity_attributions(
    client: &Client,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Result<()> {
    let source_id = get_source_id(client, ELASTOS_SOURCE_CODE).await?;
    let namespaces = [
        ELASTOS_REWARD_ADDRESS_NAMESPACE,
        ELASTOS_MINERINFO_NAMESPACE,
    ];
    let mut cursor: Option<i64> = None;

    loop {
        let rows = load_elastos_identity_reresolve_batch(
            client,
            source_id,
            cursor,
            config.batch_size,
            &namespaces,
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|row| row.event_id);

        for row in rows {
            let attributions = promoted_attributions(&row.attributions)?;
            if attributions.is_empty() {
                continue;
            }
            let promoted = attributions.len();
            upsert_event_pool_attributions_without_stale_cleanup(
                client,
                row.event_id,
                &attributions,
                row.confirmed_at,
            )
            .await
            .with_context(|| {
                format!(
                    "promote Elastos identity attributions for event {}",
                    row.event_id
                )
            })?;
            stats.elastos_identity_updates += promoted;
        }
    }

    Ok(())
}

/// Rebuild the promoted [`EventPoolAttribution`] rows from the loader's JSON array.
/// Each entry carries the stored `(namespace, match_kind, matched_value, details)`
/// plus the joined `pool_id`/`pool_identity_id`; the pool is attached and the
/// source upgraded to [`CHILD_PAYOUT_REGISTRY_SOURCE`]. Confidence stays Medium
/// (Elastos identities are RPC-decoded, unverified by AuxPoW). An unknown namespace
/// is skipped rather than guessed.
fn promoted_attributions(attributions: &serde_json::Value) -> Result<Vec<EventPoolAttribution>> {
    let entries = attributions
        .as_array()
        .context("Elastos re-resolution attributions is not a JSON array")?;
    let mut promoted = Vec::with_capacity(entries.len());
    for entry in entries {
        let namespace = entry
            .get("namespace")
            .and_then(serde_json::Value::as_str)
            .context("Elastos re-resolution row missing namespace")?;
        let Some((namespace, match_kind)) = elastos_identity_static(namespace) else {
            continue;
        };
        let matched_value = entry
            .get("matched_value")
            .and_then(serde_json::Value::as_str)
            .context("Elastos re-resolution row missing matched_value")?
            .to_owned();
        let pool_id = entry
            .get("pool_id")
            .and_then(serde_json::Value::as_i64)
            .context("Elastos re-resolution row missing pool_id")?;
        let pool_identity_id = entry
            .get("pool_identity_id")
            .and_then(serde_json::Value::as_i64)
            .context("Elastos re-resolution row missing pool_identity_id")?;
        let details = entry
            .get("details")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        promoted.push(EventPoolAttribution {
            side: PoolAttributionSide::ChildBlock,
            namespace,
            match_kind,
            matched_value,
            pool_id: Some(pool_id),
            pool_identity_id: Some(pool_identity_id),
            source: CHILD_PAYOUT_REGISTRY_SOURCE,
            confidence: PoolAttributionConfidence::Medium,
            details,
        });
    }
    Ok(promoted)
}

/// Map a stored Elastos identity namespace to its `&'static` namespace + match_kind
/// (the `EventPoolAttribution` fields are `&'static str`). Mirrors the match_kinds
/// emitted by `resolve_elastos_identity_attributions`.
fn elastos_identity_static(namespace: &str) -> Option<(&'static str, &'static str)> {
    match namespace {
        ELASTOS_MINERINFO_NAMESPACE => Some((ELASTOS_MINERINFO_NAMESPACE, "minerinfo")),
        ELASTOS_REWARD_ADDRESS_NAMESPACE => {
            Some((ELASTOS_REWARD_ADDRESS_NAMESPACE, "reward_address"))
        }
        _ => None,
    }
}
