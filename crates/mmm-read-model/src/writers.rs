//! The block/proof upsert writers and their coinbase helpers.

use std::sync::LazyLock;

use super::*;

/// Effective wrong-epoch evidence for the block row: the live classifier
/// result wins, falling back to the event rollup's `difficulty_epoch_ok`. The
/// fallback preserves a previously-proven `false` across a transient
/// `--recheck-orphans` pass (the rollup COALESCEs it in `apply_event_classification`),
/// so a momentary `None` classification never erases a proven STRICT-orphan gate.
/// Caller feeds the same value to the block column and the orphan-class gate so
/// the two cannot disagree.
pub(crate) fn merge_difficulty(
    rollup_difficulty: Option<bool>,
    classification: &ParentClassification,
) -> Option<bool> {
    classification.difficulty_epoch_ok.or(rollup_difficulty)
}

/// Project a resolved Core coinbase into the block's `btc_coinbase_*` columns.
/// Present coinbase -> `Complete` with txid/script/outputs; absent -> the
/// `Default` (`NotAttempted`, all NULL). This is
/// the only producer of the `complete` status fed into the monotonic
/// `btc_coinbase_status` merge in both write modes.
pub(crate) fn coinbase_columns(coinbase: Option<&BitcoinCoreBlockCoinbase>) -> CoreCoinbaseColumns {
    match coinbase {
        Some(coinbase) => CoreCoinbaseColumns {
            txid: Some(coinbase.txid.clone()),
            script: Some(coinbase.script.clone()),
            outputs: Some(coinbase.outputs.clone()),
            status: CoreCoinbaseStatus::Complete,
        },
        None => CoreCoinbaseColumns::default(),
    }
}

/// Resolve a Bitcoin miner `pool.id` from a raw Core coinbase script (+ outputs)
/// against the embedded pool snapshot and the live `pool` slug->id map. Returns
/// `None` when the script is absent or matches no known pool. The shared coinbase
/// resolver behind both Core-attested attribution (the `resolve_core_*` wrapper)
/// and persisted/event-derived attribution; never erases an existing pool id on
/// a no-match (callers gate on `Some`).
pub(crate) async fn resolve_bitcoin_miner_pool_id_from_coinbase<C: GenericClient>(
    client: &C,
    script: Option<&[u8]>,
    outputs: Option<&[u8]>,
) -> Result<Option<i64>> {
    let Some(script) = script else {
        return Ok(None);
    };
    let resolver = PoolResolver::from_default_snapshot().context("load embedded pool snapshot")?;
    let pool_ids_by_slug = load_pool_ids_by_slug(client).await?;
    let resolution = resolve_btc_pool_from_coinbase(script, outputs, &resolver, &pool_ids_by_slug);
    Ok(resolution.pool_id)
}

/// Resolve the Bitcoin miner `pool.id` for a `BitcoinCoreBlockCoinbase`,
/// unpacking its script/outputs onto `resolve_bitcoin_miner_pool_id_from_coinbase`.
/// Used by the synthesized/Core path and by `resolve_effective_bitcoin_miner_pool_id`
/// to prefer fresh Core coinbase evidence over event-derived attribution.
pub(crate) async fn resolve_core_coinbase_pool_id<C: GenericClient>(
    client: &C,
    coinbase: Option<&BitcoinCoreBlockCoinbase>,
) -> Result<Option<i64>> {
    resolve_bitcoin_miner_pool_id_from_coinbase(
        client,
        coinbase.map(|coinbase| coinbase.script.as_slice()),
        coinbase.map(|coinbase| coinbase.outputs.as_slice()),
    )
    .await
}

/// Load the live `pool` slug->id map used to translate a resolver pool match
/// into the stored `bitcoin_miner_pool_id`. Read against the same client/txn as
/// the write so an in-flight pool insert is visible.
pub(crate) async fn load_pool_ids_by_slug<C: GenericClient>(
    client: &C,
) -> Result<HashMap<String, i64>> {
    let rows = client
        .query("SELECT slug, id FROM pool", &[])
        .await
        .context("load pool ids by slug")?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, i64>(1)))
        .collect())
}

