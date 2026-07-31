//! merge_mining_event upserts and event_pool_attribution writes/deletes.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::GenericClient;
use tokio_postgres::types::Json;

use mmm_capture::capture::{
    CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE, EventPoolAttribution,
    HistoricalEventProvenance, MergeMiningEventPayload, PoolAttributionSide,
};

/// How an event upsert changed the store's authenticated child-identity state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventWriteDisposition {
    Inserted,
    Updated,
    Promoted,
    SatisfiedByExistingExact,
}

/// The persisted event identity and the store-owned write disposition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventWriteOutcome {
    pub event_id: i64,
    pub disposition: EventWriteDisposition,
}

impl EventWriteOutcome {
    const fn new(event_id: i64, disposition: EventWriteDisposition) -> Self {
        Self {
            event_id,
            disposition,
        }
    }
}

/// Upsert one source-specific merge-mining observation using its strongest
/// authenticated child identity.
///
/// A real child hash is exact identity. A hashless row uses
/// `(source_id, child_height, btc_parent_header_hash)` as partial identity.
/// Exact observations refine one unambiguous matching partial row in place;
/// hashless observations already represented by one exact row return that row
/// instead of minting a duplicate. Conflicts fill only missing evidence and
/// reject contradictory non-null values.
pub async fn upsert_merge_mining_event<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<EventWriteOutcome> {
    if payload.child_block_hash.is_none() && payload.child_height.is_none() {
        bail!("merge-mining observation requires a child hash or child height");
    }
    match payload.child_block_hash.as_deref() {
        Some(child_hash) => {
            if exact_event_id(client, source_id, child_hash)
                .await?
                .is_some()
            {
                return insert_event(client, source_id, payload, Exactness::Exact).await;
            }
            if let Some(child_height) = payload.child_height
                && let Some(id) =
                    matching_partial_event_id(client, source_id, child_height, payload).await?
            {
                let event_id = promote_partial_event(client, id, payload).await?;
                return Ok(EventWriteOutcome::new(
                    event_id,
                    EventWriteDisposition::Promoted,
                ));
            }
            insert_event(client, source_id, payload, Exactness::Exact).await
        }
        None => {
            let child_height = payload
                .child_height
                .expect("hashless identity checked to have a height");
            let exact_ids =
                matching_exact_event_ids(client, source_id, child_height, payload).await?;
            match exact_ids.as_slice() {
                [] => insert_event(client, source_id, payload, Exactness::Partial).await,
                [id] => {
                    fill_existing_event(client, *id, payload).await?;
                    Ok(EventWriteOutcome::new(
                        *id,
                        EventWriteDisposition::SatisfiedByExistingExact,
                    ))
                }
                _ => bail!(
                    "hashless observation ambiguously matches {} exact events for source {source_id} child height {child_height}",
                    exact_ids.len()
                ),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Exactness {
    Exact,
    Partial,
}

async fn exact_event_id<C: GenericClient>(
    client: &C,
    source_id: i64,
    child_hash: &[u8],
) -> Result<Option<i64>> {
    let row = client
        .query_opt(
            "SELECT id FROM merge_mining_event \
             WHERE source_id = $1 AND child_block_hash = $2 \
             FOR UPDATE",
            &[&source_id, &child_hash],
        )
        .await
        .context("look up exact child observation")?;
    Ok(row.map(|row| row.get(0)))
}

async fn matching_partial_event_id<C: GenericClient>(
    client: &C,
    source_id: i64,
    child_height: i32,
    payload: &MergeMiningEventPayload,
) -> Result<Option<i64>> {
    let row = client
        .query_opt(
            "SELECT id FROM merge_mining_event \
             WHERE source_id = $1 \
               AND child_height = $2 \
               AND btc_parent_header_hash = $3 \
               AND child_block_hash IS NULL \
             FOR UPDATE",
            &[&source_id, &child_height, &payload.btc_parent_header_hash],
        )
        .await
        .context("look up matching partial child observation")?;
    Ok(row.map(|row| row.get(0)))
}

async fn matching_exact_event_ids<C: GenericClient>(
    client: &C,
    source_id: i64,
    child_height: i32,
    payload: &MergeMiningEventPayload,
) -> Result<Vec<i64>> {
    let rows = client
        .query(
            "SELECT id FROM merge_mining_event \
             WHERE source_id = $1 \
               AND child_height = $2 \
               AND btc_parent_header_hash = $3 \
               AND child_block_hash IS NOT NULL \
             ORDER BY id \
             LIMIT 2 \
             FOR UPDATE",
            &[&source_id, &child_height, &payload.btc_parent_header_hash],
        )
        .await
        .context("look up exact observations matching partial identity")?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

async fn promote_partial_event<C: GenericClient>(
    client: &C,
    event_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<i64> {
    let child_nbits = payload.child_nbits.map(i64::from);
    let row = client
        .query_opt(
            "UPDATE merge_mining_event SET \
                child_block_hash = $2, \
                child_header_bytes = COALESCE(child_header_bytes, $3), \
                child_block_time = COALESCE(child_block_time, $4), \
                child_nbits = COALESCE(child_nbits, $5), \
                confirmed_at = GREATEST(confirmed_at, $6), \
                child_coinbase_txid = COALESCE(child_coinbase_txid, $7), \
                child_coinbase_script = COALESCE(child_coinbase_script, $8), \
                child_coinbase_outputs = COALESCE(child_coinbase_outputs, $9), \
                btc_parent_coinbase_txid = COALESCE(btc_parent_coinbase_txid, $10), \
                btc_parent_coinbase_script = COALESCE(btc_parent_coinbase_script, $11), \
                btc_parent_coinbase_outputs = COALESCE(btc_parent_coinbase_outputs, $12), \
                aux_merkle_proof = COALESCE(aux_merkle_proof, $13) \
             WHERE id = $1 \
               AND child_block_hash IS NULL \
               AND btc_parent_header_bytes = $14 \
               AND (child_header_bytes IS NULL OR $3::bytea IS NULL OR child_header_bytes = $3) \
               AND (child_block_time IS NULL OR $4::bigint IS NULL OR child_block_time = $4) \
               AND (child_nbits IS NULL OR $5::bigint IS NULL OR child_nbits = $5) \
             RETURNING id",
            &[
                &event_id,
                &payload.child_block_hash,
                &payload.child_header_bytes,
                &payload.child_block_time,
                &child_nbits,
                &payload.confirmed_at,
                &payload.child_coinbase_txid,
                &payload.child_coinbase_script,
                &payload.child_coinbase_outputs,
                &payload.btc_parent_coinbase_txid,
                &payload.btc_parent_coinbase_script,
                &payload.btc_parent_coinbase_outputs,
                &payload.aux_merkle_proof,
                &payload.btc_parent_header_bytes,
            ],
        )
        .await
        .context("promote partial merge_mining_event")?;
    row.map(|row| row.get(0)).ok_or_else(|| {
        anyhow::anyhow!("exact refinement contradicts the stored partial observation")
    })
}

async fn fill_existing_event<C: GenericClient>(
    client: &C,
    event_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<()> {
    let row = client
        .query_opt(
            "UPDATE merge_mining_event SET \
                confirmed_at = GREATEST(confirmed_at, $2), \
                btc_parent_coinbase_txid = COALESCE(btc_parent_coinbase_txid, $3), \
                btc_parent_coinbase_script = COALESCE(btc_parent_coinbase_script, $4), \
                btc_parent_coinbase_outputs = COALESCE(btc_parent_coinbase_outputs, $5), \
                child_block_time = COALESCE(child_block_time, $6), \
                child_nbits = COALESCE(child_nbits, $7), \
                child_coinbase_txid = COALESCE(child_coinbase_txid, $8), \
                child_coinbase_script = COALESCE(child_coinbase_script, $9), \
                child_coinbase_outputs = COALESCE(child_coinbase_outputs, $10), \
                aux_merkle_proof = COALESCE(aux_merkle_proof, $11) \
             WHERE id = $1 \
               AND btc_parent_header_hash = $12 \
               AND btc_parent_header_bytes = $13 \
               AND (child_block_time IS NULL OR $6::bigint IS NULL OR child_block_time = $6) \
               AND (child_nbits IS NULL OR $7::bigint IS NULL OR child_nbits = $7) \
             RETURNING id",
            &[
                &event_id,
                &payload.confirmed_at,
                &payload.btc_parent_coinbase_txid,
                &payload.btc_parent_coinbase_script,
                &payload.btc_parent_coinbase_outputs,
                &payload.child_block_time,
                &payload.child_nbits.map(i64::from),
                &payload.child_coinbase_txid,
                &payload.child_coinbase_script,
                &payload.child_coinbase_outputs,
                &payload.aux_merkle_proof,
                &payload.btc_parent_header_hash,
                &payload.btc_parent_header_bytes,
            ],
        )
        .await
        .context("fill exact event from partial observation")?;
    if row.is_none() {
        bail!("partial child evidence contradicts existing exact observation");
    }
    Ok(())
}

async fn insert_event<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
    exactness: Exactness,
) -> Result<EventWriteOutcome> {
    let btc_parent_kind = payload.btc_parent_kind.as_db_str();
    let child_nbits = payload.child_nbits.map(i64::from);
    let conflict = match exactness {
        Exactness::Exact => {
            "ON CONFLICT (source_id, child_block_hash) \
             WHERE child_block_hash IS NOT NULL DO UPDATE SET"
        }
        Exactness::Partial => {
            "ON CONFLICT (source_id, child_height, btc_parent_header_hash) \
             WHERE child_block_hash IS NULL AND child_height IS NOT NULL DO UPDATE SET"
        }
    };
    let sql = format!(
        "INSERT INTO merge_mining_event ( \
            source_id, child_height, child_block_hash, child_header_bytes, \
            child_block_time, child_nbits, \
            btc_parent_header_hash, btc_parent_prev_header_hash, \
            btc_parent_header_bytes, btc_parent_header_time, \
            btc_parent_height, btc_parent_kind, \
            pow_validates_btc_target, pow_validates_child_target, \
            difficulty_epoch_ok, btc_parent_coinbase_txid, \
            btc_parent_coinbase_script, btc_parent_coinbase_outputs, \
            child_coinbase_txid, child_coinbase_script, child_coinbase_outputs, \
            aux_merkle_proof, discovered_at, confirmed_at, revoked_at, revocation_reason \
         ) VALUES ( \
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
            $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, \
            $21, $22, $23, $24, $25, $26 \
         ) \
         {conflict} \
            confirmed_at = GREATEST(merge_mining_event.confirmed_at, EXCLUDED.confirmed_at), \
            child_height = COALESCE(merge_mining_event.child_height, EXCLUDED.child_height), \
            child_header_bytes = COALESCE(merge_mining_event.child_header_bytes, EXCLUDED.child_header_bytes), \
            child_block_time = COALESCE(merge_mining_event.child_block_time, EXCLUDED.child_block_time), \
            child_nbits = COALESCE(merge_mining_event.child_nbits, EXCLUDED.child_nbits), \
            btc_parent_coinbase_txid = COALESCE(merge_mining_event.btc_parent_coinbase_txid, EXCLUDED.btc_parent_coinbase_txid), \
            btc_parent_coinbase_script = COALESCE(merge_mining_event.btc_parent_coinbase_script, EXCLUDED.btc_parent_coinbase_script), \
            btc_parent_coinbase_outputs = COALESCE(merge_mining_event.btc_parent_coinbase_outputs, EXCLUDED.btc_parent_coinbase_outputs), \
            child_coinbase_txid = COALESCE(merge_mining_event.child_coinbase_txid, EXCLUDED.child_coinbase_txid), \
            child_coinbase_script = COALESCE(merge_mining_event.child_coinbase_script, EXCLUDED.child_coinbase_script), \
            child_coinbase_outputs = COALESCE(merge_mining_event.child_coinbase_outputs, EXCLUDED.child_coinbase_outputs), \
            aux_merkle_proof = COALESCE(merge_mining_event.aux_merkle_proof, EXCLUDED.aux_merkle_proof) \
         WHERE merge_mining_event.btc_parent_header_hash = EXCLUDED.btc_parent_header_hash \
           AND merge_mining_event.btc_parent_header_bytes = EXCLUDED.btc_parent_header_bytes \
           AND (merge_mining_event.child_height IS NULL OR EXCLUDED.child_height IS NULL OR merge_mining_event.child_height = EXCLUDED.child_height) \
           AND (merge_mining_event.child_header_bytes IS NULL OR EXCLUDED.child_header_bytes IS NULL OR merge_mining_event.child_header_bytes = EXCLUDED.child_header_bytes) \
           AND (merge_mining_event.child_block_time IS NULL OR EXCLUDED.child_block_time IS NULL OR merge_mining_event.child_block_time = EXCLUDED.child_block_time) \
           AND (merge_mining_event.child_nbits IS NULL OR EXCLUDED.child_nbits IS NULL OR merge_mining_event.child_nbits = EXCLUDED.child_nbits) \
         RETURNING id, (xmax = 0) AS inserted"
    );
    let row = client
        .query_opt(
            &sql,
            &[
                &source_id,
                &payload.child_height,
                &payload.child_block_hash,
                &payload.child_header_bytes,
                &payload.child_block_time,
                &child_nbits,
                &payload.btc_parent_header_hash,
                &payload.btc_parent_prev_header_hash,
                &payload.btc_parent_header_bytes,
                &payload.btc_parent_header_time,
                &payload.btc_parent_height,
                &btc_parent_kind,
                &payload.pow_validates_btc_target,
                &payload.pow_validates_child_target,
                &payload.difficulty_epoch_ok,
                &payload.btc_parent_coinbase_txid,
                &payload.btc_parent_coinbase_script,
                &payload.btc_parent_coinbase_outputs,
                &payload.child_coinbase_txid,
                &payload.child_coinbase_script,
                &payload.child_coinbase_outputs,
                &payload.aux_merkle_proof,
                &payload.discovered_at,
                &payload.confirmed_at,
                &payload.revoked_at,
                &payload.revocation_reason,
            ],
        )
        .await
        .context("upsert merge_mining_event")?;
    row.map(|row| {
        let event_id = row.get(0);
        let disposition = if row.get(1) {
            EventWriteDisposition::Inserted
        } else {
            EventWriteDisposition::Updated
        };
        EventWriteOutcome::new(event_id, disposition)
    })
    .ok_or_else(|| anyhow::anyhow!("merge-mining observation contradicts stored evidence"))
}

/// Upsert the event row, then apply a COMPLETE pool-attribution snapshot for it:
/// [`upsert_merge_mining_event`] followed by [`upsert_event_pool_attributions`]
/// (which prunes now-absent rows per source). This is the normal producer capture
/// path (auxpow family, historical ingest), where `payload.pool_attributions` is
/// the full set. When you only hold a partial set, call the plain upsert plus
/// [`upsert_event_pool_attributions_without_stale_cleanup`] instead. Returns the
/// store-owned event identity and write disposition.
pub async fn upsert_merge_mining_event_with_attributions<C: GenericClient>(
    client: &C,
    source_id: i64,
    payload: &MergeMiningEventPayload,
) -> Result<EventWriteOutcome> {
    let outcome = upsert_merge_mining_event(client, source_id, payload).await?;
    upsert_event_pool_attributions(
        client,
        outcome.event_id,
        &payload.pool_attributions,
        payload.confirmed_at,
    )
    .await?;
    if let Some(provenance) = &payload.historical_provenance {
        upsert_historical_event_provenance(client, outcome.event_id, provenance).await?;
    }
    Ok(outcome)
}

async fn upsert_historical_event_provenance<C: GenericClient>(
    client: &C,
    event_id: i64,
    provenance: &HistoricalEventProvenance,
) -> Result<()> {
    if provenance.source_row_number <= 0 {
        bail!("historical source_row_number must be positive");
    }
    let row = client
        .query_opt(
            "INSERT INTO historical_event_provenance ( \
                event_id, publication_commit, chain, source_kind, source_path, \
                source_row_number, artifact_scope, provenance, classification, \
                btc_height, validation_status, btc_stale_relevance, relevance_reason \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13 \
             ) \
             ON CONFLICT (publication_commit, chain, source_path, source_row_number) DO UPDATE SET \
                event_id = EXCLUDED.event_id \
             WHERE historical_event_provenance.event_id = EXCLUDED.event_id \
               AND historical_event_provenance.source_kind = EXCLUDED.source_kind \
               AND historical_event_provenance.artifact_scope = EXCLUDED.artifact_scope \
               AND historical_event_provenance.provenance = EXCLUDED.provenance \
               AND historical_event_provenance.classification = EXCLUDED.classification \
               AND historical_event_provenance.btc_height IS NOT DISTINCT FROM EXCLUDED.btc_height \
               AND historical_event_provenance.validation_status IS NOT DISTINCT FROM EXCLUDED.validation_status \
               AND historical_event_provenance.btc_stale_relevance IS NOT DISTINCT FROM EXCLUDED.btc_stale_relevance \
               AND historical_event_provenance.relevance_reason IS NOT DISTINCT FROM EXCLUDED.relevance_reason \
             RETURNING event_id",
            &[
                &event_id,
                &provenance.publication_commit,
                &provenance.chain,
                &provenance.source_kind,
                &provenance.source_path,
                &provenance.source_row_number,
                &provenance.artifact_scope,
                &provenance.provenance,
                &provenance.classification,
                &provenance.btc_height,
                &provenance.validation_status,
                &provenance.btc_stale_relevance,
                &provenance.relevance_reason,
            ],
        )
        .await
        .context("upsert historical_event_provenance")?;
    if row.is_none() {
        bail!(
            "historical publication row contradicts stored provenance for {}:{}:{} row {}",
            provenance.publication_commit,
            provenance.chain,
            provenance.source_path,
            provenance.source_row_number
        );
    }
    Ok(())
}

/// Apply a COMPLETE pool-attribution snapshot for an event: prune rows whose
/// `(side, source, namespace, matched_value)` keys are absent from `attributions`
/// (per source, with child-payout sources unioned across the side), upsert the
/// supplied rows, then refresh the denormalized child-miner pool. Use this when
/// the caller owns the full set for the touched sources; use
/// [`upsert_event_pool_attributions_without_stale_cleanup`] when it only holds a
/// partial add/upgrade set. `observed_at` is the first/last-seen timestamp.
pub async fn upsert_event_pool_attributions<C: GenericClient>(
    client: &C,
    event_id: i64,
    attributions: &[EventPoolAttribution],
    observed_at: i64,
) -> Result<()> {
    delete_stale_event_pool_attributions_for_sources(client, event_id, attributions).await?;
    upsert_event_pool_attribution_rows(client, event_id, attributions, observed_at).await?;
    refresh_event_child_miner_pool_id(client, event_id).await
}

/// Upsert attribution rows without deleting absent rows for the same source.
///
/// Historical repair commands use this when they have a partial set of rows to
/// add or upgrade, not a complete source snapshot for the event.
pub async fn upsert_event_pool_attributions_without_stale_cleanup<C: GenericClient>(
    client: &C,
    event_id: i64,
    attributions: &[EventPoolAttribution],
    observed_at: i64,
) -> Result<()> {
    upsert_event_pool_attribution_rows(client, event_id, attributions, observed_at).await?;
    refresh_event_child_miner_pool_id(client, event_id).await
}

/// Upsert one `event_pool_attribution` row per attribution, idempotent on
/// `(event_id, side, namespace, matched_value)`. On conflict an unknown-pool
/// (`pool_id IS NULL`) re-capture never clobbers an existing known-pool row: the
/// match/source/confidence/details/identity columns are kept from the known side,
/// while `last_seen_at` advances monotonically (`GREATEST`). Shared write half of
/// both attribution entry points; the stale-pruning policy lives in the callers.
async fn upsert_event_pool_attribution_rows<C: GenericClient>(
    client: &C,
    event_id: i64,
    attributions: &[EventPoolAttribution],
    observed_at: i64,
) -> Result<()> {
    for attribution in attributions {
        let side = attribution.side.as_db_str();
        let confidence = attribution.confidence.as_db_str();
        client
            .execute(
                "INSERT INTO event_pool_attribution ( \
                    event_id, side, namespace, match_kind, matched_value, \
                    pool_id, pool_identity_id, source, confidence, details, \
                    first_seen_at, last_seen_at \
                 ) VALUES ( \
                    $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12 \
                 ) \
                 ON CONFLICT (event_id, side, namespace, matched_value) DO UPDATE SET \
                    match_kind = CASE \
                        WHEN EXCLUDED.pool_id IS NULL \
                             AND event_pool_attribution.pool_id IS NOT NULL \
                            THEN event_pool_attribution.match_kind \
                        ELSE EXCLUDED.match_kind \
                    END, \
                    pool_id = CASE \
                        WHEN EXCLUDED.pool_id IS NOT NULL THEN EXCLUDED.pool_id \
                        ELSE event_pool_attribution.pool_id \
                    END, \
                    pool_identity_id = CASE \
                        WHEN EXCLUDED.pool_id IS NOT NULL THEN EXCLUDED.pool_identity_id \
                        WHEN event_pool_attribution.pool_id IS NOT NULL \
                            THEN event_pool_attribution.pool_identity_id \
                        ELSE EXCLUDED.pool_identity_id \
                    END, \
                    source = CASE \
                        WHEN EXCLUDED.pool_id IS NULL \
                             AND event_pool_attribution.pool_id IS NOT NULL \
                            THEN event_pool_attribution.source \
                        ELSE EXCLUDED.source \
                    END, \
                    confidence = CASE \
                        WHEN EXCLUDED.pool_id IS NULL \
                             AND event_pool_attribution.pool_id IS NOT NULL \
                            THEN event_pool_attribution.confidence \
                        ELSE EXCLUDED.confidence \
                    END, \
                    details = CASE \
                        WHEN EXCLUDED.pool_id IS NULL \
                             AND event_pool_attribution.pool_id IS NOT NULL \
                            THEN event_pool_attribution.details \
                        ELSE EXCLUDED.details \
                    END, \
                    last_seen_at = GREATEST( \
                        event_pool_attribution.last_seen_at, \
                        EXCLUDED.last_seen_at \
                    )",
                &[
                    &event_id,
                    &side,
                    &attribution.namespace,
                    &attribution.match_kind,
                    &attribution.matched_value,
                    &attribution.pool_id,
                    &attribution.pool_identity_id,
                    &attribution.source,
                    &confidence,
                    &Json(&attribution.details),
                    &observed_at,
                    &observed_at,
                ],
            )
            .await
            .with_context(|| {
                format!(
                    "upsert event_pool_attribution event={} side={} namespace={} value={}",
                    event_id, side, attribution.namespace, attribution.matched_value
                )
            })?;
    }
    Ok(())
}

/// Recompute and persist `merge_mining_event.child_miner_pool_id` from the
/// event's `child_block` attribution rows. This is the single writer of that
/// denormalized column: every attribution write/delete entry point calls it,
/// so the per-event child miner is always the projection of the current rows
/// (one distinct known pool, else NULL).
async fn refresh_event_child_miner_pool_id<C: GenericClient>(
    client: &C,
    event_id: i64,
) -> Result<()> {
    client
        .execute(
            "UPDATE merge_mining_event \
             SET child_miner_pool_id = ( \
                SELECT CASE WHEN count(DISTINCT pool_id) = 1 \
                            THEN min(pool_id) \
                            ELSE NULL END \
                FROM event_pool_attribution \
                WHERE event_id = $1 \
                  AND side = 'child_block' \
                  AND pool_id IS NOT NULL \
             ) \
             WHERE id = $1",
            &[&event_id],
        )
        .await
        .with_context(|| format!("refresh child miner pool for event {event_id}"))?;
    Ok(())
}

/// Prune `event_pool_attribution` rows for the event whose `(side, source)` is
/// present in `attributions` but whose `(namespace, matched_value)` key is no
/// longer retained, i.e. enforce the supplied set as the complete snapshot for
/// each touched source. Child-payout sources
/// (`CHILD_COINBASE_OUTPUT_SOURCE`/`CHILD_PAYOUT_REGISTRY_SOURCE`) share a
/// keep-set across the side so a multi-source payout set is pruned as one.
/// Sources absent from `attributions` are left untouched. The cleanup half of
/// [`upsert_event_pool_attributions`].
async fn delete_stale_event_pool_attributions_for_sources<C: GenericClient>(
    client: &C,
    event_id: i64,
    attributions: &[EventPoolAttribution],
) -> Result<()> {
    let mut retained_keys_by_source: BTreeMap<(String, String), Vec<(String, String)>> =
        BTreeMap::new();
    for attribution in attributions {
        let entry = retained_keys_by_source
            .entry((
                attribution.side.as_db_str().to_owned(),
                attribution.source.to_owned(),
            ))
            .or_default();
        entry.push((
            attribution.namespace.to_owned(),
            attribution.matched_value.clone(),
        ));
    }

    for ((side, source), mut retained_keys) in retained_keys_by_source {
        if is_child_payout_source(&source) {
            for attribution in attributions {
                if attribution.side.as_db_str() == side
                    && is_child_payout_source(attribution.source)
                {
                    let key = (
                        attribution.namespace.to_owned(),
                        attribution.matched_value.clone(),
                    );
                    if !retained_keys.contains(&key) {
                        retained_keys.push(key);
                    }
                }
            }
        }
        let (namespaces, matched_values): (Vec<_>, Vec<_>) = retained_keys.into_iter().unzip();
        client
            .execute(
                "DELETE FROM event_pool_attribution existing \
                 WHERE existing.event_id = $1 \
                   AND existing.side = $2 \
                   AND existing.source = $3 \
                   AND NOT EXISTS ( \
                       SELECT 1 \
                       FROM unnest($4::text[], $5::text[]) AS keep(namespace, matched_value) \
                       WHERE keep.namespace = existing.namespace \
                         AND keep.matched_value = existing.matched_value \
                   )",
                &[
                    &event_id,
                    &side,
                    &source,
                    &namespaces,
                    &matched_values,
                ],
            )
            .await
            .with_context(|| {
                format!(
                    "delete stale event_pool_attribution rows event={event_id} side={side} source={source}"
                )
            })?;
    }
    Ok(())
}

/// True for the child-payout attribution sources whose rows are pruned as a
/// single cross-source keep-set (coinbase output + payout registry).
fn is_child_payout_source(source: &str) -> bool {
    matches!(
        source,
        CHILD_COINBASE_OUTPUT_SOURCE | CHILD_PAYOUT_REGISTRY_SOURCE
    )
}

/// Delete ALL `event_pool_attribution` rows for one `(event_id, side, source)`,
/// then refresh the denormalized child-miner pool. An unconditional clear of a
/// whole source on a side (unlike the snapshot-prune in
/// [`upsert_event_pool_attributions`]); reclassify-pools uses it to drop a stale
/// pool-snapshot-sourced attribution before re-upserting the new one.
pub async fn delete_event_pool_attributions_for_source<C: GenericClient>(
    client: &C,
    event_id: i64,
    side: PoolAttributionSide,
    source: &str,
) -> Result<()> {
    let side = side.as_db_str();
    client
        .execute(
            "DELETE FROM event_pool_attribution \
             WHERE event_id = $1 AND side = $2 AND source = $3",
            &[&event_id, &side, &source],
        )
        .await
        .with_context(|| {
            format!("delete {source} {side} pool attributions for event {event_id}")
        })?;
    refresh_event_child_miner_pool_id(client, event_id).await?;
    Ok(())
}

/// One-way fill for child coinbase bytes recovered after the original event
/// capture. Existing non-NULL fields always win.
pub async fn fill_event_child_coinbase<C: GenericClient>(
    client: &C,
    event_id: i64,
    child_coinbase_txid: &[u8],
    child_coinbase_script: &[u8],
    child_coinbase_outputs: &[u8],
) -> Result<bool> {
    let changed = client
        .execute(
            "UPDATE merge_mining_event \
                SET child_coinbase_txid = COALESCE(child_coinbase_txid, $2), \
                    child_coinbase_script = COALESCE(child_coinbase_script, $3), \
                    child_coinbase_outputs = COALESCE(child_coinbase_outputs, $4) \
              WHERE id = $1 \
                AND (child_coinbase_txid IS NULL \
                     OR child_coinbase_script IS NULL \
                     OR child_coinbase_outputs IS NULL)",
            &[
                &event_id,
                &child_coinbase_txid,
                &child_coinbase_script,
                &child_coinbase_outputs,
            ],
        )
        .await
        .with_context(|| format!("fill child coinbase fields for event {event_id}"))?;
    Ok(changed > 0)
}
