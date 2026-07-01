//! Event/block-grain reconciliation and the dependent-cascade queue.

use super::*;

/// `Some(true)` if `merge_mining_event` `id` is revoked, `Some(false)` if live,
/// `None` if the row is absent. Read by revoke/restore before re-running the
/// reconcile so a no-op transition can short-circuit.
pub(crate) async fn event_revoked_state<C: GenericClient>(
    client: &C,
    event_id: i64,
) -> Result<Option<bool>> {
    let row = client
        .query_opt(
            "SELECT revoked_at IS NOT NULL FROM merge_mining_event WHERE id = $1",
            &[&event_id],
        )
        .await
        .with_context(|| format!("load revocation state for event {event_id}"))?;
    Ok(row.map(|row| row.get(0)))
}

/// One unit of cascade work. `Event` carries an optional pre-computed
/// `ParentClassification` (boxed to keep the enum small) so the seeding caller
/// can hand reconcile a trusted classification and skip a re-classify;
/// `enqueue_dependents` always pushes `Event(_, None)` because a cascaded child
/// must be re-classified from current state. `Block` is a derived-table-grain
/// item (orphan block or stale-competitor dependent) with no anchor event.
#[derive(Debug)]
pub(crate) enum ReconcileWork {
    Event(i64, Option<Box<ParentClassification>>),
    Block(Vec<u8>),
}

/// Drain the cascade queue to a fixed point, reconciling each parent once.
///
/// FIFO work-list, not recursion: each dequeued item is reconciled, then its
/// dependents (`enqueue_dependents`) are appended, so the cascade widens
/// breadth-first until the queue empties. `visited_events` / `visited_blocks`
/// dedup so a hash reached by several parents is reconciled once; events dedup
/// on dequeue (the seed may carry a preclassification a later enqueue cannot),
/// blocks dedup on the cheaper enqueue check. `budget` bounds
/// `parents_reconciled` (the count of items actually reconciled, not enqueued):
/// hitting it raises `ReconcileCascadeBudgetExhausted` rather than looping
/// unbounded on a pathological fan-out. Each `reconcile_one_*` runs in its own
/// transaction, so the cascade is a sequence of committed steps, not one
/// mega-transaction.
pub(crate) async fn drain_reconcile_queue(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    mut queue: VecDeque<ReconcileWork>,
    budget: Option<usize>,
) -> Result<ReconcileStats> {
    let mut stats = ReconcileStats {
        parents_reconciled: 0,
        descendants_reconciled: 0,
    };
    let mut visited_events = HashSet::new();
    let mut visited_blocks = HashSet::new();

    while let Some(work) = queue.pop_front() {
        if budget.is_some_and(|budget| stats.parents_reconciled >= budget) {
            return Err(ReconcileCascadeBudgetExhausted {
                budget: budget.unwrap_or(DEFAULT_CASCADE_BUDGET),
            }
            .into());
        }
        let changed_hashes = match work {
            ReconcileWork::Event(event_id, preclassified) => {
                if !visited_events.insert(event_id) {
                    continue;
                }
                let preclassified = preclassified.map(|classification| *classification);
                let changed_hashes =
                    reconcile_one_event(client, event_id, classifier, preclassified).await?;
                stats.parents_reconciled += 1;
                changed_hashes
            }
            ReconcileWork::Block(hash) => {
                if !visited_blocks.insert(hash.clone()) {
                    continue;
                }
                let changed_hashes = reconcile_one_block(client, &hash, classifier).await?;
                stats.parents_reconciled += 1;
                changed_hashes
            }
        };

        for hash in changed_hashes {
            enqueue_dependents(
                client,
                &hash,
                &mut queue,
                &visited_events,
                &visited_blocks,
                &mut stats,
            )
            .await?;
        }
    }

    Ok(stats)
}

pub(crate) async fn reconcile_dependents_after_changes(
    client: &mut Client,
    hashes: &[Vec<u8>],
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    reconcile_dependents_after_changes_with_budget(
        client,
        hashes,
        classifier,
        DEFAULT_CASCADE_BUDGET,
    )
    .await
}

