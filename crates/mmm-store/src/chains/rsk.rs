//! RSK producer base-table SQL: the event + `rsk_merge_mining_evidence` sidecar
//! capture writers, and the RSK pool / `pool_identity` adapters over the generic
//! `crate::pool` helpers.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio_postgres::{Client, GenericClient};

use mmm_capture::capture::{MergeMiningEventPayload, RskEvidencePayload};
use mmm_capture::pool_resolver::{
    PoolIdentityRegistry, RSK_MINER_ADDRESS_NAMESPACE, normalize_rsk_address,
};

use crate::pool::{
    PoolIdentitySeed, upsert_pool_identities_for_namespace_with_policy, upsert_registry_only_pools,
};
use crate::upsert_merge_mining_event_with_attributions;

/// Low-level fixture writer for one RSK block (canonical or uncle): opens its own
/// transaction and upserts the shared `merge_mining_event` row plus the 1:1
/// `rsk_merge_mining_evidence` sidecar.
///
/// TEST/FIXTURE ONLY: this bypasses `mmm_read_model::capture_in_txn`, so it does NOT
/// maintain `source_health`. Production RSK capture goes through `capture_in_txn`
/// injecting [`write_rsk_capture_in_txn`]. Gated behind `test`/`db-integration`
/// so it cannot become a production maintenance bypass; tests that read
/// `/sources` after using it must call `rebuild_source_health` first.
#[cfg(any(test, feature = "db-integration"))]
pub async fn write_rsk_capture(
    client: &mut Client,
    source_id: i64,
    payload: &MergeMiningEventPayload,
    evidence: &RskEvidencePayload,
) -> Result<i64> {
    let txn = client
        .transaction()
        .await
        .context("begin RSK capture transaction")?;

    let event_id = write_rsk_capture_in_txn(&txn, source_id, payload, evidence).await?;

    txn.commit()
        .await
        .context("commit RSK capture transaction")?;
    Ok(event_id)
}

/// Write an RSK capture in the caller's transaction (injected as the
/// `capture_in_txn` upsert closure): upsert the shared `merge_mining_event` row
/// plus the 1:1 `rsk_merge_mining_evidence` sidecar. Production RSK capture
/// reaches this through `mmm_read_model::capture_in_txn`, which owns
/// `source_health`; this fn writes only the base + sidecar rows.
pub async fn write_rsk_capture_in_txn<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
    evidence: &RskEvidencePayload,
) -> Result<i64> {
    let event_id = upsert_merge_mining_event_with_attributions(client, source_id, payload).await?;
    upsert_rsk_evidence(client, event_id, evidence).await?;
    Ok(event_id)
}

async fn upsert_rsk_evidence<C: GenericClient>(
    client: &C,
    event_id: i64,
    evidence: &RskEvidencePayload,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO rsk_merge_mining_evidence ( \
                event_id, rsk_block_hash, rsk_height, is_uncle, uncle_index, \
                uncle_parent_height, rsk_miner, pool_identity_id, \
                merge_mining_hash, merkle_proof, coinbase_tail, \
                proof_format \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12 \
             ) \
             ON CONFLICT (event_id) DO UPDATE SET \
                pool_identity_id = COALESCE( \
                    rsk_merge_mining_evidence.pool_identity_id, \
                    EXCLUDED.pool_identity_id \
                ), \
                is_uncle = EXCLUDED.is_uncle, \
                uncle_index = EXCLUDED.uncle_index, \
                uncle_parent_height = EXCLUDED.uncle_parent_height",
            &[
                &event_id,
                &evidence.rsk_block_hash,
                &evidence.rsk_height,
                &evidence.is_uncle,
                &evidence.uncle_index,
                &evidence.uncle_parent_height,
                &evidence.rsk_miner,
                &evidence.pool_identity_id,
                &evidence.merge_mining_hash,
                &evidence.merkle_proof,
                &evidence.coinbase_tail,
                &evidence.proof_format,
            ],
        )
        .await
        .context("upsert rsk_merge_mining_evidence")?;
    Ok(())
}

