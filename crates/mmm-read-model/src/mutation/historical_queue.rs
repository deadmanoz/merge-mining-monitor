//! Durable, bounded read-model work for historical publication imports.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, GenericClient};

use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::capture::ParentKind;
use mmm_capture::nbits_table::NbitsTable;

use super::{PrimaryDiff, cascade_changed_with_nbits_table, lock_core_classification_view_shared};
use crate::{
    DEFAULT_CASCADE_BUDGET, PreclassifiedParent, RECONCILE_LOCK_SET_RETRY_LIMIT,
    find_anchor_event_for_block, is_reconcile_lock_set_changed, load_block_cascade_state,
    load_event, lock_block_hash, lock_event_for_source_health, preclassify_event_parent,
    rebuild_parent_read_model, reconcile_one_event_in_txn,
};

/// Drain durable historical parent work to completion.
///
/// Each primary parent rebuild commits independently. Its exact changed-hash
/// seeds are stored in the queue in the same transaction before the dependent
/// cascade runs. The queue row is deleted only after that cascade succeeds, so
/// rerunning an interrupted import resumes either the primary rebuild or the
/// already-recorded cascade without relying on an idempotent rebuild to emit the
/// same change a second time. `classifications` supplies verdicts authenticated
/// during this import; each cached verdict is reused only while the
/// barrier-protected local Core view still supports it. Queue entries left by
/// an interrupted earlier run fall back to the configured classifier.
pub async fn drain_historical_reconcile_queue(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    classifications: &HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    drain_historical_reconcile_queue_with_nbits_table(client, classifier, classifications, None)
        .await
}

/// Drain historical work against one supplied Core-cache snapshot.
pub async fn drain_historical_reconcile_queue_with_nbits_table(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    classifications: &HashMap<Vec<u8>, ParentClassification>,
    nbits_table: Option<&NbitsTable>,
) -> Result<()> {
    drain_historical_reconcile_queue_with_budget(
        client,
        classifier,
        classifications,
        DEFAULT_CASCADE_BUDGET,
        nbits_table,
    )
    .await
}

#[cfg(feature = "db-integration")]
#[doc(hidden)]
pub async fn drain_historical_reconcile_queue_with_budget_for_test(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    classifications: &HashMap<Vec<u8>, ParentClassification>,
    cascade_budget: usize,
) -> Result<()> {
    drain_historical_reconcile_queue_with_budget(
        client,
        classifier,
        classifications,
        cascade_budget,
        None,
    )
    .await
}

async fn drain_historical_reconcile_queue_with_budget(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    classifications: &HashMap<Vec<u8>, ParentClassification>,
    cascade_budget: usize,
    nbits_table: Option<&NbitsTable>,
) -> Result<()> {
    loop {
        let Some(row) = client
            .query_opt(
                "SELECT btc_parent_header_hash, primary_pending, changed_hashes, generation \
                 FROM historical_reconcile_queue \
                 ORDER BY enqueued_at, btc_parent_header_hash \
                 LIMIT 1",
                &[],
            )
            .await
            .context("load historical reconcile work")?
        else {
            return Ok(());
        };
        let parent_hash: Vec<u8> = row.get(0);
        let primary_pending: bool = row.get(1);
        let changed_hashes: Vec<Vec<u8>> = row.get(2);
        let generation: i64 = row.get(3);

        if primary_pending {
            reconcile_historical_primary(
                client,
                classifier,
                &parent_hash,
                generation,
                classifications.get(&parent_hash).cloned(),
                nbits_table,
            )
            .await?;
            continue;
        }

        cascade_changed_with_nbits_table(
            client,
            classifier,
            changed_hashes.clone(),
            cascade_budget,
            nbits_table,
        )
        .await
        .with_context(|| {
            format!(
                "cascade durable historical parent {}",
                hex::encode(&parent_hash)
            )
        })?;
        client
            .execute(
                "DELETE FROM historical_reconcile_queue \
                 WHERE btc_parent_header_hash = $1 \
                   AND primary_pending = FALSE \
                   AND generation = $2 \
                   AND changed_hashes = $3",
                &[&parent_hash, &generation, &changed_hashes],
            )
            .await
            .context("complete historical reconcile work")?;
    }
}

pub(super) async fn enqueue_historical_parent<C: GenericClient>(
    client: &C,
    parent_hash: &[u8],
) -> Result<()> {
    client
        .execute(
            "INSERT INTO historical_reconcile_queue (btc_parent_header_hash) \
             VALUES ($1) \
             ON CONFLICT (btc_parent_header_hash) DO UPDATE SET \
                primary_pending = TRUE, \
                generation = historical_reconcile_queue.generation + 1, \
                updated_at = now()",
            &[&parent_hash],
        )
        .await
        .context("enqueue historical parent reconcile")?;
    Ok(())
}