/// `reconcile_dependents_after_changes` with an explicit cascade budget. The
/// mutation module uses this to apply the standard per-change cascade bound.
pub(crate) async fn reconcile_dependents_after_changes_with_budget(
    client: &mut Client,
    hashes: &[Vec<u8>],
    classifier: &ConfiguredParentClassifier,
    cascade_budget: usize,
) -> Result<()> {
    let mut queue = VecDeque::new();
    let visited_events = HashSet::new();
    let visited_blocks = HashSet::new();
    let mut stats = ReconcileStats {
        parents_reconciled: 0,
        descendants_reconciled: 0,
    };
    for hash in hashes {
        enqueue_dependents(
            client,
            hash,
            &mut queue,
            &visited_events,
            &visited_blocks,
            &mut stats,
        )
        .await?;
    }
    if !queue.is_empty() {
        drain_reconcile_queue(client, classifier, queue, Some(cascade_budget)).await?;
    }
    Ok(())
}

/// Re-reconcile every derived row that depends on a single changed parent
/// hash, after the backbone sync committed that parent. Entry point for
/// `mmm-producers` core-backbone sync; seeds the cascade from this hash's
/// dependents (not the hash itself) at the default budget. The plural-hash
/// and explicit-budget forms live behind `reconcile_dependents_after_changes_with_budget`.
pub async fn reconcile_dependents_after_change(
    client: &mut Client,
    hash: &[u8],
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    reconcile_dependents_after_changes(client, &[hash.to_vec()], classifier).await
}

/// Append every derived row that depends on `hash` to the cascade queue.
///
/// Three dependent classes, in this fixed order: child events whose
/// `btc_parent_prev_header_hash = hash` (the next merge-mining event chained
/// off this parent), derived child blocks whose `btc_prev_header_hash = hash`,
/// and stale competitor dependents (stale rows whose canonical competitor is
/// `hash`). The
/// `visited_*` sets are consulted (not mutated) here so a dependent already
/// queued or reconciled is not re-enqueued; `stats.descendants_reconciled`
/// counts each newly-queued item. Child events enqueue as `Event(_, None)`:
/// a cascaded child is re-classified from current state, never from a stale
/// preclassification.
pub(crate) async fn enqueue_dependents(
    client: &Client,
    hash: &[u8],
    queue: &mut VecDeque<ReconcileWork>,
    visited_events: &HashSet<i64>,
    visited_blocks: &HashSet<Vec<u8>>,
    stats: &mut ReconcileStats,
) -> Result<()> {
    for id in find_child_events(client, hash).await? {
        if !visited_events.contains(&id) {
            queue.push_back(ReconcileWork::Event(id, None));
            stats.descendants_reconciled += 1;
        }
    }
    for child_hash in find_derived_child_blocks(client, hash).await? {
        if !visited_blocks.contains(&child_hash) {
            queue.push_back(ReconcileWork::Block(child_hash));
            stats.descendants_reconciled += 1;
        }
    }
    for stale_hash in find_stale_competitor_dependents(client, hash).await? {
        if !visited_blocks.contains(&stale_hash) {
            queue.push_back(ReconcileWork::Block(stale_hash));
            stats.descendants_reconciled += 1;
        }
    }
    Ok(())
}

