//! RSK producer base-table SQL: the event + `rsk_merge_mining_evidence` sidecar
//! capture writers, and the RSK pool / `pool_identity` adapters over the generic
//! `crate::pool` helpers.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, GenericClient};

use mmm_capture::capture::{MergeMiningEventPayload, RskEvidencePayload};
use mmm_capture::pool_resolver::{
    PoolIdentityRegistry, RSK_MINER_ADDRESS_NAMESPACE, normalize_rsk_address,
};

use crate::pool::{
    PoolIdentitySeed, upsert_pool_identities_for_namespace_with_policy, upsert_registry_only_pools,
};
use crate::{EventWriteOutcome, upsert_merge_mining_event_with_attributions};

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

    let outcome = write_rsk_capture_in_txn(&txn, source_id, payload, evidence).await?;

    txn.commit()
        .await
        .context("commit RSK capture transaction")?;
    Ok(outcome.event_id)
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
) -> Result<EventWriteOutcome> {
    let outcome = upsert_merge_mining_event_with_attributions(client, source_id, payload).await?;
    upsert_rsk_evidence(client, outcome.event_id, evidence).await?;
    Ok(outcome)
}

async fn upsert_rsk_evidence<C: GenericClient>(
    client: &C,
    event_id: i64,
    evidence: &RskEvidencePayload,
) -> Result<()> {
    let affected = client
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
                uncle_parent_height = EXCLUDED.uncle_parent_height, \
                merkle_proof = COALESCE( \
                    rsk_merge_mining_evidence.merkle_proof, \
                    EXCLUDED.merkle_proof \
                ), \
                coinbase_tail = COALESCE( \
                    rsk_merge_mining_evidence.coinbase_tail, \
                    EXCLUDED.coinbase_tail \
                ) \
             WHERE rsk_merge_mining_evidence.rsk_block_hash = EXCLUDED.rsk_block_hash \
               AND rsk_merge_mining_evidence.rsk_height = EXCLUDED.rsk_height \
               AND rsk_merge_mining_evidence.rsk_miner = EXCLUDED.rsk_miner \
               AND rsk_merge_mining_evidence.merge_mining_hash = EXCLUDED.merge_mining_hash \
               AND rsk_merge_mining_evidence.proof_format = EXCLUDED.proof_format \
               AND (rsk_merge_mining_evidence.pool_identity_id IS NULL \
                    OR EXCLUDED.pool_identity_id IS NULL \
                    OR rsk_merge_mining_evidence.pool_identity_id = EXCLUDED.pool_identity_id) \
               AND (rsk_merge_mining_evidence.merkle_proof IS NULL \
                    OR EXCLUDED.merkle_proof IS NULL \
                    OR rsk_merge_mining_evidence.merkle_proof = EXCLUDED.merkle_proof) \
               AND (rsk_merge_mining_evidence.coinbase_tail IS NULL \
                    OR EXCLUDED.coinbase_tail IS NULL \
                    OR rsk_merge_mining_evidence.coinbase_tail = EXCLUDED.coinbase_tail)",
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
    if affected != 1 {
        bail!("RSK evidence contradicts stored immutable sidecar fields");
    }
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
