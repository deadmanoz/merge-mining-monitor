//! Row loads, dependent finders, repair candidates, and advisory locks.

use super::*;

/// Aggregate every active (non-`near`, non-revoked) auxpow attestation of a
/// parent into the rollup the `block` row is rebuilt from: attestation count,
/// distinct sources, distinct auxpow chains, OR-folded pow/difficulty flags,
/// and the attestation time window. Returns `None` when no active event
/// references the hash, which is the signal for `rebuild_parent_read_model` to
/// take the zero-active demotion path. Core's own observation is NOT counted
/// here; the reconciler adds the +1 distinct-source for `core_attested` when it
/// compares against the persisted block.
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
                  AND e.btc_parent_kind <> 'near' \
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

/// Pick the one active event whose parent-header bytes and coinbase represent
/// the parent for the rebuilt `block` row. Ordering prefers a non-NULL coinbase
/// script, then lowest `child_height`/`id`, so the chosen header carries pool
/// evidence when any attestation has it. Calls `ensure_active_parent_headers_match`
/// first to fail loudly if active events disagree on the header bytes for this
/// hash, since the derived block can only commit to one header.
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
               AND btc_parent_kind <> 'near' \
               AND revoked_at IS NULL \
             ORDER BY (btc_parent_coinbase_script IS NULL), child_height, id \
             LIMIT 1",
            &[&hash],
        )
        .await
        .context("load representative active event")?;
    let event = event_from_row(&row)?;
    ensure_active_parent_headers_match(client, hash, &event.btc_parent_header_bytes).await?;
    Ok(event)
}

/// Bail if any active event for this parent hash carries different header bytes
/// than the representative. The derived `block` row commits to exactly one
/// header; divergent bytes under one hash mean corrupt evidence rather than a
/// reconcilable state, so we return an error instead of silently picking
/// one. Called by `load_representative_active_event` before the header is used.
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
               AND btc_parent_kind <> 'near' \
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

/// Load the derived `block` fields that decide whether a rebuild actually
/// changed anything and where the cascade must propagate: kind, height (+source),
/// competitor hash, core-attested flag, difficulty flag, coinbase script.
/// Returns `None` when no `block` row exists yet. Mutation/reconcile paths read
/// this as the before/after pair around a parent rebuild; equality of the two
/// snapshots is the idempotency key that decides whether to enqueue dependents
/// and whether `(before != after)` reports a changed hash.
pub(crate) async fn load_block_cascade_state<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<BlockCascadeState>> {
    let row = client
        .query_opt(
            "SELECT kind, btc_height, btc_height_source, canonical_competitor_hash, \
                    core_attested, difficulty_epoch_ok, btc_coinbase_script \
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
            canonical_competitor_hash: row.get(3),
            core_attested: row.get(4),
            difficulty_epoch_ok: row.get(5),
            btc_coinbase_script: row.get(6),
        })
    })
    .transpose()
}

/// Load one `merge_mining_event` by id into the typed form, including revoked
/// rows (the reconciler re-reads the same id across lock acquisition to detect
/// drift, so this must NOT filter on `revoked_at`). Errors if the id is absent.
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

/// Decode a `merge_mining_event` row into the typed event. Shared by the
/// by-id, representative, and anchor loaders so the column projection stays in
/// one place; the column order here must match every SELECT that feeds it.
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

/// Parse the `merge_mining_event.btc_parent_kind` enum string. Note `near` is a
/// real stored value (proximity attestation) while orphan-ness is NOT here: it
/// is the derived `block.btc_orphan_class`, never a parent-kind. Errors on any
/// other string so a schema/enum drift fails loudly rather than mis-classifies.
pub(crate) fn parent_kind_from_db(value: &str) -> Result<ParentKind> {
    match value {
        "canonical" => Ok(ParentKind::Canonical),
        "stale" => Ok(ParentKind::Stale),
        "near" => Ok(ParentKind::Near),
        "unknown" => Ok(ParentKind::Unknown),
        other => bail!("unknown merge_mining_event.btc_parent_kind {other:?}"),
    }
}