/// Ensure `pool` rows exist for every slug the RSK miner registry references.
/// Existing slugs (from the bootstrap snapshot) are preserved untouched;
/// missing slugs are created with empty `coinbase_tags` / `payout_addresses`
/// because the only attribution path for these pools is `pool_identity`.
/// Returns the resulting slug -> pool.id map, mutated in place.
pub async fn upsert_rsk_only_pools(
    client: &Client,
    registry: &PoolIdentityRegistry,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<()> {
    let definitions = registry.distinct_pool_definitions();
    upsert_registry_only_pools(client, "RSK miner registry", &definitions, pool_ids_by_slug).await
}

/// Upsert one pool_identity row per registry entry. Returns the
/// identifier -> pool_identity.id map keyed by the registry's
/// (case-preserved) miner_address. Existing identities mapped to a different
/// pool are treated as conflicts by default; replay callers with an explicit
/// overwrite flag can opt into remapping through
/// [`upsert_rsk_pool_identities_with_policy`].
pub async fn upsert_rsk_pool_identities(
    client: &Client,
    registry: &PoolIdentityRegistry,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> Result<HashMap<String, i64>> {
    upsert_rsk_pool_identities_with_policy(client, registry, pool_ids_by_slug, false).await
}

/// Upsert one pool_identity row per registry entry, optionally refusing to
/// remap existing identities to a different pool. Non-overwrite replay paths
/// use this to ensure registry enrichment cannot silently rewrite already
/// resolved historical attribution via the shared identity row.
pub async fn upsert_rsk_pool_identities_with_policy(
    client: &Client,
    registry: &PoolIdentityRegistry,
    pool_ids_by_slug: &HashMap<String, i64>,
    remap_existing: bool,
) -> Result<HashMap<String, i64>> {
    let seeds = registry
        .rsk_registry()
        .entries
        .iter()
        .map(|entry| {
            PoolIdentitySeed::new(
                normalize_rsk_address(&entry.miner_address),
                &entry.pool_slug,
            )
        })
        .collect::<Vec<_>>();
    upsert_pool_identities_for_namespace_with_policy(
        client,
        RSK_MINER_ADDRESS_NAMESPACE,
        &seeds,
        pool_ids_by_slug,
        remap_existing,
        "rerun with --overwrite to remap",
    )
    .await
}

/// Late-fill the RSK sidecar's pool identity pointer from a replayed miner
/// registry match. This is intentionally one-way: unresolved or stale replay
/// evidence must never erase or replace an already-recorded identity.
pub async fn late_fill_rsk_pool_identity_id<C: GenericClient>(
    client: &C,
    event_id: i64,
    pool_identity_id: i64,
) -> Result<bool> {
    let changed = client
        .execute(
            "UPDATE rsk_merge_mining_evidence \
                SET pool_identity_id = $2 \
              WHERE event_id = $1 \
                AND pool_identity_id IS NULL",
            &[&event_id, &pool_identity_id],
        )
        .await
        .with_context(|| {
            format!("late-fill rsk_merge_mining_evidence.pool_identity_id event={event_id}")
        })?;
    Ok(changed > 0)
}

/// Active-set fingerprint of one source's non-revoked events: the row count plus
/// an order-independent 64-bit XOR digest of the active event ids. Lets the
/// `reclassify-pools` RSK pass detect any change to the active RSK set (add,
/// revoke, restore, or a balanced swap), not just appended ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RskActiveSetFingerprint {
    pub active_event_count: i64,
    pub active_event_digest: i64,
}

/// The stored RSK reclassify watermark (keyed singleton): the embedded registry
/// content hash and the active-set fingerprint captured at the last successful
/// non-overwrite RSK pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RskReclassifyWatermark {
    pub registry_hash: String,
    pub fingerprint: RskActiveSetFingerprint,
}

/// Compute the live active-set fingerprint over the RSK source's non-revoked
/// events in ONE aggregate: `(count, bit_xor(hashtextextended(id::text, 0)))`.
/// `hashtextextended` is the documented 64-bit Postgres hash; XOR is
/// order-independent and self-cancelling (`x XOR x = 0`), so a revoke-then-restore
/// of the same id reproduces the digest exactly while any net membership change
/// flips it. A bounded scan of the `source_id` partition, not a full table scan.
pub async fn rsk_active_set_fingerprint<C: GenericClient>(
    client: &C,
    rsk_source_id: i64,
) -> Result<RskActiveSetFingerprint> {
    let row = client
        .query_one(
            "SELECT count(*)::bigint, \
                    COALESCE(bit_xor(hashtextextended(id::text, 0)), 0)::bigint \
               FROM merge_mining_event \
              WHERE source_id = $1 AND revoked_at IS NULL",
            &[&rsk_source_id],
        )
        .await
        .context("compute RSK active-set fingerprint")?;
    Ok(RskActiveSetFingerprint {
        active_event_count: row.get(0),
        active_event_digest: row.get(1),
    })
}

/// Read the keyed-singleton RSK reclassify watermark, or `None` if no successful
/// pass has written it yet (so a fresh database never short-circuits).
pub async fn load_rsk_reclassify_watermark<C: GenericClient>(
    client: &C,
) -> Result<Option<RskReclassifyWatermark>> {
    let row = client
        .query_opt(
            "SELECT registry_hash, active_event_count, active_event_digest \
               FROM rsk_reclassify_watermark \
              WHERE id = TRUE",
            &[],
        )
        .await
        .context("load RSK reclassify watermark")?;
    Ok(row.map(|row| RskReclassifyWatermark {
        registry_hash: row.get(0),
        fingerprint: RskActiveSetFingerprint {
            active_event_count: row.get(1),
            active_event_digest: row.get(2),
        },
    }))
}

/// Upsert the keyed-singleton RSK reclassify watermark after a successful
/// non-overwrite pass. `ON CONFLICT (id)` keeps exactly one logical row.
pub async fn upsert_rsk_reclassify_watermark<C: GenericClient>(
    client: &C,
    registry_hash: &str,
    fingerprint: RskActiveSetFingerprint,
    completed_at: i64,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO rsk_reclassify_watermark \
                 (id, registry_hash, active_event_count, active_event_digest, completed_at) \
             VALUES (TRUE, $1, $2, $3, $4) \
             ON CONFLICT (id) DO UPDATE SET \
                 registry_hash = EXCLUDED.registry_hash, \
                 active_event_count = EXCLUDED.active_event_count, \
                 active_event_digest = EXCLUDED.active_event_digest, \
                 completed_at = EXCLUDED.completed_at",
            &[
                &registry_hash,
                &fingerprint.active_event_count,
                &fingerprint.active_event_digest,
                &completed_at,
            ],
        )
        .await
        .context("upsert RSK reclassify watermark")?;
    Ok(())
}