/// Upsert a Bitcoin Core-attested canonical block row. Internal building block
/// of [`mutation::write_core_canonical`], which adds the parent advisory lock,
/// the source-health bracket, and the post-commit dependent cascade; mutation
/// callers must enter through that command rather than call this directly.
pub(crate) async fn upsert_core_canonical_header_with_coinbase<C: GenericClient>(
    client: &C,
    header: &Header,
    height: i32,
    coinbase: Option<BitcoinCoreBlockCoinbase>,
) -> Result<()> {
    let classified = ClassifiedHeader {
        hash: header.block_hash().to_byte_array().to_vec(),
        prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
        header: *header,
        height,
        coinbase,
    };
    upsert_synthesized_canonical(client, &classified).await
}

/// Upsert a synthesized/Core-canonical block via `BlockWriteMode::SynthesizedCore`:
/// resolve the Core coinbase pool, project the coinbase columns, and write a
/// fully Core-attested `Canonical` row (live/core/pow all true,
/// `difficulty_epoch_ok = true`, no orphan class). The `SynthesizedCore` merge
/// preserves any richer event-derived state on conflict and only promotes
/// kind/height on a Core-attested canonical promotion. This is the shared write
/// body behind both the reconciler's synthesized sibling
/// (`upsert_synthesized_sibling`) and the backbone's Core header upsert; the
/// source-health bracket and cascade live in the callers, not here.
pub(crate) async fn upsert_synthesized_canonical<C: GenericClient>(
    client: &C,
    header: &ClassifiedHeader,
) -> Result<()> {
    let bitcoin_miner_pool_id =
        resolve_core_coinbase_pool_id(client, header.coinbase.as_ref()).await?;
    let coinbase = coinbase_columns(header.coinbase.as_ref());
    let block = BlockInput {
        hash: header.hash.clone(),
        prev_hash: header.prev_hash.clone(),
        height: Some(header.height),
        height_source: Some(HeightSource::BitcoinCore),
        kind: BlockKind::Canonical,
        header_bytes: serialize(&header.header),
        header_time: header.header.time as i64,
        bitcoin_miner_pool_id,
        btc_coinbase_txid: coinbase.txid,
        btc_coinbase_script: coinbase.script,
        btc_coinbase_outputs: coinbase.outputs,
        btc_coinbase_status: coinbase.status,
        canonical_competitor_hash: None,
        total_attestations: 0,
        distinct_sources: 1,
        auxpow_chain_count: 0,
        live_observed: true,
        core_attested: true,
        pow_validated: true,
        difficulty_epoch_ok: Some(true),
        first_attested_at: None,
        last_attested_at: None,
        // Synthesized canonical blocks are never unknown, so they carry no orphan
        // class; the SynthesizedCore write mode also clears it on a conflicting
        // unknown.
        btc_orphan_class: None,
    };
    upsert_block_row(client, &block, BlockWriteMode::SynthesizedCore).await
}

/// The byte-identical coinbase merge the two write modes share: Core
/// coinbase evidence only ever upgrades (COALESCE), and the status state
/// machine is monotonic (complete wins; failed only overwrites non-complete).
const COINBASE_MERGE_SET: &str = "btc_coinbase_txid = COALESCE(EXCLUDED.btc_coinbase_txid, block.btc_coinbase_txid), \
                btc_coinbase_script = COALESCE(EXCLUDED.btc_coinbase_script, block.btc_coinbase_script), \
                btc_coinbase_outputs = COALESCE(EXCLUDED.btc_coinbase_outputs, block.btc_coinbase_outputs), \
                btc_coinbase_status = CASE \
                    WHEN EXCLUDED.btc_coinbase_status = 'complete' THEN 'complete' \
                    WHEN EXCLUDED.btc_coinbase_status = 'failed' AND block.btc_coinbase_status <> 'complete' THEN 'failed' \
                    ELSE block.btc_coinbase_status \
                END";

/// The matching change-detection clauses for [`COINBASE_MERGE_SET`].
const COINBASE_MERGE_WHERE: &str = "block.btc_coinbase_txid IS DISTINCT FROM COALESCE(EXCLUDED.btc_coinbase_txid, block.btc_coinbase_txid) \
                OR block.btc_coinbase_script IS DISTINCT FROM COALESCE(EXCLUDED.btc_coinbase_script, block.btc_coinbase_script) \
                OR block.btc_coinbase_outputs IS DISTINCT FROM COALESCE(EXCLUDED.btc_coinbase_outputs, block.btc_coinbase_outputs) \
                OR block.btc_coinbase_status IS DISTINCT FROM CASE \
                    WHEN EXCLUDED.btc_coinbase_status = 'complete' THEN 'complete' \
                    WHEN EXCLUDED.btc_coinbase_status = 'failed' AND block.btc_coinbase_status <> 'complete' THEN 'failed' \
                    ELSE block.btc_coinbase_status \
                END";