/// Find the anchor event for a block hash: the lowest-`child_height`/`id`
/// active (non-`near`, non-revoked) event attesting this parent. The reconciler
/// uses it to reconcile a block-grain change through the event path that owns
/// the parent rebuild, so block and event reconciliation converge on one row.
/// Returns `None` for a block with no surviving attestations.
pub(crate) async fn find_anchor_event_for_block<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<i64>> {
    let row = client
        .query_opt(
            "SELECT id \
             FROM merge_mining_event \
             WHERE btc_parent_header_hash = $1 \
               AND btc_parent_kind <> 'near' \
               AND revoked_at IS NULL \
             ORDER BY child_height, id \
             LIMIT 1",
            &[&hash],
        )
        .await
        .context("find anchor event for block")?;
    Ok(row.map(|row| row.get(0)))
}

/// Find active, pow-validating events whose parent's PREV-hash is this hash:
/// the next-block-up dependents in the chain-link cascade. When a parent's
/// derived state changes, its children's `canonical_competitor`/sibling
/// reasoning may change, so `enqueue_dependents` re-queues these. Restricted to
/// `pow_validates_btc_target` because only valid-PoW links anchor a successor.
pub(crate) async fn find_child_events<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Vec<i64>> {
    let rows = client
        .query(
            "SELECT id \
             FROM merge_mining_event \
             WHERE btc_parent_prev_header_hash = $1 \
               AND btc_parent_kind <> 'near' \
               AND pow_validates_btc_target \
               AND revoked_at IS NULL \
             ORDER BY child_height, id",
            &[&hash],
        )
        .await
        .context("find child events")?;
    Ok(rows.into_iter().map(|row| row.get(0)).collect())
}

/// Find derived `block` rows whose `btc_prev_header_hash` is this hash: the
/// block-grain successors to re-queue when a parent's derived state changes.
/// Complements `find_child_events` (which finds successors via event rows that
/// have no materialized block yet); both feed `enqueue_dependents`.
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

/// Find stale blocks whose canonical competitor is this hash. A structural
/// change to the canonical block (kind/height/validity) can invalidate the stale
/// side, so `enqueue_dependents` re-queues each stale side for rebuild. This is
/// the canonical-to-stale edge of the cascade that the chain-link finders do not
/// cover.
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

/// A repair candidate emitted by `load_reconcile_candidates`, tagged by grain:
/// `Event(id)` reconciles through the parent-rebuild path, `Block(hash)`
/// reconciles an orphan/zero-attestation block that no active event references.
/// The runner dispatches on this in lib.rs.
#[derive(Debug)]
pub(crate) enum ReconcileCandidate {
    Event(i64),
    Block(Vec<u8>),
}

