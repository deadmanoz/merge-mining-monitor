//! Parent classification: preflight, event-derived, and persisted forms.

use super::*;

/// Load the `ParentPreflight` for a candidate parent: the persisted state of
/// its BTC predecessor (`prev_hash`), if `block` already holds one. The
/// classifier in `mmm-bitcoin-core` uses `known_prev` to short-circuit a Core
/// round-trip when the predecessor is an established canonical/stale block.
/// Returns `known_prev: None` when the predecessor is unseen, leaving the
/// classifier to consult Core. Maps the row through `KnownBlockContextCompat`
/// so the DB columns stay decoupled from the classifier's input type.
pub async fn load_parent_preflight<C: GenericClient>(
    client: &C,
    prev_hash: &[u8],
) -> Result<ParentPreflight> {
    let row = client
        .query_opt(
            "SELECT kind, btc_height, btc_height_source, canonical_competitor_hash, \
                    core_attested, difficulty_epoch_ok \
             FROM block \
             WHERE btc_header_hash = $1",
            &[&prev_hash],
        )
        .await
        .context("load parent preflight")?;

    let known_prev = row
        .map(|row| {
            let kind: String = row.get(0);
            let height_source: Option<String> = row.get(2);
            let compat = KnownBlockContextCompat {
                kind: BlockKind::from_db_str(&kind)?,
                btc_height: row.get(1),
                btc_height_source: height_source
                    .as_deref()
                    .map(HeightSource::from_db_str)
                    .transpose()?,
                canonical_competitor_hash: row.get(3),
                core_attested: row.get(4),
            };
            Ok::<mmm_bitcoin_core::KnownBlockContext, anyhow::Error>(compat.into())
        })
        .transpose()?;

    Ok(ParentPreflight { known_prev })
}

/// Row-shaped staging struct for one `block` row, converted via `From` into
/// `mmm_bitcoin_core::KnownBlockContext`. Exists so
/// `load_parent_preflight` can build the classifier input field-by-field from
/// the SQL projection (with fallible `BlockKind`/`HeightSource` parsing) before
/// crossing the crate boundary, keeping the column decode out of the
/// mmm-bitcoin-core type.
pub(crate) struct KnownBlockContextCompat {
    kind: BlockKind,
    btc_height: Option<i32>,
    btc_height_source: Option<HeightSource>,
    canonical_competitor_hash: Option<Vec<u8>>,
    core_attested: bool,
}

impl From<KnownBlockContextCompat> for mmm_bitcoin_core::KnownBlockContext {
    fn from(value: KnownBlockContextCompat) -> Self {
        Self {
            kind: value.kind,
            btc_height: value.btc_height,
            btc_height_source: value.btc_height_source,
            canonical_competitor_hash: value.canonical_competitor_hash,
            core_attested: value.core_attested,
        }
    }
}