/// Synthesized/Core writes only replace the effective Bitcoin miner when Core
/// coinbase evidence resolved to a concrete pool. A present-but-unmatched Core
/// coinbase script enriches the row without erasing event-derived attribution.
const SYNTHESIZED_BITCOIN_MINER_POOL_MERGE: &str = "CASE \
                    WHEN EXCLUDED.btc_coinbase_script IS NOT NULL \
                         AND EXCLUDED.bitcoin_miner_pool_id IS NOT NULL \
                    THEN EXCLUDED.bitcoin_miner_pool_id \
                    ELSE block.bitcoin_miner_pool_id \
                END";

/// The 22 shared insert columns (the event-rollup mode appends
/// `btc_orphan_class` as $23).
const BLOCK_INSERT_COLUMNS: &str = "btc_header_hash, btc_prev_header_hash, btc_height, btc_height_source, kind, \
                btc_header_bytes, btc_header_time, bitcoin_miner_pool_id, btc_coinbase_txid, \
                btc_coinbase_script, btc_coinbase_outputs, btc_coinbase_status, \
                canonical_competitor_hash, total_attestations, distinct_sources, \
                auxpow_chain_count, live_observed, \
                core_attested, pow_validated, difficulty_epoch_ok, first_attested_at, \
                last_attested_at";

/// Which conflict-update contract a block write follows. BOTH modes carry
/// and write the Core coinbase columns (the synthesized/Core-canonical path
/// is precisely the one persisting coinbase evidence); what differs is the
/// merge policy for everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockWriteMode {
    /// The reconciler rebuild from the event rollup: every
    /// derived column is overwritten from the rollup, including the orphan
    /// class.
    EventRollup,
    /// A synthesized predecessor/competitor or Core-canonical observation:
    /// preserves event-derived state (heights/kind/counters) unless a
    /// Core-attested canonical promotion applies, OR-merges the
    /// live/core/pow flags, and clears the orphan class for re-derivation.
    SynthesizedCore,
}

/// `BlockWriteMode::EventRollup` upsert: rebuild from the event
/// rollup. Every derived column (including `btc_orphan_class`, the $23 column)
/// is overwritten from EXCLUDED; only the Core coinbase columns deviate, using
/// the monotonic `COINBASE_MERGE_SET`. The WHERE guard is the column-by-column
/// DISTINCT-FROM set so an unchanged rollup is a no-op (no `updated_at` churn,
/// no spurious cascade enqueue).
static EVENT_ROLLUP_UPSERT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT INTO block ( \
                {BLOCK_INSERT_COLUMNS}, btc_orphan_class, created_at, updated_at \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
                $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, \
                $21, $22, $23, \
                extract(epoch from now())::bigint, extract(epoch from now())::bigint \
             ) \
             ON CONFLICT (btc_header_hash) DO UPDATE SET \
                btc_prev_header_hash = EXCLUDED.btc_prev_header_hash, \
                btc_height = EXCLUDED.btc_height, \
                btc_height_source = EXCLUDED.btc_height_source, \
                kind = EXCLUDED.kind, \
                btc_header_bytes = EXCLUDED.btc_header_bytes, \
                btc_header_time = EXCLUDED.btc_header_time, \
                bitcoin_miner_pool_id = EXCLUDED.bitcoin_miner_pool_id, \
                {COINBASE_MERGE_SET}, \
                canonical_competitor_hash = EXCLUDED.canonical_competitor_hash, \
                total_attestations = EXCLUDED.total_attestations, \
                distinct_sources = EXCLUDED.distinct_sources, \
                auxpow_chain_count = EXCLUDED.auxpow_chain_count, \
                live_observed = EXCLUDED.live_observed, \
                core_attested = EXCLUDED.core_attested, \
                pow_validated = EXCLUDED.pow_validated, \
                difficulty_epoch_ok = EXCLUDED.difficulty_epoch_ok, \
                first_attested_at = EXCLUDED.first_attested_at, \
                last_attested_at = EXCLUDED.last_attested_at, \
                btc_orphan_class = EXCLUDED.btc_orphan_class, \
                updated_at = extract(epoch from now())::bigint \
             WHERE block.btc_prev_header_hash IS DISTINCT FROM EXCLUDED.btc_prev_header_hash \
                OR block.btc_height IS DISTINCT FROM EXCLUDED.btc_height \
                OR block.btc_height_source IS DISTINCT FROM EXCLUDED.btc_height_source \
                OR block.kind IS DISTINCT FROM EXCLUDED.kind \
                OR block.btc_header_bytes IS DISTINCT FROM EXCLUDED.btc_header_bytes \
                OR block.btc_header_time IS DISTINCT FROM EXCLUDED.btc_header_time \
                OR block.bitcoin_miner_pool_id IS DISTINCT FROM EXCLUDED.bitcoin_miner_pool_id \
                OR {COINBASE_MERGE_WHERE} \
                OR block.canonical_competitor_hash IS DISTINCT FROM EXCLUDED.canonical_competitor_hash \
                OR block.total_attestations IS DISTINCT FROM EXCLUDED.total_attestations \
                OR block.distinct_sources IS DISTINCT FROM EXCLUDED.distinct_sources \
                OR block.auxpow_chain_count IS DISTINCT FROM EXCLUDED.auxpow_chain_count \
                OR block.live_observed IS DISTINCT FROM EXCLUDED.live_observed \
                OR block.core_attested IS DISTINCT FROM EXCLUDED.core_attested \
                OR block.pow_validated IS DISTINCT FROM EXCLUDED.pow_validated \
                OR block.difficulty_epoch_ok IS DISTINCT FROM EXCLUDED.difficulty_epoch_ok \
                OR block.first_attested_at IS DISTINCT FROM EXCLUDED.first_attested_at \
                OR block.last_attested_at IS DISTINCT FROM EXCLUDED.last_attested_at \
                OR block.btc_orphan_class IS DISTINCT FROM EXCLUDED.btc_orphan_class"
    )
});