/// The missing-only repair-candidate scan: one statement yielding both
/// event-grain and block-grain candidates (orphan blocks, unclassified
/// unknown events, zero-attestation rows), height/source-scoped by the
/// reconcile config.
const RECONCILE_CANDIDATES_SQL: &str = "WITH parent_scope AS ( \
                SELECT e.btc_parent_header_hash, min(e.child_height) AS sort_child_height, min(e.id) AS sort_event_id \
                FROM merge_mining_event e \
                WHERE e.btc_parent_kind <> 'near' \
                  AND e.revoked_at IS NULL \
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
                WHERE e.btc_parent_kind <> 'near' AND e.revoked_at IS NULL \
                GROUP BY e.btc_parent_header_hash \
             ), representative_source AS ( \
                SELECT DISTINCT ON (e.btc_parent_header_hash) \
                       e.btc_parent_header_hash, e.btc_parent_kind, e.btc_parent_height, \
                       b.kind AS block_kind, canonical_b.kind AS competitor_kind \
                FROM merge_mining_event e \
                JOIN parent_scope ps ON ps.btc_parent_header_hash = e.btc_parent_header_hash \
                LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                LEFT JOIN block canonical_b ON canonical_b.btc_header_hash = b.canonical_competitor_hash \
                WHERE e.btc_parent_kind <> 'near' \
                  AND e.revoked_at IS NULL \
                ORDER BY e.btc_parent_header_hash, e.child_height, e.id \
             ), representative_event AS ( \
                SELECT btc_parent_header_hash, expected_kind, \
                       CASE WHEN expected_kind IN ('canonical','stale') \
                            THEN btc_parent_height \
                            ELSE NULL END AS expected_height \
                FROM ( \
                    SELECT btc_parent_header_hash, btc_parent_height, \
                           CASE \
                                WHEN btc_parent_kind = 'canonical' THEN 'canonical' \
                                WHEN btc_parent_kind = 'stale' \
                                     AND ((block_kind = 'stale' AND competitor_kind = 'canonical') \
                                          OR ($5::boolean AND block_kind IS NULL)) THEN 'stale' \
                                ELSE 'unknown' END AS expected_kind \
                    FROM representative_source \
                ) selected_representative \
             ), proof_scope AS ( \
                SELECT e.btc_parent_header_hash, e.source_id, jsonb_agg(e.id ORDER BY e.id) AS ids \
                FROM merge_mining_event e \
                WHERE e.btc_parent_kind <> 'near' AND e.revoked_at IS NULL \
                GROUP BY e.btc_parent_header_hash, e.source_id \
             ), event_candidates AS ( \
                SELECT 'event'::text AS kind, min(e.id) AS event_id, NULL::bytea AS hash, \
                       min(e.child_height) AS sort_child_height, min(e.id) AS sort_event_id \
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
                WHERE e.btc_parent_kind <> 'near' AND e.revoked_at IS NULL \
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
                       OR (re.expected_kind = 'unknown' AND b.btc_height_source IS NOT NULL) \
                       OR b.difficulty_epoch_ok IS DISTINCT FROM pr.difficulty_epoch_ok \
                       OR ap.evidence -> 'contributing_event_ids' IS DISTINCT FROM p.ids \
                       OR (b.kind = 'stale' AND canonical_b.kind IS DISTINCT FROM 'canonical')) \
                GROUP BY e.btc_parent_header_hash \
             ), block_candidates AS ( \
                SELECT 'block'::text AS kind, NULL::bigint AS event_id, b.btc_header_hash AS hash, \
                       2147483647 AS sort_child_height, 9223372036854775807::bigint AS sort_event_id \
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

/// Run the missing-only repair scan (`RECONCILE_CANDIDATES_SQL`) and decode the
/// dual-grain rows into `ReconcileCandidate`s, ordered so event candidates sort
/// ahead of block candidates and the `LIMIT` is a deterministic batch. Resolves
/// the optional source-code filter to an id first. `classifier_enabled` ($5)
/// gates the "stale parent with no block yet" candidate: only treat a missing
/// block as stale-pending when a classifier could actually confirm it, else it
/// stays unknown and is not flagged.
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

/// Take the per-block `pg_advisory_xact_lock` for one hash, keyed by
/// `hashtextextended(hex(hash), BLOCK_LOCK_SEED)` so the lock key is stable
/// across processes and the seed namespaces it away from other advisory locks.
/// Held until the transaction ends; it serializes concurrent rebuilds of the
/// same parent so the source_health before/after bracket is consistent. Callers
/// that lock more than one hash MUST go through `lock_block_hashes` for ordered
/// acquisition; calling this directly for a single, already-canonical hash is
/// fine (see `lock_parent_hash_in_txn`).
pub(crate) async fn lock_block_hash<C: GenericClient>(client: &C, hash: &[u8]) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock(hashtextextended(encode($1::bytea, 'hex'), $2::bigint))",
            &[&hash, &BLOCK_LOCK_SEED],
        )
        .await
        .context("acquire block advisory lock")?;
    Ok(())
}

/// Acquire per-hash advisory locks for a set, sorted and deduped FIRST. Global
/// sort order is the deadlock-avoidance contract: every path that locks more
/// than one block hash must acquire them in the same total order, so two
/// transactions touching an overlapping set can never hold-and-wait in a cycle.
/// All callers funnel the parent + prev + classification + competitor hashes
/// through here for exactly that reason; never hand-lock a multi-hash set out
/// of order.
pub(crate) async fn lock_block_hashes<C: GenericClient>(
    client: &C,
    hashes: &[Vec<u8>],
) -> Result<()> {
    let mut hashes = hashes.to_vec();
    hashes.sort();
    hashes.dedup();
    for hash in hashes {
        lock_block_hash(client, &hash).await?;
    }
    Ok(())
}