/// Reconcile one event to a fixed point in its own transaction, retrying on a
/// changed advisory-lock set.
///
/// The lock set an event needs is derived from its classification (predecessor
/// / competitor hashes), which can shift if the event drifts under us between
/// the pre-lock classify and the post-lock re-classify. When that happens
/// `reconcile_one_event_in_txn` returns `ReconcileLockSetChanged`; this wrapper
/// rolls back and retries up to `RECONCILE_LOCK_SET_RETRY_LIMIT` times,
/// recomputing the preclassification each attempt unless the caller supplied a
/// trusted one. This is the no-wrapper path, so it passes `PrimaryDiff::Reconcile`:
/// it owns the primary parent's source_health diff (its `before` snapshot is
/// genuinely pre-mutation). Used by the cascade drain and by block reconcile
/// when the block has an anchor event.
pub(crate) async fn reconcile_one_event(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<ParentClassification>,
) -> Result<Vec<Vec<u8>>> {
    let trusted_preclassified = preclassified.map(PreclassifiedParent::trusted);
    for attempt in 0..RECONCILE_LOCK_SET_RETRY_LIMIT {
        let attempt_preclassified = match &trusted_preclassified {
            Some(preclassified) => Some(preclassified.clone()),
            None => preclassify_event_parent(client, event_id, classifier).await?,
        };
        let txn = client
            .transaction()
            .await
            .context("begin reconcile transaction")?;
        match reconcile_one_event_in_txn(
            &txn,
            event_id,
            classifier,
            attempt_preclassified.clone(),
            // No wrapper: this path owns the primary source_health diff.
            PrimaryDiff::Reconcile,
        )
        .await
        {
            Ok(changed_hashes) => {
                txn.commit().await.context("commit reconcile transaction")?;
                return Ok(changed_hashes);
            }
            Err(err)
                if is_reconcile_lock_set_changed(&err)
                    && attempt + 1 < RECONCILE_LOCK_SET_RETRY_LIMIT =>
            {
                txn.rollback()
                    .await
                    .context("rollback reconcile transaction after lock-set change")?;
                debug!(
                    event_id,
                    attempt, "retrying reconcile after lock-set change"
                );
            }
            Err(err) => {
                let _ = txn.rollback().await;
                return Err(err);
            }
        }
    }
    unreachable!("reconcile retry loop always returns")
}

/// Write one synthesized canonical sibling (the classified predecessor or
/// competitor) inside the event transaction, with its own cascade-state and
/// source-health bracket; returns the hash when derived state changed.
async fn upsert_synthesized_sibling<C: GenericClient>(
    client: &C,
    sibling: &ClassifiedHeader,
) -> Result<Option<Vec<u8>>> {
    let before = load_block_cascade_state(client, &sibling.hash).await?;
    let sh_before =
        crate::source_health_sql::snapshot_parent_contribution(client, &sibling.hash).await?;
    upsert_synthesized_canonical(client, sibling).await?;
    let after = load_block_cascade_state(client, &sibling.hash).await?;
    let sh_after =
        crate::source_health_sql::snapshot_parent_contribution(client, &sibling.hash).await?;
    crate::source_health_sql::apply_source_health_diff(client, &sh_before, &sh_after).await?;
    Ok((before != after).then(|| sibling.hash.clone()))
}