/// `BlockWriteMode::SynthesizedCore` upsert: preserve event-derived state on
/// conflict. height/height_source/kind/canonical_competitor_hash are kept unless
/// a Core-attested canonical promotion fires (EXCLUDED.core_attested AND
/// EXCLUDED.kind='canonical' AND block.kind<>'canonical'), which adopts the Core
/// height and clears the competitor; an `unknown` block also adopts the incoming
/// kind. live_observed/core_attested are ORed together across both sides (kept in
/// lockstep per `ensure_live_core_lockstep`), pow_validated ORs, difficulty and
/// the Core coinbase only upgrade (COALESCE / `COINBASE_MERGE_SET`). `btc_orphan_class`
/// is unconditionally cleared to NULL so the next event-rollup reconcile
/// re-derives it. The WHERE guard mirrors each clause so an unchanged write is a
/// no-op.
static SYNTHESIZED_CORE_UPSERT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT INTO block ( \
                {BLOCK_INSERT_COLUMNS}, created_at, updated_at \
             ) VALUES ( \
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
                $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, \
                $21, $22, \
                extract(epoch from now())::bigint, extract(epoch from now())::bigint \
             ) \
             ON CONFLICT (btc_header_hash) DO UPDATE SET \
                btc_prev_header_hash = EXCLUDED.btc_prev_header_hash, \
                btc_height = CASE \
                    WHEN EXCLUDED.core_attested AND EXCLUDED.kind = 'canonical' AND block.kind <> 'canonical' THEN EXCLUDED.btc_height \
                    ELSE COALESCE(block.btc_height, EXCLUDED.btc_height) \
                END, \
                btc_height_source = CASE \
                    WHEN EXCLUDED.core_attested AND EXCLUDED.kind = 'canonical' AND block.kind <> 'canonical' THEN EXCLUDED.btc_height_source \
                    ELSE COALESCE(block.btc_height_source, EXCLUDED.btc_height_source) \
                END, \
                kind = CASE \
                    WHEN EXCLUDED.core_attested AND EXCLUDED.kind = 'canonical' AND block.kind <> 'canonical' THEN EXCLUDED.kind \
                    WHEN block.kind = 'unknown' THEN EXCLUDED.kind \
                    ELSE block.kind \
                END, \
                btc_header_bytes = EXCLUDED.btc_header_bytes, \
                btc_header_time = EXCLUDED.btc_header_time, \
                bitcoin_miner_pool_id = {SYNTHESIZED_BITCOIN_MINER_POOL_MERGE}, \
                {COINBASE_MERGE_SET}, \
                canonical_competitor_hash = CASE \
                    WHEN EXCLUDED.core_attested AND EXCLUDED.kind = 'canonical' AND block.kind <> 'canonical' THEN NULL \
                    ELSE block.canonical_competitor_hash \
                END, \
                distinct_sources = GREATEST(block.distinct_sources, EXCLUDED.distinct_sources), \
                live_observed = block.live_observed OR block.core_attested OR EXCLUDED.live_observed OR EXCLUDED.core_attested, \
                core_attested = block.live_observed OR block.core_attested OR EXCLUDED.live_observed OR EXCLUDED.core_attested, \
                pow_validated = block.pow_validated OR EXCLUDED.pow_validated, \
                difficulty_epoch_ok = COALESCE(block.difficulty_epoch_ok, EXCLUDED.difficulty_epoch_ok), \
                btc_orphan_class = NULL, \
                updated_at = extract(epoch from now())::bigint \
             WHERE block.btc_prev_header_hash IS DISTINCT FROM EXCLUDED.btc_prev_header_hash \
                OR block.btc_height IS DISTINCT FROM COALESCE(block.btc_height, EXCLUDED.btc_height) \
                OR (EXCLUDED.core_attested AND EXCLUDED.kind = 'canonical' AND block.kind <> 'canonical') \
                OR block.bitcoin_miner_pool_id IS DISTINCT FROM {SYNTHESIZED_BITCOIN_MINER_POOL_MERGE} \
                OR {COINBASE_MERGE_WHERE} \
                OR block.kind = 'unknown' \
                OR block.live_observed IS DISTINCT FROM block.core_attested \
                OR NOT block.live_observed \
                OR NOT block.core_attested \
                OR NOT block.pow_validated \
                OR block.difficulty_epoch_ok IS NULL"
    )
});