/// First mutation of a reconcile pass: roll the freshly resolved
/// `ParentClassification` into every active `merge_mining_event` row sharing
/// this parent header. Three guards in the WHERE clause are required for correctness:
/// `revoked_at IS NULL` (revoked evidence is inert), `btc_parent_kind <> 'near'`
/// (near parents are out of scope and never reclassified), and
/// `($2 <> 'unknown' OR btc_parent_kind = 'unknown')` so a transient `unknown`
/// never demotes a previously-proven canonical/stale row (demote bad evidence
/// only by explicit revoke). Bails if a canonical/stale result carries no
/// height. The `COALESCE` on `difficulty_epoch_ok` preserves a proven
/// wrong-epoch `false` across a transient re-resolve (see the inline note).
pub(crate) async fn apply_event_classification<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    classification: &ParentClassification,
) -> Result<()> {
    let kind = classification.kind.as_db_str();
    let height = match classification.kind {
        ParentKind::Canonical | ParentKind::Stale => classification.height,
        ParentKind::Near | ParentKind::Unknown => None,
    };
    if matches!(
        classification.kind,
        ParentKind::Canonical | ParentKind::Stale
    ) && height.is_none()
    {
        bail!(
            "canonical/stale classification for event {} has no height",
            event.id
        );
    }

    // COALESCE difficulty_epoch_ok so a transient NULL never erases a stored value.
    // Only the unknown path supplies NULL here (canonical/stale always resolve a
    // concrete value); difficulty_epoch_ok is a fixed property of the header and
    // its resolved height, so a later run that could not re-resolve it (e.g. a
    // --recheck-orphans pass whose inferred-stale competitor lookup was transiently
    // missing) must preserve a previously-proven wrong-epoch `false` rather than
    // clobber it. This keeps the event rollup, the block column, and the orphan
    // class consistent so the missing-only repair scanner does not churn.
    client
        .execute(
            "UPDATE merge_mining_event \
             SET btc_parent_kind = $2, \
                 btc_parent_height = $3, \
                 difficulty_epoch_ok = COALESCE($4, difficulty_epoch_ok) \
             WHERE btc_parent_header_hash = $1 \
               AND revoked_at IS NULL \
               AND btc_parent_kind <> 'near' \
               AND ($2 <> 'unknown' OR btc_parent_kind = 'unknown')",
            &[
                &event.btc_parent_header_hash,
                &kind,
                &height,
                &classification.difficulty_epoch_ok,
            ],
        )
        .await
        .context("update merge_mining_event classification")?;
    Ok(())
}

/// Rebuild every derived row keyed on one parent header from its current event
/// rollup: the `block` row and AuxPoW `attestation_proof` rows. `classification`
/// distinguishes the two entry forms: `Some` (a live reconcile) runs the result
/// through `effective_classification`; `None` (a dependent-cascade rebuild)
/// falls back to `persisted_classification_from_block`, else `unknown`. When no
/// active event remains for the hash the row is demoted via
/// `demote_zero_active_block`. Core adds one
/// to `distinct_sources` and forces `pow_validated` when `core_attested`. The
/// orphan class is computed before the upsert so `kind` and `btc_orphan_class`
/// land in one CHECK-safe statement.
pub(crate) async fn rebuild_parent_read_model<C: GenericClient>(
    client: &C,
    hash: &[u8],
    classification: Option<&ParentClassification>,
) -> Result<()> {
    let rollup = load_parent_rollup(client, hash).await?;

    let Some(rollup) = rollup else {
        demote_zero_active_block(client, hash).await?;
        rebuild_auxpow_proofs(client, hash).await?;
        return Ok(());
    };

    let event = load_representative_active_event(client, hash).await?;
    let header: Header = deserialize(&event.btc_parent_header_bytes)
        .context("deserialize representative parent header")?;
    let classification = match classification.cloned() {
        Some(classification) => {
            effective_classification(client, &event, &header, classification).await?
        }
        None => persisted_classification_from_block(client, &event, &header)
            .await?
            .unwrap_or_else(|| ParentClassification::unknown(&header)),
    };

    let (kind, height, height_source, competitor_hash) = match classification.kind {
        ParentKind::Canonical => (
            BlockKind::Canonical,
            classification.height,
            classification
                .height_source
                .or(Some(HeightSource::BitcoinCore)),
            None,
        ),
        ParentKind::Stale => (
            BlockKind::Stale,
            classification.height,
            classification.height_source,
            classification.canonical_competitor_hash.clone(),
        ),
        ParentKind::Unknown | ParentKind::Near => (BlockKind::Unknown, None, None, None),
    };

    let core_source_count = if classification.core_attested { 1 } else { 0 };
    // The effective wrong-epoch evidence: the current classifier result merged with
    // the event rollup, which COALESCEs a previously-proven `false` across a
    // transient `--recheck-orphans` pass (see apply_event_classification). Used for
    // both the block column and the orphan-class gate so they cannot disagree (and
    // the missing-only repair scanner sees no drift).
    let difficulty_epoch_ok = merge_difficulty(rollup.difficulty_epoch_ok, &classification);
    // Compute before building the block so the orphan class is written in the same
    // upsert statement as `kind` (CHECK-safe).
    let btc_orphan_class = compute_block_orphan_class(
        client,
        hash,
        kind,
        &classification,
        &header,
        difficulty_epoch_ok,
    )
    .await?;
    let coinbase = coinbase_columns(classification.coinbase.as_ref());
    let bitcoin_miner_pool_id =
        resolve_effective_bitcoin_miner_pool_id(client, hash, kind, &classification, &event)
            .await?;
    let block = BlockInput {
        hash: hash.to_vec(),
        prev_hash: event.btc_parent_prev_header_hash.clone(),
        height,
        height_source,
        kind,
        header_bytes: event.btc_parent_header_bytes.clone(),
        header_time: event.btc_parent_header_time,
        bitcoin_miner_pool_id,
        btc_coinbase_txid: coinbase.txid,
        btc_coinbase_script: coinbase.script,
        btc_coinbase_outputs: coinbase.outputs,
        btc_coinbase_status: coinbase.status,
        canonical_competitor_hash: competitor_hash.clone(),
        total_attestations: rollup.total_attestations,
        distinct_sources: rollup.distinct_sources + core_source_count,
        auxpow_chain_count: rollup.auxpow_chain_count,
        live_observed: classification.live_observed,
        core_attested: classification.core_attested,
        pow_validated: rollup.pow_validated || classification.core_attested,
        difficulty_epoch_ok,
        first_attested_at: rollup.first_attested_at,
        last_attested_at: rollup.last_attested_at,
        btc_orphan_class,
    };
    upsert_block_row(client, &block, BlockWriteMode::EventRollup).await?;
    rebuild_auxpow_proofs(client, hash).await?;

    Ok(())
}

