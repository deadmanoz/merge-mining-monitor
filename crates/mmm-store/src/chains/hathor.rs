//! Hathor producer base-table SQL: the event + `hathor_merge_mining_evidence`
//! sidecar capture writer, the per-height event read queries, and the DB-only
//! reward-address replay loads / audit updates.

use anyhow::{Context, Result};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, GenericClient};

use mmm_capture::capture::{
    HATHOR_REVOKE_NON_BTC, HATHOR_REVOKE_SUPERSEDED, HATHOR_REVOKE_VOIDED, HathorEvidencePayload,
    MergeMiningEventPayload,
};

use crate::upsert_merge_mining_event_with_attributions;

/// Write a Hathor capture in the caller's transaction (injected as the
/// `capture_in_txn` upsert closure): upsert the event, restore a
/// reversibly-revoked row, then upsert the 1:1 evidence sidecar.
pub async fn write_hathor_capture_in_txn<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
    evidence: &HathorEvidencePayload,
) -> Result<i64> {
    let event_id = upsert_merge_mining_event_with_attributions(client, source_id, payload).await?;
    // RESTORE-AND-REFRESH, scoped by reason: a re-observation of a block that
    // was auto-revoked for a child-DAG reorg (voided/superseded) clears the
    // revocation; a hathor_nbits_classifier_conflict or any manual revoke stays
    // sticky and is never re-activated by a recapture.
    client
        .execute(
            "UPDATE merge_mining_event \
                SET revoked_at = NULL, revocation_reason = NULL \
              WHERE id = $1 AND revocation_reason IN ($2, $3, $4)",
            &[
                &event_id,
                &HATHOR_REVOKE_VOIDED,
                &HATHOR_REVOKE_SUPERSEDED,
                &HATHOR_REVOKE_NON_BTC,
            ],
        )
        .await
        .context("clear reversible Hathor revocation on recapture")?;
    upsert_hathor_evidence(client, event_id, evidence).await?;
    Ok(event_id)
}

async fn upsert_hathor_evidence<C: GenericClient>(
    client: &C,
    event_id: i64,
    evidence: &HathorEvidencePayload,
) -> Result<()> {
    let reward_output_details = evidence.reward_output_details.as_ref().map(Json);
    let reward_addresses = evidence.reward_addresses.as_ref().map(Json);
    client
        .execute(
            "INSERT INTO hathor_merge_mining_evidence ( \
                event_id, hathor_block_hash, hathor_height, aux_pow, funds_graph, \
                funds_graph_split, reward_output_details, reward_addresses, \
                expected_btc_nbits, proof_format \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (event_id) DO UPDATE SET \
                hathor_block_hash = EXCLUDED.hathor_block_hash, \
                hathor_height = EXCLUDED.hathor_height, \
                aux_pow = EXCLUDED.aux_pow, \
                funds_graph = EXCLUDED.funds_graph, \
                funds_graph_split = EXCLUDED.funds_graph_split, \
                reward_output_details = EXCLUDED.reward_output_details, \
                reward_addresses = EXCLUDED.reward_addresses, \
                expected_btc_nbits = EXCLUDED.expected_btc_nbits, \
                proof_format = EXCLUDED.proof_format",
            &[
                &event_id,
                &evidence.hathor_block_hash,
                &evidence.hathor_height,
                &evidence.aux_pow,
                &evidence.funds_graph,
                &evidence.funds_graph_split,
                &reward_output_details,
                &reward_addresses,
                &evidence.expected_btc_nbits,
                &evidence.proof_format,
            ],
        )
        .await
        .context("upsert hathor_merge_mining_evidence")?;
    Ok(())
}

/// One `merge_mining_event` row at a Hathor height, with its active flag. Used
/// by the void/supersession state machine to reconcile prior captures.
#[derive(Debug, Clone)]
pub struct HathorEventRow {
    pub event_id: i64,
    pub child_block_hash: Vec<u8>,
    pub is_active: bool,
}

/// All events (active and revoked) for a Hathor source at a child height.
pub async fn hathor_events_at_height(
    client: &Client,
    source_id: i64,
    height: i32,
) -> Result<Vec<HathorEventRow>> {
    let rows = client
        .query(
            "SELECT id, child_block_hash, (revoked_at IS NULL) AS is_active \
               FROM merge_mining_event \
              WHERE source_id = $1 AND child_height = $2",
            &[&source_id, &height],
        )
        .await
        .context("query Hathor events at height")?;
    Ok(rows
        .iter()
        .map(|row| HathorEventRow {
            event_id: row.get("id"),
            child_block_hash: row.get("child_block_hash"),
            is_active: row.get("is_active"),
        })
        .collect())
}

