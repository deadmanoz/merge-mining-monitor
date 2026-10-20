//! Row loads, dependent finders, repair candidates, and advisory locks.

use super::*;

/// Aggregate active (non-revoked) auxpow attestations for a parent.
/// Returns `None` when no active event references `hash`.
pub(crate) async fn load_parent_rollup<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<ParentRollup>> {
    let row = client
        .query_opt(
            "WITH active_events AS ( \
                SELECT e.*, s.chain, s.kind AS source_kind \
                FROM merge_mining_event e \
                JOIN source s ON s.id = e.source_id \
                WHERE e.btc_parent_header_hash = $1 \
                  AND e.revoked_at IS NULL \
             ) \
             SELECT count(*)::int, \
                    count(DISTINCT e.source_id)::int, \
                    (count(DISTINCT e.chain) FILTER (WHERE e.source_kind = 'auxpow'))::int, \
                    bool_or(e.pow_validates_btc_target), \
                    bool_or(e.difficulty_epoch_ok), \
                    min(e.discovered_at), \
                    max(e.confirmed_at) \
             FROM active_events e",
            &[&hash],
        )
        .await
        .context("load parent rollup")?;

    let Some(row) = row else {
        return Ok(None);
    };
    let total: i32 = row.get(0);
    if total == 0 {
        return Ok(None);
    }

    Ok(Some(ParentRollup {
        total_attestations: total,
        distinct_sources: row.get(1),
        auxpow_chain_count: row.get(2),
        pow_validated: row.get::<_, Option<bool>>(3).unwrap_or(false),
        difficulty_epoch_ok: row.get(4),
        first_attested_at: row.get(5),
        last_attested_at: row.get(6),
    }))
}

/// Select the representative active event for a parent (preferring non-NULL
/// coinbase script, then lowest `child_height`/`id`) and verify all active
/// events agree on parent header bytes.
pub(crate) async fn load_representative_active_event<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<MergeMiningEvent> {
    let row = client
        .query_one(
            "SELECT id, btc_parent_header_hash, \
                    btc_parent_prev_header_hash, btc_parent_header_bytes, \
                    btc_parent_header_time, btc_parent_height, btc_parent_kind, \
                    pow_validates_btc_target, difficulty_epoch_ok, \
                    btc_parent_coinbase_script, btc_parent_coinbase_outputs \
             FROM merge_mining_event \
             WHERE btc_parent_header_hash = $1 \
               AND revoked_at IS NULL \
             ORDER BY (btc_parent_coinbase_script IS NULL), child_height NULLS LAST, id \
             LIMIT 1",
            &[&hash],
        )
        .await
        .context("load representative active event")?;
    let event = event_from_row(&row)?;
    ensure_active_parent_headers_match(client, hash, &event.btc_parent_header_bytes).await?;
    Ok(event)
}

pub(crate) async fn ensure_active_parent_headers_match<C: GenericClient>(
    client: &C,
    hash: &[u8],
    header_bytes: &[u8],
) -> Result<()> {
    let row = client
        .query_opt(
            "SELECT id \
             FROM merge_mining_event \
             WHERE btc_parent_header_hash = $1 \
               AND revoked_at IS NULL \
               AND btc_parent_header_bytes <> $2 \
             LIMIT 1",
            &[&hash, &header_bytes],
        )
        .await
        .context("check active parent header byte consistency")?;
    if let Some(row) = row {
        let event_id: i64 = row.get(0);
        bail!(
            "active merge_mining_event {} disagrees on parent header bytes for {}",
            event_id,
            hex::encode(hash)
        );
    }
    Ok(())
}