/// Resolve `block.bitcoin_miner_pool_id` with a precedence ladder: (1) Core's
/// own coinbase for a freshly Core-attested canonical/stale parent, (2) the
/// pool already persisted from a prior Core attestation when the block is
/// canonical (so a Core-off reconcile does not lose it), (3) the AuxPoW event's
/// reported parent coinbase. Core evidence wins over event-reported coinbase
/// because the event coinbase can be spoofed by the child-chain submitter.
async fn resolve_effective_bitcoin_miner_pool_id<C: GenericClient>(
    client: &C,
    hash: &[u8],
    kind: BlockKind,
    classification: &ParentClassification,
    event: &MergeMiningEvent,
) -> Result<Option<i64>> {
    if classification.core_attested
        && matches!(
            classification.kind,
            ParentKind::Canonical | ParentKind::Stale
        )
    {
        let core_pool_id =
            resolve_core_coinbase_pool_id(client, classification.coinbase.as_ref()).await?;
        if core_pool_id.is_some() {
            return Ok(core_pool_id);
        }
    }

    if kind == BlockKind::Canonical
        && let Some(core_pool_id) =
            resolve_persisted_core_coinbase_bitcoin_miner_pool_id(client, hash).await?
    {
        return Ok(Some(core_pool_id));
    }

    resolve_bitcoin_miner_pool_id_from_coinbase(
        client,
        event.btc_parent_coinbase_script.as_deref(),
        event.btc_parent_coinbase_outputs.as_deref(),
    )
    .await
}

/// Resolve the classification actually written, preserving proven state under a
/// transient `unknown`. A bare `unknown` (e.g. Core off or an RPC failure) must
/// not erase a real verdict, so the fallback order is: persisted
/// canonical/stale from `block`, then the event's own canonical claim
/// (`classification_from_event`), then for an event already marked stale an
/// `unknown` that still carries the incoming `difficulty_epoch_ok`. Any
/// non-unknown result passes through unchanged. Operators clear bad evidence by
/// explicit revoke, not by letting a transient unknown demote it.
pub(crate) async fn effective_classification<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    header: &Header,
    classification: ParentClassification,
) -> Result<ParentClassification> {
    if classification.kind == ParentKind::Unknown {
        // Unknown can mean a transient RPC failure. Preserve the prior proven
        // state; operators can explicitly revoke bad evidence and rerun repair.
        if let Some(classification) =
            persisted_classification_from_block(client, event, header).await?
        {
            return Ok(classification);
        }
        if let Some(classification) = classification_from_event(event, header) {
            return Ok(classification);
        }
        if event.btc_parent_kind == ParentKind::Stale {
            let mut unknown = ParentClassification::unknown(header);
            unknown.difficulty_epoch_ok = classification.difficulty_epoch_ok;
            return Ok(unknown);
        }
    }
    Ok(classification)
}

