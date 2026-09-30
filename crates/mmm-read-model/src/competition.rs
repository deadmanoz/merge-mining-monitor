//! Zero-active demotion and orphan-class derivation.

use super::*;

/// Retire the derived rows of a parent with no active non-near evidence left
/// (its events were deleted or revoked). A block exists only while evidence
/// attests it, so a non-Core block and its proofs are deleted. A Core-attested
/// block stays as the backbone row Core vouches for, and a block another row
/// still names as its `canonical_competitor_hash` cannot be deleted (the
/// foreign key has no `ON DELETE`); both are demoted in place and keep their
/// proof history, and the dependent cascade re-derives any referencing row.
pub(crate) async fn retire_zero_active_block<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<()> {
    let row = client
        .query_opt(
            "SELECT core_attested, \
                    EXISTS (SELECT 1 FROM block r WHERE r.canonical_competitor_hash = $1) \
             FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await
        .context("load zero-active block")?;
    let (core_attested, referenced) = match row {
        Some(row) => (row.get::<_, bool>(0), row.get::<_, bool>(1)),
        None => (false, false),
    };
    if core_attested || referenced {
        demote_zero_active_block(client, hash).await?;
        return rebuild_auxpow_proofs(client, hash).await;
    }
    client
        .execute(
            "DELETE FROM attestation_proof WHERE btc_header_hash = $1 AND proof_kind = 'auxpow'",
            &[&hash],
        )
        .await
        .context("delete proofs of a zero-active block")?;
    client
        .execute("DELETE FROM block WHERE btc_header_hash = $1", &[&hash])
        .await
        .context("delete zero-active block")?;
    Ok(())
}

/// Rewrite a zero-active `block` row in place: a Core-attested block keeps the
/// backbone fields Core vouches for (kind, height, competitor, one distinct
/// source, `pow_validated=true`, persisted Core-coinbase miner); a non-Core
/// block that must survive because a row still references it collapses to an
/// `unknown` husk (counters zeroed, `pow_validated=false`, orphan class and
/// error reason cleared, miner NULL), which the API orphan index filters out.
async fn demote_zero_active_block<C: GenericClient>(client: &C, hash: &[u8]) -> Result<()> {
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
                 error_block_reason = CASE WHEN core_attested THEN error_block_reason ELSE NULL END, \
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
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
) -> Result<Option<String>> {
    if kind != BlockKind::Unknown {
        return Ok(None);
    }
    if has_published_stale_branch_attestation(client, hash).await? {
        return Ok(BtcOrphanVerdict::Excluded.as_db_str().map(str::to_string));
    }
    if !classification.core_absence_attested {
        return load_persisted_orphan_class(client, hash).await;
    }
    // Check operator-imported membership before strict/weak refinement. The
    // membership boundary and self-reference rationale live in mmm-store.
    if mmm_store::is_known_stale_hash(client, hash).await? {
        debug!(
            hash = %hex::encode(hash),
            "known-stale membership: excluding parent from strict/weak orphan classification"
        );
        return Ok(BtcOrphanVerdict::Excluded.as_db_str().map(str::to_string));
    }
    let strict_height = load_strict_bip34_height(client, hash).await?;
    let loaded_nbits_table = match nbits_table {
        Some(_) => None,
        None => Some(mmm_store::load_bitcoin_core_nbits_table(client).await?),
    };
    let nbits_table = nbits_table
        .or(loaded_nbits_table.as_ref())
        .expect("Core nBits table is loaded when no shared table was supplied");
    let (verdict, reason) = mmm_capture::btc_orphan::classify_btc_orphan_with(
        nbits_table,
        header.time as i64,
        header.bits,
        strict_height,
    );
    if matches!(verdict, BtcOrphanVerdict::Pending) {
        debug!(
            hash = %hex::encode(hash),
            reason,
            "btc orphan classification pending: the Core header cache has not reached this evidence"
        );
    }
    Ok(verdict.as_db_str().map(str::to_string))
}

async fn has_published_stale_branch_attestation<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<bool> {
    Ok(client
        .query_opt(
            "SELECT 1 \
             FROM merge_mining_event e \
             JOIN historical_event_provenance p ON p.event_id = e.id \
             WHERE e.btc_parent_header_hash = $1 \
               AND e.revoked_at IS NULL \
               AND p.relevance_reason IN ('valid_direct_stale', 'valid_stale_descendant') \
             LIMIT 1",
            &[&hash],
        )
        .await
        .context("check published stale-branch attestation")?
        .is_some())
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
/// for a canonical or Core-attested stale block missing a fresh Core coinbase.
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
/// evidence passes the shared strict-height validator. Hathor additionally
/// requires a complete coinbase transaction whose sole input script matches
/// the stored script. RSK (NULL coinbase) and Xaya are excluded by the chain
/// join, so they are weak-only. Returns `None` when no strict evidence is
/// available.
///
/// Crate-internal: the api crate cannot (and must not) reach this
/// writer-crate helper; it carries its own DECLARED read-only copy in
/// `crates/mmm-api/src/projection/shared/mod.rs`, built on the shared
/// mmm-capture parser and constants.
pub(crate) async fn load_strict_bip34_height<C: GenericClient>(
    client: &C,
    hash: &[u8],
) -> Result<Option<i32>> {
    let strict_chains: &[&str] = btc_orphan::STRICT_BIP34_CHAINS;
    let rows = client
        .query(
            "SELECT s.chain, e.btc_parent_coinbase_script, e.btc_parent_coinbase_tx_bytes \
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
        let chain: String = row.get(0);
        let script: Vec<u8> = row.get(1);
        let tx_bytes: Option<Vec<u8>> = row.get(2);
        if let Some(height) =
            btc_orphan::strict_bip34_height_from_evidence(&chain, &script, tx_bytes.as_deref())
        {
            return Ok(Some(height));
        }
    }
    Ok(None)
}