/// One active (non-revoked) Hathor sidecar row for DB-only reward-address replay:
/// carries the raw `funds_graph` blob to re-parse plus the pre-aggregated set of
/// existing child-block reward attributions in this namespace, so the replay can
/// apply `ExistingAttributionSet` write-policy without a second round trip.
#[derive(Debug, Clone)]
pub struct HathorRewardReplayRow {
    pub event_id: i64,
    pub confirmed_at: i64,
    pub funds_graph: Vec<u8>,
    pub funds_graph_split: i32,
    pub existing_attributions: serde_json::Value,
}

/// Page through active Hathor events for reward-address replay: keyset pagination
/// on `event_id` (pass the last row's id back as `cursor_event_id`, `None` to
/// start), capped at `batch_size`. Skips revoked events and LEFT JOINs the
/// existing `child_block` reward attributions for `namespace` limited to
/// `sources`, aggregating them per event (empty array when none) so the caller
/// never re-queries attributions per row.
pub async fn load_hathor_reward_replay_batch<C: GenericClient>(
    client: &C,
    source_id: i64,
    cursor_event_id: Option<i64>,
    batch_size: i64,
    namespace: &str,
    sources: &[&str],
) -> Result<Vec<HathorRewardReplayRow>> {
    let source_values = sources
        .iter()
        .map(|source| (*source).to_owned())
        .collect::<Vec<_>>();
    let rows = client
        .query(
            "SELECT e.id, e.confirmed_at, h.funds_graph, h.funds_graph_split, \
                    COALESCE( \
                        jsonb_agg( \
                            jsonb_build_object( \
                                'source', a.source, \
                                'namespace', a.namespace, \
                                'match_kind', a.match_kind, \
                                'matched_value', a.matched_value, \
                                'pool_id', a.pool_id, \
                                'pool_identity_id', a.pool_identity_id, \
                                'confidence', a.confidence, \
                                'details', a.details \
                            ) ORDER BY a.matched_value \
                        ) FILTER (WHERE a.id IS NOT NULL), \
                        '[]'::jsonb \
                    ) AS existing_attributions \
             FROM hathor_merge_mining_evidence h \
             JOIN merge_mining_event e ON e.id = h.event_id \
             LEFT JOIN event_pool_attribution a \
               ON a.event_id = e.id \
              AND a.side = 'child_block' \
              AND a.namespace = $4 \
              AND a.source = ANY($5::text[]) \
             WHERE e.source_id = $1 \
               AND e.revoked_at IS NULL \
               AND ($2::bigint IS NULL OR e.id > $2) \
             GROUP BY e.id, e.confirmed_at, h.funds_graph, h.funds_graph_split \
             ORDER BY e.id \
             LIMIT $3",
            &[
                &source_id,
                &cursor_event_id,
                &batch_size,
                &namespace,
                &source_values,
            ],
        )
        .await
        .context("load Hathor reward replay batch")?;

    Ok(rows
        .into_iter()
        .map(|row| HathorRewardReplayRow {
            event_id: row.get(0),
            confirmed_at: row.get(1),
            funds_graph: row.get(2),
            funds_graph_split: row.get(3),
            existing_attributions: row.get(4),
        })
        .collect())
}

/// Refresh the `reward_output_details` / `reward_addresses` audit columns on a
/// Hathor evidence row. Idempotent and change-detecting: the `IS DISTINCT FROM`
/// guard makes a no-op write touch zero rows, and the bool return (rows > 0) lets
/// the caller count only rows it actually changed.
pub async fn update_hathor_reward_audit<C: GenericClient>(
    client: &C,
    event_id: i64,
    reward_output_details: &serde_json::Value,
    reward_addresses: &serde_json::Value,
) -> Result<bool> {
    let details = Json(reward_output_details);
    let addresses = Json(reward_addresses);
    let changed = client
        .execute(
            "UPDATE hathor_merge_mining_evidence \
                SET reward_output_details = $2, reward_addresses = $3 \
              WHERE event_id = $1 \
                AND (reward_output_details IS DISTINCT FROM $2::jsonb \
                     OR reward_addresses IS DISTINCT FROM $3::jsonb)",
            &[&event_id, &details, &addresses],
        )
        .await
        .with_context(|| format!("update Hathor reward audit fields event={event_id}"))?;
    Ok(changed > 0)
}