async fn reconcile_historical_primary(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    parent_hash: &[u8],
    generation: i64,
    classification: Option<ParentClassification>,
    nbits_table: Option<&NbitsTable>,
) -> Result<()> {
    match find_anchor_event_for_block(client, parent_hash).await? {
        Some(event_id) => {
            reconcile_historical_anchored_primary(
                client,
                classifier,
                parent_hash,
                event_id,
                generation,
                classification,
                nbits_table,
            )
            .await
        }
        None => {
            reconcile_historical_orphan_primary(client, parent_hash, generation, nbits_table).await
        }
    }
}

async fn reconcile_historical_anchored_primary(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    parent_hash: &[u8],
    event_id: i64,
    generation: i64,
    classification: Option<ParentClassification>,
    nbits_table: Option<&NbitsTable>,
) -> Result<()> {
    let trusted = classification.map(PreclassifiedParent::trusted);
    for attempt in 0..RECONCILE_LOCK_SET_RETRY_LIMIT {
        let txn = client
            .transaction()
            .await
            .context("begin historical parent reconcile")?;
        lock_core_classification_view_shared(&txn, nbits_table).await?;
        let (preclassified, cached_rejected) = resolve_historical_preclassification(
            &txn,
            classifier,
            parent_hash,
            event_id,
            trusted.as_ref(),
        )
        .await?;
        // Acquire the full sorted block-lock set before the event rows, matching
        // capture/reconcile ordering. The queue row remains last so a base
        // importer that already owns the event can enqueue and commit first.
        let event = load_event(&txn, event_id).await?;
        lock_event_for_source_health(&txn, &event, classifier, preclassified.clone()).await?;
        if !lock_historical_parent_events(&txn, parent_hash, event_id).await? {
            txn.rollback()
                .await
                .context("rollback missing historical reconcile anchor")?;
            return Ok(());
        }
        if !lock_current_historical_primary(&txn, parent_hash, generation).await? {
            txn.rollback()
                .await
                .context("rollback superseded historical parent reconcile")?;
            return Ok(());
        }
        if cached_rejected
            && preclassified
                .as_ref()
                .is_some_and(|parent| parent.classification.kind == ParentKind::Unknown)
        {
            txn.rollback()
                .await
                .context("rollback unresolved invalidated historical classification")?;
            bail!(
                "cached historical classification no longer matches the Core view and fresh classification is unknown for parent {}",
                hex::encode(parent_hash)
            );
        }
        match reconcile_one_event_in_txn(
            &txn,
            event_id,
            classifier,
            preclassified,
            PrimaryDiff::BulkImport,
            nbits_table,
        )
        .await
        {
            Ok(changed_hashes) => {
                persist_historical_primary(&txn, parent_hash, generation, &changed_hashes).await?;
                txn.commit()
                    .await
                    .context("commit historical parent reconcile")?;
                return Ok(());
            }
            Err(err)
                if is_reconcile_lock_set_changed(&err)
                    && attempt + 1 < RECONCILE_LOCK_SET_RETRY_LIMIT =>
            {
                txn.rollback()
                    .await
                    .context("rollback historical reconcile after lock-set change")?;
            }
            Err(err) => {
                let _ = txn.rollback().await;
                return Err(err);
            }
        }
    }
    unreachable!("historical reconcile retry loop always returns")
}

async fn resolve_historical_preclassification<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    parent_hash: &[u8],
    event_id: i64,
    trusted: Option<&PreclassifiedParent>,
) -> Result<(Option<PreclassifiedParent>, bool)> {
    match trusted {
        Some(preclassified) if !classifier.is_enabled() => Ok((Some(preclassified.clone()), false)),
        Some(preclassified)
            if cached_classification_matches_core_view(
                client,
                parent_hash,
                &preclassified.classification,
            )
            .await? =>
        {
            Ok((Some(preclassified.clone()), false))
        }
        Some(_) => Ok((
            preclassify_event_parent(client, event_id, classifier).await?,
            true,
        )),
        None => Ok((
            preclassify_event_parent(client, event_id, classifier).await?,
            false,
        )),
    }
}