/// Event-derived classification: reconstruct a `ParentClassification` from the
/// `merge_mining_event` row's own columns, with no Core or `block` lookup. Only
/// the canonical case is reconstructable (height plus an implied
/// `HeightSource::BitcoinCore`); returns `None` for any non-canonical event
/// because stale/unknown need competitor and absence evidence the event row
/// does not carry. The result is Core-free (`core_attested`,
/// `core_absence_attested`, `live_observed` all false). Used by
/// `effective_classification` as the middle rung of its preserve-on-unknown
/// ladder.
pub(crate) fn classification_from_event(
    event: &MergeMiningEvent,
    header: &Header,
) -> Option<ParentClassification> {
    if event.btc_parent_kind != ParentKind::Canonical {
        return None;
    }
    Some(ParentClassification {
        kind: event.btc_parent_kind,
        height: event.btc_parent_height,
        height_source: event.btc_parent_height.map(|_| HeightSource::BitcoinCore),
        prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
        canonical_predecessor_header: None,
        canonical_competitor_header: None,
        canonical_competitor_hash: None,
        coinbase: None,
        difficulty_epoch_ok: event.difficulty_epoch_ok,
        live_observed: false,
        core_attested: false,
        core_absence_attested: false,
    })
}

/// Persisted-form classification: reconstruct a `ParentClassification` from the
/// already-derived `block` row(s), used to hold proven state across a transient
/// `unknown`. Only canonical/stale persisted blocks qualify (unknown returns
/// `None`). For a stale block the competitor must still resolve to a persisted
/// canonical block, else `None` so a half-rebuilt stale relationship is not
/// re-asserted. `core_attested`/`live_observed` are carried from the persisted
/// row but `core_absence_attested` is false: this is replay of stored evidence,
/// not a fresh Core absence consultation, so it must not retrigger orphan
/// reclassification.
pub(crate) async fn persisted_classification_from_block<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    header: &Header,
) -> Result<Option<ParentClassification>> {
    let persisted = load_block_cascade_state(client, &event.btc_parent_header_hash).await?;
    let Some(persisted) = persisted else {
        return Ok(None);
    };
    if !matches!(persisted.kind, BlockKind::Canonical | BlockKind::Stale) {
        return Ok(None);
    }
    let kind = match persisted.kind {
        BlockKind::Canonical => ParentKind::Canonical,
        BlockKind::Stale => ParentKind::Stale,
        BlockKind::Unknown => unreachable!("unknown state filtered above"),
    };
    let canonical_competitor_hash = match kind {
        ParentKind::Stale => {
            let Some(competitor_hash) = persisted.canonical_competitor_hash.clone() else {
                return Ok(None);
            };
            let competitor = load_block_cascade_state(client, &competitor_hash).await?;
            if !competitor.is_some_and(|state| state.kind == BlockKind::Canonical) {
                return Ok(None);
            }
            Some(competitor_hash)
        }
        ParentKind::Canonical => None,
        ParentKind::Near | ParentKind::Unknown => unreachable!("filtered above"),
    };

    Ok(Some(ParentClassification {
        kind,
        height: persisted.btc_height,
        height_source: persisted.btc_height_source,
        prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
        canonical_predecessor_header: None,
        canonical_competitor_header: None,
        canonical_competitor_hash,
        coinbase: None,
        difficulty_epoch_ok: persisted.difficulty_epoch_ok,
        live_observed: persisted.core_attested,
        core_attested: persisted.core_attested,
        core_absence_attested: false,
    }))
}