/// The one block writer. Replaces the upsert_block / upsert_synthesized_block
/// twins; the column-by-column contract of each mode is documented on
/// [`BlockWriteMode`] and pinned by the DB integration suites.
pub(crate) async fn upsert_block_row<C: GenericClient>(
    client: &C,
    block: &BlockInput,
    mode: BlockWriteMode,
) -> Result<()> {
    ensure_live_core_lockstep(block)?;
    let kind = block.kind.as_db_str();
    let height_source = block.height_source.map(HeightSource::as_db_str);
    let coinbase_status = block.btc_coinbase_status.as_db_str();
    let mut params: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = vec![
        &block.hash,
        &block.prev_hash,
        &block.height,
        &height_source,
        &kind,
        &block.header_bytes,
        &block.header_time,
        &block.bitcoin_miner_pool_id,
        &block.btc_coinbase_txid,
        &block.btc_coinbase_script,
        &block.btc_coinbase_outputs,
        &coinbase_status,
        &block.canonical_competitor_hash,
        &block.total_attestations,
        &block.distinct_sources,
        &block.auxpow_chain_count,
        &block.live_observed,
        &block.core_attested,
        &block.pow_validated,
        &block.difficulty_epoch_ok,
        &block.first_attested_at,
        &block.last_attested_at,
    ];
    let (sql, context): (&str, &str) = match mode {
        BlockWriteMode::EventRollup => {
            params.push(&block.btc_orphan_class);
            (EVENT_ROLLUP_UPSERT_SQL.as_str(), "upsert block")
        }
        BlockWriteMode::SynthesizedCore => (
            SYNTHESIZED_CORE_UPSERT_SQL.as_str(),
            "upsert synthesized canonical block",
        ),
    };
    client.execute(sql, &params).await.context(context)?;
    Ok(())
}

/// Invariant guard: a local read-model block must observe `live_observed ==
/// core_attested` (this deployment treats live Bitcoin observation and Core
/// attestation as the same signal; the `SynthesizedCore` merge keeps them in
/// lockstep by ORing all four flags into both columns). Bail before any write
/// if a caller hands in drifted flags, so a logic bug surfaces here rather than
/// as silently inconsistent derived state.
pub(crate) fn ensure_live_core_lockstep(block: &BlockInput) -> Result<()> {
    if block.live_observed != block.core_attested {
        bail!(
            "local read-model block {} has live/core flag drift: live_observed={}, core_attested={}",
            hex::encode(&block.hash),
            block.live_observed,
            block.core_attested
        );
    }
    Ok(())
}