/// Reconcile one event's derived rows inside an already-open transaction; the
/// single in-txn body shared by the cascade path and every mutation-module
/// wrapper.
///
/// Lock ordering: classify the parent, fold in `effective_classification`,
/// derive the full lock-hash set (parent + prev + predecessor + competitor)
/// and acquire it sorted-and-deduped via `lock_block_hashes` BEFORE any
/// mutation, so all writers acquire the same hashes in the same global order
/// and cannot deadlock. After locking, re-load the event and re-derive: if it
/// drifted (different event, or required locks not covered by what we hold) we
/// raise `ReconcileLockSetChanged` so the caller retries rather than mutating
/// under a stale lock set. The `expected_parent_hash` and `PrimaryDiff::Wrapper`
/// parent-hash guards catch the same drift earlier.
///
/// Near / target-failing events early-return with no changes (they hold no
/// derived state). The `primary` argument fixes who owns the primary parent's
/// source_health before/after diff; see the inline note on the parameter.
/// Returns the sorted-deduped set of parent hashes whose derived cascade-state
/// actually changed (primary plus any synthesized siblings), which the caller
/// feeds back into `enqueue_dependents`.
pub(crate) async fn reconcile_one_event_in_txn<C: GenericClient>(
    client: &C,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
    // Who owns the primary parent's source_health diff. `Wrapper` means a
    // mutation-module entry point already opened a `PrimarySourceHealthBracket`
    // BEFORE its own mutation and owns that diff, so this function must NOT
    // diff the primary hash (it would double-apply or use a post-mutation
    // `before`); it still owns the synthesized predecessor/competitor diffs,
    // whose hashes never overlap the wrapper-owned primary. `Reconcile`
    // (reclassify / bulk reconcile / cascade) means there is no wrapper and
    // this function's `before` IS pre-mutation, so it owns the primary diff
    // too. The variant requires an opened bracket, so ownership can no longer
    // be claimed without the snapshot having been taken.
    primary: PrimaryDiff<'_>,
) -> Result<Vec<Vec<u8>>> {
    let initial_event = load_event(client, event_id).await?;
    if initial_event.skips_parent_read_model() {
        return Ok(Vec::new());
    }
    if preclassified
        .as_ref()
        .and_then(|preclassified| preclassified.expected_parent_hash.as_ref())
        .is_some_and(|hash| hash != &initial_event.btc_parent_header_hash)
    {
        return Err(ReconcileLockSetChanged { event_id }.into());
    }
    // A wrapper-owned bracket must cover THIS event's parent; a mismatch means
    // the event moved under the wrapper (same drift family as the
    // expected_parent_hash check above), so force the retry path rather than
    // mis-attribute the source_health diff.
    if let PrimaryDiff::Wrapper(bracket) = &primary
        && bracket.parent_hash() != initial_event.btc_parent_header_hash.as_slice()
    {
        return Err(ReconcileLockSetChanged { event_id }.into());
    }

    let (initial_header, initial_classification) =
        classify_event_parent(client, &initial_event, classifier, preclassified.clone()).await?;
    let classification = effective_classification(
        client,
        &initial_event,
        &initial_header,
        initial_classification.clone(),
    )
    .await?;
    let lock_hashes = classification_lock_hashes(&initial_event, &classification);
    lock_block_hashes(client, &lock_hashes).await?;

    let Some((event, header, classification)) = load_locked_reconcile_event(
        client,
        event_id,
        classifier,
        &initial_event,
        initial_header,
        initial_classification,
        preclassified.as_ref(),
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    let classification = effective_classification(client, &event, &header, classification).await?;
    let required_lock_hashes = classification_lock_hashes(&event, &classification);
    if !lock_hashes_cover(&lock_hashes, &required_lock_hashes) {
        return Err(ReconcileLockSetChanged { event_id: event.id }.into());
    }

    let before = load_block_cascade_state(client, &event.btc_parent_header_hash).await?;
    // Primary parent source_health snapshot, owned here only when no wrapper owns
    // it. `before` here is genuinely pre-mutation for the no-wrapper callers
    // (apply_event_classification below is the first mutation).
    let sh_primary_before = match primary {
        PrimaryDiff::Wrapper(_) => None,
        PrimaryDiff::Reconcile => Some(
            crate::source_health_sql::snapshot_parent_contribution(
                client,
                &event.btc_parent_header_hash,
            )
            .await?,
        ),
    };

    apply_event_classification(client, &event, &classification).await?;

    let mut changed_hashes = Vec::new();
    for sibling in [
        classification.canonical_predecessor_header.as_ref(),
        classification.canonical_competitor_header.as_ref(),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(changed) = upsert_synthesized_sibling(client, sibling).await? {
            changed_hashes.push(changed);
        }
    }

    rebuild_parent_read_model(client, &event.btc_parent_header_hash, Some(&classification)).await?;
    let after = load_block_cascade_state(client, &event.btc_parent_header_hash).await?;
    if let Some(sh_before) = sh_primary_before {
        let sh_after = crate::source_health_sql::snapshot_parent_contribution(
            client,
            &event.btc_parent_header_hash,
        )
        .await?;
        crate::source_health_sql::apply_source_health_diff(client, &sh_before, &sh_after).await?;
    }
    if before != after {
        changed_hashes.push(event.btc_parent_header_hash);
    }
    changed_hashes.sort();
    changed_hashes.dedup();
    Ok(changed_hashes)
}

async fn load_locked_reconcile_event<C: GenericClient>(
    client: &C,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    initial_event: &MergeMiningEvent,
    initial_header: Header,
    initial_classification: ParentClassification,
    preclassified: Option<&PreclassifiedParent>,
) -> Result<Option<(MergeMiningEvent, Header, ParentClassification)>> {
    let event = load_event(client, event_id).await?;
    if event.skips_parent_read_model() {
        return Ok(None);
    }
    if event == *initial_event {
        return Ok(Some((event, initial_header, initial_classification)));
    }
    if preclassified.is_some() {
        return Err(ReconcileLockSetChanged { event_id: event.id }.into());
    }
    let (header, classification) = classify_event_parent(client, &event, classifier, None).await?;
    Ok(Some((event, header, classification)))
}

/// Deserialize the event's stored parent header and classify it. With a
/// `preclassified` parent the stored `ParentClassification` is reused verbatim
/// (the trusted form supplied by a seeding caller); otherwise the parent's
/// prev-hash preflight is loaded and the classifier is run. This is the
/// event-derived classification form, distinct from the persisted
/// `block`-row state and the preflight form.
pub(crate) async fn classify_event_parent<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
) -> Result<(Header, ParentClassification)> {
    let header: Header = deserialize(&event.btc_parent_header_bytes)
        .with_context(|| format!("deserialize parent header for event {}", event.id))?;
    let classification = match preclassified {
        Some(preclassified) => preclassified.classification,
        None => {
            let preflight =
                load_parent_preflight(client, &event.btc_parent_prev_header_hash).await?;
            classifier.classify_parent(&header, preflight).await?
        }
    };
    Ok((header, classification))
}

