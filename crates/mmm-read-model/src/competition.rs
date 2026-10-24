//! Zero-active demotion and orphan-class derivation.

use super::*;

/// Retire or demote a parent `block` row when no active events remain.
/// Core-attested blocks and blocks referenced via `canonical_competitor_hash` are
/// demoted in place; all others and their proofs are deleted.
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

/// Demote a zero-active `block` row in place: Core-attested rows retain their
/// backbone fields; non-Core referenced rows collapse to a zero-attestation
/// `unknown` husk.
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
                 invalid_block_reason = CASE WHEN core_attested THEN invalid_block_reason ELSE NULL END, \
                 btc_orphan_class = NULL, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE btc_header_hash = $1",
            &[&hash, &core_pool_id],
        )
        .await
        .context("demote zero-active block")?;
    Ok(())
}

/// Derive `block.btc_orphan_class` for `hash` (`None` unless `kind == Unknown`;
/// preserves the persisted class when `core_absence_attested` is false).
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

/// First valid strict BIP34 coinbase height from an active event on
/// [`btc_orphan::STRICT_BIP34_CHAINS`]. Mirrored by the declared read-only copy
/// in `crates/mmm-api/src/projection/shared/mod.rs`.
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