/// Rebuild the per-source `attestation_proof` rows for a parent from the
/// `merge_mining_event` rollup (idempotency key `(btc_header_hash, source_id,
/// 'auxpow')`, non-`near` events only). Per source: if any events are active
/// (`revoked_at IS NULL`) the proof reflects the active set (active confirmed_at,
/// OR of pow_validates_btc_target, not revoked); if all are revoked the proof is
/// retained from the historical set and marked revoked with the latest
/// revocation reason; a source that never had a proof and has no revocation is
/// skipped. `contributing_event_ids` records the exact event ids that built the
/// proof. The DISTINCT-FROM guard suppresses no-op updates so unchanged proofs
/// do not churn `updated_at`/the cascade.
pub(crate) async fn rebuild_auxpow_proofs<C: GenericClient>(client: &C, hash: &[u8]) -> Result<()> {
    let rows = client
        .query(
            "SELECT source_id, \
                    array_agg(id ORDER BY id) FILTER (WHERE revoked_at IS NULL) AS active_ids, \
                    array_agg(id ORDER BY id) AS historical_ids, \
                    min(confirmed_at) FILTER (WHERE revoked_at IS NULL) AS active_confirmed_at, \
                    bool_or(pow_validates_btc_target) FILTER (WHERE revoked_at IS NULL) AS active_pow, \
                    min(discovered_at) AS historical_discovered_at, \
                    min(confirmed_at) AS historical_confirmed_at, \
                    bool_or(pow_validates_btc_target) AS historical_pow, \
                    max(revoked_at) AS max_revoked_at, \
                    (array_agg(revocation_reason ORDER BY revoked_at DESC NULLS LAST, id ASC))[1] AS revocation_reason \
             FROM merge_mining_event \
             WHERE btc_parent_header_hash = $1 \
               AND btc_parent_kind <> 'near' \
             GROUP BY source_id",
            &[&hash],
        )
        .await
        .context("load proof rollups")?;

    for row in rows {
        let source_id: i64 = row.get(0);
        let active_ids: Option<Vec<i64>> = row.get(1);
        let historical_ids: Vec<i64> = row.get(2);
        let (ids, discovered_at, confirmed_at, pow_validated, revoked_at, revocation_reason) =
            if let Some(active_ids) = active_ids {
                (
                    active_ids,
                    row.get::<_, Option<i64>>(5)
                        .context("historical proof discovered_at missing")?,
                    row.get::<_, Option<i64>>(3)
                        .context("active proof confirmed_at missing")?,
                    row.get::<_, Option<bool>>(4).unwrap_or(false),
                    None,
                    None::<String>,
                )
            } else {
                let max_revoked_at: Option<i64> = row.get(8);
                if max_revoked_at.is_none() {
                    continue;
                }
                (
                    historical_ids,
                    row.get::<_, i64>(5),
                    row.get::<_, i64>(6),
                    row.get::<_, Option<bool>>(7).unwrap_or(false),
                    max_revoked_at,
                    row.get(9),
                )
            };

        let evidence = json!({ "contributing_event_ids": ids });
        client
            .execute(
                "INSERT INTO attestation_proof ( \
                    btc_header_hash, source_id, proof_kind, evidence, pow_validated, \
                    discovered_at, confirmed_at, revoked_at, revocation_reason \
                 ) VALUES ( \
                    $1, $2, 'auxpow', $3, $4, $5, $6, $7, $8 \
                 ) \
                 ON CONFLICT (btc_header_hash, source_id, proof_kind) DO UPDATE SET \
                    evidence = EXCLUDED.evidence, \
                    pow_validated = EXCLUDED.pow_validated, \
                    discovered_at = EXCLUDED.discovered_at, \
                    confirmed_at = EXCLUDED.confirmed_at, \
                    revoked_at = EXCLUDED.revoked_at, \
                    revocation_reason = EXCLUDED.revocation_reason \
                 WHERE attestation_proof.evidence IS DISTINCT FROM EXCLUDED.evidence \
                    OR attestation_proof.pow_validated IS DISTINCT FROM EXCLUDED.pow_validated \
                    OR attestation_proof.discovered_at IS DISTINCT FROM EXCLUDED.discovered_at \
                    OR attestation_proof.confirmed_at IS DISTINCT FROM EXCLUDED.confirmed_at \
                    OR attestation_proof.revoked_at IS DISTINCT FROM EXCLUDED.revoked_at \
                    OR attestation_proof.revocation_reason IS DISTINCT FROM EXCLUDED.revocation_reason",
                &[
                    &hash,
                    &source_id,
                    &Json(&evidence),
                    &pow_validated,
                    &discovered_at,
                    &confirmed_at,
                    &revoked_at,
                    &revocation_reason,
                ],
            )
            .await
            .context("upsert attestation_proof")?;
    }
    Ok(())
}