/// Load the derived `block` fields used to detect cascade-relevant changes.
pub(crate) async fn load_block_cascade_state<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<BlockCascadeState>> {
    let row = client
        .query_opt(
            "SELECT kind, btc_height, btc_height_source, error_block_reason, \
                    canonical_competitor_hash, core_attested, difficulty_epoch_ok, btc_coinbase_script \
             FROM block \
             WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await
        .context("load block cascade state")?;

    row.map(|row| {
        let kind: String = row.get(0);
        let height_source: Option<String> = row.get(2);
        Ok(BlockCascadeState {
            kind: BlockKind::from_db_str(&kind)?,
            btc_height: row.get(1),
            btc_height_source: height_source
                .as_deref()
                .map(HeightSource::from_db_str)
                .transpose()?,
            error_block_reason: row.get(3),
            canonical_competitor_hash: row.get(4),
            core_attested: row.get(5),
            difficulty_epoch_ok: row.get(6),
            btc_coinbase_script: row.get(7),
        })
    })
    .transpose()
}

pub(crate) async fn load_event<C: GenericClient>(
    client: &C,
    event_id: i64,
) -> Result<MergeMiningEvent> {
    let row = client
        .query_one(
            "SELECT id, btc_parent_header_hash, \
                    btc_parent_prev_header_hash, btc_parent_header_bytes, \
                    btc_parent_header_time, btc_parent_height, btc_parent_kind, \
                    pow_validates_btc_target, difficulty_epoch_ok, \
                    btc_parent_coinbase_script, btc_parent_coinbase_outputs \
             FROM merge_mining_event \
             WHERE id = $1",
            &[&event_id],
        )
        .await
        .with_context(|| format!("load merge_mining_event {event_id}"))?;
    event_from_row(&row)
}

pub(crate) fn event_from_row(row: &Row) -> Result<MergeMiningEvent> {
    let kind: String = row.get(6);
    Ok(MergeMiningEvent {
        id: row.get(0),
        btc_parent_header_hash: row.get(1),
        btc_parent_prev_header_hash: row.get(2),
        btc_parent_header_bytes: row.get(3),
        btc_parent_header_time: row.get(4),
        btc_parent_height: row.get(5),
        btc_parent_kind: parent_kind_from_db(&kind)?,
        pow_validates_btc_target: row.get(7),
        difficulty_epoch_ok: row.get(8),
        btc_parent_coinbase_script: row.get(9),
        btc_parent_coinbase_outputs: row.get(10),
    })
}

pub(crate) fn parent_kind_from_db(value: &str) -> Result<ParentKind> {
    match value {
        "canonical" => Ok(ParentKind::Canonical),
        "stale" => Ok(ParentKind::Stale),
        "error_block" => Ok(ParentKind::ErrorBlock),
        "unknown" => Ok(ParentKind::Unknown),
        other => bail!("unknown merge_mining_event.btc_parent_kind {other:?}"),
    }
}

/// Lowest-`child_height`/`id` active event attesting `hash`.
pub(crate) async fn find_anchor_event_for_block<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<i64>> {
    let row = client
        .query_opt(
            "SELECT id \
             FROM merge_mining_event \
             WHERE btc_parent_header_hash = $1 \
               AND revoked_at IS NULL \
             ORDER BY child_height NULLS LAST, id \
             LIMIT 1",
            &[&hash],
        )
        .await
        .context("find anchor event for block")?;
    Ok(row.map(|row| row.get(0)))
}

/// Active, PoW-validating events whose parent `prev_hash` equals `hash`.
pub(crate) async fn find_child_events<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Vec<i64>> {
    let rows = client
        .query(
            "SELECT id \
             FROM merge_mining_event \
             WHERE btc_parent_prev_header_hash = $1 \
               AND pow_validates_btc_target \
               AND revoked_at IS NULL \
             ORDER BY child_height NULLS LAST, id",
            &[&hash],
        )
        .await
        .context("find child events")?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

pub(crate) async fn find_derived_child_blocks<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Vec<Vec<u8>>> {
    let rows = client
        .query(
            "SELECT btc_header_hash \
             FROM block \
             WHERE btc_prev_header_hash = $1 \
             ORDER BY btc_height, btc_header_hash",
            &[&hash],
        )
        .await
        .context("find derived child blocks")?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

pub(crate) async fn find_stale_competitor_dependents<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Vec<Vec<u8>>> {
    let rows = client
        .query(
            "SELECT btc_header_hash \
             FROM block \
             WHERE kind = 'stale' \
               AND canonical_competitor_hash = $1 \
             ORDER BY btc_header_hash",
            &[&hash],
        )
        .await
        .context("find stale competitor dependents")?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

#[derive(Debug)]
pub(crate) enum ReconcileCandidate {
    Event(i64),
    Block(Vec<u8>),
}

const RECONCILE_CANDIDATES_SQL: &str = "WITH parent_scope AS ( \
                SELECT e.btc_parent_header_hash, COALESCE(min(e.child_height)::bigint, 2147483648::bigint) AS sort_child_height, min(e.id) AS sort_event_id \
                FROM merge_mining_event e \
                WHERE e.revoked_at IS NULL \
                  AND ($1::int IS NULL OR e.child_height >= $1) \
                  AND ($2::int IS NULL OR e.child_height <= $2) \
                  AND ($3::bigint IS NULL OR e.source_id = $3) \
                GROUP BY e.btc_parent_header_hash \
             ), parent_rollup AS ( \
                SELECT e.btc_parent_header_hash, count(*)::int AS total_attestations, \
                       count(DISTINCT e.source_id)::int AS distinct_sources, \
                       (count(DISTINCT s.chain) FILTER (WHERE s.kind = 'auxpow'))::int AS auxpow_chain_count, \
                       min(e.discovered_at) AS first_attested_at, max(e.confirmed_at) AS last_attested_at, \
                       bool_or(e.pow_validates_btc_target) AS pow_validated, \
                       bool_or(e.difficulty_epoch_ok) AS difficulty_epoch_ok \
                FROM merge_mining_event e \
                JOIN source s ON s.id = e.source_id \
                WHERE e.revoked_at IS NULL \
                GROUP BY e.btc_parent_header_hash \
             ), representative_source AS ( \
                SELECT DISTINCT ON (e.btc_parent_header_hash) \
                       e.btc_parent_header_hash, e.btc_parent_kind, e.btc_parent_height, \
                       b.kind AS block_kind, canonical_b.kind AS competitor_kind \
                FROM merge_mining_event e \
                JOIN parent_scope ps ON ps.btc_parent_header_hash = e.btc_parent_header_hash \
                LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                LEFT JOIN block canonical_b ON canonical_b.btc_header_hash = b.canonical_competitor_hash \
                WHERE e.revoked_at IS NULL \
                ORDER BY e.btc_parent_header_hash, e.child_height NULLS LAST, e.id \
             ), representative_event AS ( \
                SELECT btc_parent_header_hash, expected_kind, \
                       CASE WHEN expected_kind IN ('canonical','stale','error_block') \
                            THEN btc_parent_height \
                            ELSE NULL END AS expected_height \
                FROM ( \
                    SELECT btc_parent_header_hash, btc_parent_height, \
                           CASE \
                                WHEN btc_parent_kind = 'canonical' THEN 'canonical' \
                                WHEN btc_parent_kind = 'stale' \
                                     AND ((block_kind = 'stale' AND competitor_kind = 'canonical') \
                                          OR ($5::boolean AND block_kind IS NULL)) THEN 'stale' \
                                WHEN btc_parent_kind = 'error_block' THEN 'error_block' \
                                ELSE 'unknown' END AS expected_kind \
                    FROM representative_source \
                ) selected_representative \
             ), proof_scope AS ( \
                SELECT e.btc_parent_header_hash, e.source_id, jsonb_agg(e.id ORDER BY e.id) AS ids \
                FROM merge_mining_event e \
                WHERE e.revoked_at IS NULL \
                GROUP BY e.btc_parent_header_hash, e.source_id \
             ), event_candidates AS ( \
                SELECT 'event'::text AS kind, min(e.id) AS event_id, NULL::bytea AS hash, \
                       COALESCE(min(e.child_height)::bigint, 2147483648::bigint) AS sort_child_height, min(e.id) AS sort_event_id \
                FROM merge_mining_event e \
                JOIN parent_scope ps ON ps.btc_parent_header_hash = e.btc_parent_header_hash \
                JOIN parent_rollup pr ON pr.btc_parent_header_hash = e.btc_parent_header_hash \
                JOIN representative_event re ON re.btc_parent_header_hash = e.btc_parent_header_hash \
                LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                LEFT JOIN block canonical_b ON canonical_b.btc_header_hash = b.canonical_competitor_hash \
                LEFT JOIN proof_scope p ON p.btc_parent_header_hash = e.btc_parent_header_hash AND p.source_id = e.source_id \
                LEFT JOIN attestation_proof ap ON ap.btc_header_hash = p.btc_parent_header_hash \
                                               AND ap.source_id = p.source_id \
                                               AND ap.proof_kind = 'auxpow' \
                WHERE e.revoked_at IS NULL \
                  AND (b.btc_header_hash IS NULL \
                       OR b.total_attestations IS DISTINCT FROM pr.total_attestations \
                       OR b.distinct_sources IS DISTINCT FROM pr.distinct_sources + CASE WHEN b.core_attested THEN 1 ELSE 0 END \
                       OR b.auxpow_chain_count IS DISTINCT FROM pr.auxpow_chain_count \
                       OR b.first_attested_at IS DISTINCT FROM pr.first_attested_at \
                       OR b.last_attested_at IS DISTINCT FROM pr.last_attested_at \
                       OR b.pow_validated IS DISTINCT FROM pr.pow_validated \
                       OR b.kind IS DISTINCT FROM re.expected_kind \
                       OR b.btc_height IS DISTINCT FROM re.expected_height \
                       OR (re.expected_kind IN ('canonical','stale') AND b.btc_height_source IS NULL) \
                       OR (re.expected_kind = 'error_block' \
                           AND b.btc_height_source IS DISTINCT FROM 'error-block-catalog' \
                           AND b.btc_height_source IS DISTINCT FROM 'prev-canonical' \
                           AND b.btc_height_source IS DISTINCT FROM 'prev-stale') \
                       OR (re.expected_kind = 'unknown' AND b.btc_height_source IS NOT NULL) \
                       OR b.difficulty_epoch_ok IS DISTINCT FROM pr.difficulty_epoch_ok \
                       OR ap.evidence -> 'contributing_event_ids' IS DISTINCT FROM p.ids \
                       OR (b.kind = 'stale' AND canonical_b.kind IS DISTINCT FROM 'canonical')) \
                GROUP BY e.btc_parent_header_hash \
             ), block_candidates AS ( \
                SELECT 'block'::text AS kind, NULL::bigint AS event_id, b.btc_header_hash AS hash, \
                       2147483649::bigint AS sort_child_height, 9223372036854775807::bigint AS sort_event_id \
                FROM block b \
                LEFT JOIN parent_rollup pr ON pr.btc_parent_header_hash = b.btc_header_hash \
                WHERE pr.btc_parent_header_hash IS NULL \
                  AND ( \
                    (b.core_attested = FALSE \
                     AND (b.kind <> 'unknown' OR b.total_attestations <> 0)) \
                    OR EXISTS (SELECT 1 FROM attestation_proof ap WHERE ap.btc_header_hash = b.btc_header_hash AND ap.revoked_at IS NULL) \
                  ) \
             ) \
             SELECT kind, event_id, hash \
             FROM (SELECT * FROM event_candidates UNION ALL SELECT * FROM block_candidates) candidates \
             ORDER BY sort_child_height, sort_event_id, kind, hash \
             LIMIT $4";

pub(crate) async fn load_reconcile_candidates(
    client: &Client,
    config: &ReconcileReadModelConfig,
    classifier_enabled: bool,
) -> Result<Vec<ReconcileCandidate>> {
    let source_id = match &config.source_code {
        Some(code) => Some(get_source_id(client, code).await?),
        None => None,
    };

    let rows = client
        .query(
            RECONCILE_CANDIDATES_SQL,
            &[
                &config.start_height,
                &config.end_height,
                &source_id,
                &config.batch_size,
                &classifier_enabled,
            ],
        )
        .await
        .context("load missing read-model candidates")?;

    rows.into_iter()
        .map(|row| {
            let kind: String = row.get(0);
            match kind.as_str() {
                "event" => Ok(ReconcileCandidate::Event(row.get(1))),
                "block" => Ok(ReconcileCandidate::Block(row.get(2))),
                other => bail!("unknown candidate kind {other:?}"),
            }
        })
        .collect()
}

/// Acquire the per-block transaction advisory lock (`hashtextextended(hex(hash), BLOCK_LOCK_SEED)`).
/// Callers locking multiple hashes must use [`lock_block_hashes`].
pub async fn lock_block_hash<C: GenericClient>(client: &C, hash: &[u8]) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock(hashtextextended(encode($1::bytea, 'hex'), $2::bigint))",
            &[&hash, &BLOCK_LOCK_SEED],
        )
        .await
        .context("acquire block advisory lock")?;
    Ok(())
}

/// Acquire per-hash advisory locks in sorted, deduplicated order to prevent deadlocks.
pub async fn lock_block_hashes<C: GenericClient>(client: &C, hashes: &[Vec<u8>]) -> Result<()> {
    let mut hashes = hashes.to_vec();
    hashes.sort();
    hashes.dedup();
    for hash in hashes {
        lock_block_hash(client, &hash).await?;
    }
    Ok(())
}