/// Pre-compute a parent classification OUTSIDE the reconcile transaction so the
/// in-txn body can skip the (potentially RPC-backed) classifier while holding
/// locks. Returns `None` when the classifier is disabled or the event is near /
/// target-failing (those reconcile to no derived state). The result carries the
/// event's `expected_parent_hash` so reconcile can detect the event drifting
/// under it and force the retry path.
pub(crate) async fn preclassify_event_parent(
    client: &Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<Option<PreclassifiedParent>> {
    if !classifier.is_enabled() {
        return Ok(None);
    }

    let event = load_event(client, event_id).await?;
    if event.skips_parent_read_model() {
        return Ok(None);
    }
    let (_, classification) = classify_event_parent(client, &event, classifier, None).await?;
    Ok(Some(PreclassifiedParent::for_event(&event, classification)))
}

/// The full advisory-lock hash set for an event: the parent hash, its prev
/// hash, plus any predecessor/competitor hashes from the classification, sorted
/// and deduped. Sorting fixes a global lock-acquisition order across all writers
/// so they cannot deadlock; the dedup keeps `lock_block_hashes` idempotent.
pub(crate) fn classification_lock_hashes(
    event: &MergeMiningEvent,
    classification: &ParentClassification,
) -> Vec<Vec<u8>> {
    let mut hashes = vec![
        event.btc_parent_header_hash.clone(),
        event.btc_parent_prev_header_hash.clone(),
    ];
    push_classification_lock_hashes(&mut hashes, classification);
    hashes.sort();
    hashes.dedup();
    hashes
}

/// Append the predecessor/competitor block hashes a classification implies to
/// `hashes` (both the bare `canonical_competitor_hash` and the competitor /
/// predecessor header hashes, when present). Shared by `classification_lock_hashes`
/// here and by the mutation-module lock-set builder in lib.rs so the two derive
/// the identical extra-lock set.
pub(crate) fn push_classification_lock_hashes(
    hashes: &mut Vec<Vec<u8>>,
    classification: &ParentClassification,
) {
    if let Some(hash) = &classification.canonical_competitor_hash {
        hashes.push(hash.clone());
    }
    if let Some(competitor) = &classification.canonical_competitor_header {
        hashes.push(competitor.hash.clone());
    }
    if let Some(predecessor) = &classification.canonical_predecessor_header {
        hashes.push(predecessor.hash.clone());
    }
}

/// True if every hash in `required` is present in the sorted `locked` set
/// (binary search assumes `locked` is sorted, as `classification_lock_hashes`
/// returns it). Used post-re-classify to confirm the locks already held cover
/// the locks the re-derived classification needs; a miss forces
/// `ReconcileLockSetChanged` instead of mutating under an incomplete lock set.
pub(crate) fn lock_hashes_cover(locked: &[Vec<u8>], required: &[Vec<u8>]) -> bool {
    required
        .iter()
        .all(|hash| locked.binary_search(hash).is_ok())
}

/// Compute the exact advisory-lock hash set `reconcile_one_event_in_txn` will
/// acquire for this event, mirroring its classify -> effective -> lock_hashes
/// derivation. Used by revoke/restore to pre-acquire the same set BEFORE their
/// `revoked_at` UPDATE.
pub(crate) async fn event_reconcile_lock_hashes<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
) -> Result<Vec<Vec<u8>>> {
    let (header, classification) =
        classify_event_parent(client, event, classifier, preclassified).await?;
    let classification = effective_classification(client, event, &header, classification).await?;
    Ok(classification_lock_hashes(event, &classification))
}

