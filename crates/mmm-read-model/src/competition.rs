//! Zero-active demotion and orphan-class derivation.

use super::*;

/// Zero-active demotion: rewrite `block` when its event rollup has no active
/// events left (all revoked). A core-attested block is preserved as the backbone
/// row Core still vouches for (kind, height, competitor, single distinct source,
/// `pow_validated=true`, persisted Core-coinbase miner reattributed); a non-core
/// block collapses to a fully-revoked `unknown` husk (counters zeroed,
/// `pow_validated=false`, `btc_orphan_class` cleared, miner NULL). The
/// `pow_validated=false` husk is what the api orphan index filters out so it does
/// not masquerade as a genuine PoW-valid unknown.
pub(crate) async fn demote_zero_active_block<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<()> {
    let core_pool_id = resolve_persisted_core_coinbase_bitcoin_miner_pool_id(client, hash).await?;
    client
        .execute(
            "UPDATE block \
             SET kind = CASE WHEN core_attested THEN kind ELSE 'unknown' END, \
                 btc_height = CASE WHEN core_attested THEN btc_height ELSE NULL END, \
                 btc_height_source = CASE WHEN core_attested THEN btc_height_source ELSE NULL END, \
                 canonical_competitor_hash = CASE WHEN core_attested THEN canonical_competitor_hash ELSE NULL END, \
                 total_attestations = 0, \
                 distinct_sources = CASE WHEN core_attested THEN 1 ELSE 0 END, \
                 auxpow_chain_count = 0, \
                 bitcoin_miner_pool_id = CASE WHEN core_attested THEN $2::bigint ELSE NULL END, \
                 pow_validated = core_attested, \
                 difficulty_epoch_ok = CASE WHEN core_attested THEN difficulty_epoch_ok ELSE NULL END, \
                 first_attested_at = NULL, \
                 last_attested_at = NULL, \
                 btc_orphan_class = NULL, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE btc_header_hash = $1",
            &[&hash, &core_pool_id],
        )
        .await
        .context("demote zero-active block")?;
    Ok(())
}

/// Derive `block.btc_orphan_class` for the reconciled parent. Canonical/stale
/// blocks carry NULL. An unknown block is freshly classified only when this pass
/// carries a Core-absence verdict (`core_absence_attested`); otherwise the
/// persisted value is preserved (mirrors `effective_classification`'s
/// preserve-under-transient-unknown behaviour) so a Core-off or transient
/// reconcile never wipes a real orphan class.
pub(crate) async fn compute_block_orphan_class<C: GenericClient>(
    client: &C,
    hash: &[u8],
    kind: BlockKind,
    classification: &ParentClassification,
    header: &Header,
    difficulty_epoch_ok: Option<bool>,
) -> Result<Option<String>> {
    if kind != BlockKind::Unknown {
        return Ok(None);
    }
    if !classification.core_absence_attested {
        return load_persisted_orphan_class(client, hash).await;
    }
    // Core proved the candidate absent, but if the header carries the wrong nBits
    // for its only possible BTC height (`difficulty_epoch_ok = Some(false)`), it is
    // an invalid scratch parent: exclude it directly rather than letting the
    // offline timestamp/BIP34 classifier mislabel it as a strict or weak orphan.
    // The merged value (current result over the event rollup) preserves a
    // previously-proven `false` across a transient `--recheck-orphans` pass, so a
    // previously-excluded wrong-epoch parent stays excluded instead of flipping.
    if difficulty_epoch_ok == Some(false) {
        return Ok(BtcOrphanVerdict::Excluded.as_db_str().map(str::to_string));
    }
    let strict_height = load_strict_bip34_height(client, hash).await?;
    let (verdict, reason) = classify_btc_orphan(header.time as i64, header.bits, strict_height);
    if matches!(verdict, BtcOrphanVerdict::Pending) {
        debug!(
            hash = %hex::encode(hash),
            reason,
            "btc orphan classification pending: nBits table horizon reached; regenerate \
             scripts/gen-nbits-table.py and rerun reclassify-unknown-parents --recheck-orphans"
        );
    }
    Ok(verdict.as_db_str().map(str::to_string))
}

/// Read the persisted `block.btc_orphan_class` for a hash. The preserve-under-
/// transient-unknown fallback in `compute_block_orphan_class`: when a reconcile
/// pass carries no fresh Core-absence verdict, the stored class is reused so a
/// Core-off or transient run never wipes a real orphan class. `None` when the
/// row or column is NULL.
pub(crate) async fn load_persisted_orphan_class<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<String>> {
    let row = client
        .query_opt(
            "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await
        .context("load persisted btc_orphan_class")?;
    Ok(row.and_then(|row| row.get::<_, Option<String>>(0)))
}

/// Resolve the Bitcoin miner pool from the Core coinbase evidence already
/// persisted on `block` (script + outputs), via the shared
/// `resolve_bitcoin_miner_pool_id_from_coinbase` resolver. `None` when the row or
/// its stored coinbase script is absent. Used to re-attribute a block whose live
/// classification carries no fresh coinbase: by `demote_zero_active_block` to keep
/// a core-preserved block's miner, and by `resolve_effective_bitcoin_miner_pool_id`
/// for a canonical block missing a fresh Core coinbase.
pub(crate) async fn resolve_persisted_core_coinbase_bitcoin_miner_pool_id<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<i64>> {
    let row = client
        .query_opt(
            "SELECT btc_coinbase_script, btc_coinbase_outputs \
             FROM block \
             WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await
        .context("load persisted Core coinbase evidence")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let script: Option<Vec<u8>> = row.get(0);
    let Some(script) = script else {
        return Ok(None);
    };
    let outputs: Option<Vec<u8>> = row.get(1);
    resolve_bitcoin_miner_pool_id_from_coinbase(client, Some(&script), outputs.as_deref()).await
}

/// Best BIP34 coinbase height usable as STRICT orphan evidence for this parent:
/// any active non-near event from a strict-eligible chain (see
/// [`btc_orphan::STRICT_BIP34_CHAINS`]) whose stored BTC parent coinbase
/// scriptSig decodes to a height >= BIP34 activation. RSK (NULL coinbase),
/// Hathor (reconstructed coinbase), and Xaya are excluded by the chain join, so
/// they are weak-only. Returns `None` when no strict evidence is available.
///
/// Crate-internal: the api crate cannot (and must not) reach this
/// writer-crate helper; it carries its own DECLARED read-only copy in
/// `crates/mmm-api/src/projection/shared.rs`, built on the shared
/// mmm-capture parser and constants.
pub(crate) async fn load_strict_bip34_height<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<i32>> {
    let strict_chains: &[&str] = btc_orphan::STRICT_BIP34_CHAINS;
    let rows = client
        .query(
            "SELECT e.btc_parent_coinbase_script \
             FROM merge_mining_event e \
             JOIN source s ON s.id = e.source_id \
             WHERE e.btc_parent_header_hash = $1 \
               AND e.revoked_at IS NULL \
               AND e.btc_parent_kind <> 'near' \
               AND e.btc_parent_coinbase_script IS NOT NULL \
               AND s.chain = ANY($2) \
             ORDER BY e.id",
            &[&hash, &strict_chains],
        )
        .await
        .context("load strict BIP34 coinbase candidates")?;
    for row in rows {
        let script: Vec<u8> = row.get(0);
        if let Some(height) = parse_bip34_height(&script)
            && height >= btc_orphan::BIP34_HEIGHT
        {
            return Ok(Some(height));
        }
    }
    Ok(None)
}