async fn reconcile_historical_orphan_primary(
    client: &mut Client,
    parent_hash: &[u8],
    generation: i64,
    nbits_table: Option<&NbitsTable>,
) -> Result<()> {
    let txn = client
        .transaction()
        .await
        .context("begin historical orphaned-block reconcile")?;
    lock_core_classification_view_shared(&txn, nbits_table).await?;
    // Keep the orphan path on the same global order as anchored reconcile:
    // block advisory locks before the durable queue row. An importer writes its
    // event before enqueueing this parent, so taking the queue row first could
    // close a queue -> block -> event -> queue deadlock cycle when anchor
    // discovery raced that uncommitted event.
    lock_block_hash(&txn, parent_hash).await?;
    if !lock_current_historical_primary(&txn, parent_hash, generation).await? {
        txn.rollback()
            .await
            .context("rollback superseded historical orphaned-block reconcile")?;
        return Ok(());
    }
    let before = load_block_cascade_state(&txn, parent_hash).await?;
    rebuild_parent_read_model(&txn, parent_hash, None, nbits_table).await?;
    let after = load_block_cascade_state(&txn, parent_hash).await?;
    let changed_hashes = if before != after {
        vec![parent_hash.to_vec()]
    } else {
        Vec::new()
    };
    persist_historical_primary(&txn, parent_hash, generation, &changed_hashes).await?;
    txn.commit()
        .await
        .context("commit historical orphaned-block reconcile")
}

/// Reuse an import preflight classification when the barrier-protected local
/// Core view still supports it. This preserves the cross-chain classification
/// cache without allowing a suffix switch between preflight and queue drain to
/// write the displaced verdict back after repair.
async fn cached_classification_matches_core_view<C: GenericClient>(
    client: &C,
    parent_hash: &[u8],
    classification: &ParentClassification,
) -> Result<bool> {
    if classification.kind == ParentKind::ErrorBlock {
        return Ok(true);
    }
    if let Some(height) = classification.height {
        let canonical = client
            .query(
                "SELECT btc_header_hash FROM block \
                 WHERE kind = 'canonical' AND btc_height = $1 \
                 ORDER BY btc_header_hash",
                &[&height],
            )
            .await
            .context("validate cached classification against canonical height")?;
        if canonical.is_empty() {
            return Ok(true);
        }
        let [row] = canonical.as_slice() else {
            return Ok(false);
        };
        let canonical_hash: Vec<u8> = row.get(0);
        return Ok(match classification.kind {
            ParentKind::Canonical => canonical_hash == parent_hash,
            ParentKind::Stale => {
                classification.canonical_competitor_hash.as_deref()
                    == Some(canonical_hash.as_slice())
            }
            ParentKind::ErrorBlock => true,
            ParentKind::Near | ParentKind::Unknown => false,
        });
    }
    let core_context_exists: bool = client
        .query_one(
            "SELECT EXISTS ( \
                 SELECT 1 FROM block \
                 WHERE core_attested \
                   AND btc_header_hash IN ($1, $2) \
             )",
            &[&parent_hash, &classification.prev_hash],
        )
        .await
        .context("validate cached unheighted classification against Core context")?
        .get(0);
    Ok(!core_context_exists)
}

async fn lock_historical_parent_events<C: GenericClient>(
    client: &C,
    parent_hash: &[u8],
    anchor_event_id: i64,
) -> Result<bool> {
    let rows = client
        .query(
            "SELECT id FROM merge_mining_event \
             WHERE btc_parent_header_hash = $1 \
               AND btc_parent_kind <> 'near' \
               AND revoked_at IS NULL \
             ORDER BY id \
             FOR UPDATE",
            &[&parent_hash],
        )
        .await
        .context("lock historical reconcile parent events")?;
    Ok(rows
        .iter()
        .any(|row| row.get::<_, i64>(0) == anchor_event_id))
}

async fn lock_current_historical_primary<C: GenericClient>(
    client: &C,
    parent_hash: &[u8],
    generation: i64,
) -> Result<bool> {
    Ok(client
        .query_opt(
            "SELECT 1 FROM historical_reconcile_queue \
             WHERE btc_parent_header_hash = $1 \
               AND primary_pending = TRUE \
               AND generation = $2 \
             FOR UPDATE",
            &[&parent_hash, &generation],
        )
        .await
        .context("lock historical primary reconcile work")?
        .is_some())
}

async fn persist_historical_primary<C: GenericClient>(
    client: &C,
    parent_hash: &[u8],
    generation: i64,
    changed_hashes: &[Vec<u8>],
) -> Result<()> {
    let affected = client
        .execute(
            "UPDATE historical_reconcile_queue q SET \
                primary_pending = FALSE, \
                changed_hashes = ARRAY( \
                    SELECT DISTINCT value \
                    FROM unnest(q.changed_hashes || $3::bytea[]) AS hashes(value) \
                    ORDER BY value \
                ), \
                generation = q.generation + 1, \
                updated_at = now() \
             WHERE q.btc_parent_header_hash = $1 \
               AND q.primary_pending = TRUE \
               AND q.generation = $2",
            &[&parent_hash, &generation, &changed_hashes],
        )
        .await
        .context("persist historical primary reconcile result")?;
    if affected != 1 {
        bail!(
            "historical reconcile queue generation changed while locked for parent {}",
            hex::encode(parent_hash)
        );
    }
    Ok(())
}