/// Acquire the parent advisory lock(s) for a revoke/restore source_health diff
/// BEFORE the `revoked_at` UPDATE. A near / target-failing event takes a
/// parent-only lock (reconcile early-returns for it and never locks a broader
/// set, so there is no global-order hazard); any other event takes the FULL
/// reconcile lock set so the later in-reconcile lock is a re-entrant subset and
/// cannot deadlock against a concurrent reconcile holding a competitor lock.
pub(crate) async fn lock_event_for_source_health<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
) -> Result<()> {
    if event.skips_parent_read_model() {
        lock_block_hashes(client, std::slice::from_ref(&event.btc_parent_header_hash)).await
    } else {
        let lock_hashes =
            event_reconcile_lock_hashes(client, event, classifier, preclassified).await?;
        lock_block_hashes(client, &lock_hashes).await
    }
}

/// Reconcile a block-grain cascade item (orphan block or stale-competitor
/// dependent) that has no live anchor event.
///
/// If a non-near, non-revoked event anchors this hash, delegate to
/// `reconcile_one_event` so the richer event-derived classification path runs.
/// Otherwise rebuild the parent read-model from block-row state alone, in its
/// own transaction: lock the hash, snapshot cascade-state and source-health
/// before, `rebuild_parent_read_model(.., None)`, snapshot after, and apply the
/// source-health diff. Returns the hash when its derived cascade-state changed,
/// so the drain can fan out to its dependents.
pub(crate) async fn reconcile_one_block(
    client: &mut Client,
    hash: &[u8],
    classifier: &ConfiguredParentClassifier,
) -> Result<Vec<Vec<u8>>> {
    if let Some(event_id) = find_anchor_event_for_block(client, hash).await? {
        return reconcile_one_event(client, event_id, classifier, None).await;
    }

    let txn = client
        .transaction()
        .await
        .context("begin block reconcile")?;
    lock_block_hash(&txn, hash).await?;
    let before = load_block_cascade_state(&txn, hash).await?;
    let sh_before = crate::source_health_sql::snapshot_parent_contribution(&txn, hash).await?;
    rebuild_parent_read_model(&txn, hash, None).await?;
    let after = load_block_cascade_state(&txn, hash).await?;
    let sh_after = crate::source_health_sql::snapshot_parent_contribution(&txn, hash).await?;
    crate::source_health_sql::apply_source_health_diff(&txn, &sh_before, &sh_after).await?;
    txn.commit().await.context("commit block reconcile")?;
    Ok(if before != after {
        vec![hash.to_vec()]
    } else {
        Vec::new()
    })
}
