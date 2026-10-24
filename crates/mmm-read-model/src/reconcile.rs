//! Event/block-grain reconciliation and the dependent-cascade queue.

use super::*;

/// Return `Some(true)` if `event_id` is revoked, `Some(false)` if live, `None` if absent.
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

/// Cascade work item (`Event` with optional preclassification, `StrictEvent`, or `Block`).
#[derive(Debug)]
pub(crate) enum ReconcileWork {
    Event(i64, Option<Box<PreclassifiedParent>>),
    /// Explicit operator repair: require fresh Core evidence instead of
    /// preserving a prior state after a transient classifier failure.
    StrictEvent(i64),
    Block(Vec<u8>),
}

/// Take the Core-header-cache reader lock before taking any parent lock.
///
/// Cache refresh takes the exclusive lock before its derived-row sweep. Every
/// mutation wrapper must therefore take this shared lock before its parent
/// advisory locks, or refresh and capture could wait on one another in reverse
/// order. A supplied table belongs to an operation that already holds the
/// exclusive cache lock.
pub(crate) async fn lock_core_header_cache_for_reconcile<C: GenericClient>(
    client: &C,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
) -> Result<()> {
    if nbits_table.is_none() {
        mmm_store::lock_bitcoin_core_header_cache_shared_in_transaction(client).await?;
    }
    Ok(())
}

/// Drain the breadth-first cascade queue to a fixed point, reconciling each parent once
/// in its own transaction up to `budget`.
pub(crate) async fn drain_reconcile_queue(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    mut queue: VecDeque<ReconcileWork>,
    budget: Option<usize>,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
) -> Result<ReconcileStats> {
    // Each item commits on its own and the queue lives only in memory.
    let _pending = crate::pending_cascade::PendingCascade::open();
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
                    reconcile_one_event(client, event_id, classifier, preclassified, nbits_table)
                        .await?;
                stats.parents_reconciled += 1;
                changed_hashes
            }
            ReconcileWork::StrictEvent(event_id) => {
                if !visited_events.insert(event_id) {
                    continue;
                }
                let changed_hashes = reconcile_one_event_with_policy(
                    client,
                    event_id,
                    classifier,
                    None,
                    nbits_table,
                    true,
                )
                .await?;
                stats.parents_reconciled += 1;
                changed_hashes
            }
            ReconcileWork::Block(hash) => {
                if !visited_blocks.insert(hash.clone()) {
                    continue;
                }
                let changed_hashes =
                    reconcile_one_block(client, &hash, classifier, nbits_table).await?;
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

pub(crate) async fn reconcile_dependents_after_changes_with_budget(
    client: &mut Client,
    hashes: &[Vec<u8>],
    classifier: &ConfiguredParentClassifier,
    cascade_budget: usize,
) -> Result<()> {
    reconcile_dependents_after_changes_with_budget_and_nbits_table(
        client,
        hashes,
        classifier,
        cascade_budget,
        None,
    )
    .await
}

/// Cascade dependent rows using a caller-validated `NbitsTable` snapshot.
pub(crate) async fn reconcile_dependents_after_changes_with_budget_and_nbits_table(
    client: &mut Client,
    hashes: &[Vec<u8>],
    classifier: &ConfiguredParentClassifier,
    cascade_budget: usize,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
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
        drain_reconcile_queue(client, classifier, queue, Some(cascade_budget), nbits_table).await?;
    }
    Ok(())
}

/// Re-reconcile derived rows that depend on `hash` under `DEFAULT_CASCADE_BUDGET`.
pub async fn reconcile_dependents_after_change(
    client: &mut Client,
    hash: &[u8],
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    reconcile_dependents_after_changes_with_budget(
        client,
        &[hash.to_vec()],
        classifier,
        DEFAULT_CASCADE_BUDGET,
    )
    .await
}

/// Append child events, derived child blocks, and stale competitor blocks depending on `hash`.
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

/// Reconcile one event in its own transaction, retrying on `ReconcileLockSetChanged`.
pub(crate) async fn reconcile_one_event(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
) -> Result<Vec<Vec<u8>>> {
    let strict_core_errors = preclassified
        .as_ref()
        .is_some_and(|preclassified| preclassified.strict_core_errors);
    reconcile_one_event_with_policy(
        client,
        event_id,
        classifier,
        preclassified,
        nbits_table,
        strict_core_errors,
    )
    .await
}

/// Durable `bitcoin_core_reconcile_queue` primary state settled inside a strict reconcile transaction.
#[derive(Clone, Copy)]
pub(crate) struct DurableCascade<'a> {
    pub(crate) source_id: i64,
    pub(crate) primary_hash: &'a [u8],
    pub(crate) generation: i64,
}

impl DurableCascade<'_> {
    async fn settle<C: GenericClient>(&self, txn: &C, changed_hashes: &[Vec<u8>]) -> Result<()> {
        let mut changed = false;
        let mut siblings = Vec::new();
        for hash in changed_hashes {
            if hash.as_slice() == self.primary_hash {
                changed = true;
            } else {
                siblings.push(hash.clone());
            }
        }
        if !siblings.is_empty() {
            crate::mutation::enqueue_core_reconcile_expansions(txn, self.source_id, &siblings)
                .await?;
        }
        crate::mutation::settle_core_reconcile_primary(
            txn,
            self.source_id,
            self.primary_hash,
            self.generation,
            changed,
        )
        .await
    }
}

async fn reconcile_one_event_with_policy(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
    strict_core_errors: bool,
) -> Result<Vec<Vec<u8>>> {
    reconcile_one_event_with_cascade(
        client,
        event_id,
        classifier,
        preclassified,
        nbits_table,
        strict_core_errors,
        None,
    )
    .await
}

async fn reconcile_one_event_with_cascade(
    client: &mut Client,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
    strict_core_errors: bool,
    durable: Option<DurableCascade<'_>>,
) -> Result<Vec<Vec<u8>>> {
    let trusted_preclassified = preclassified;
    for attempt in 0..RECONCILE_LOCK_SET_RETRY_LIMIT {
        let txn = client
            .transaction()
            .await
            .context("begin reconcile transaction")?;
        crate::mutation::lock_core_classification_view_shared(&txn, nbits_table).await?;
        let attempt_preclassified = if !classifier.is_enabled() {
            trusted_preclassified.clone()
        } else if strict_core_errors {
            preclassify_event_parent_strict(&txn, event_id, classifier).await?
        } else {
            preclassify_event_parent(&txn, event_id, classifier).await?
        };
        match reconcile_one_event_in_txn(
            &txn,
            event_id,
            classifier,
            attempt_preclassified.clone(),
            PrimaryDiff::Reconcile,
            nbits_table,
        )
        .await
        {
            Ok(changed_hashes) => {
                if let Some(durable) = &durable {
                    durable.settle(&txn, &changed_hashes).await?;
                }
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

/// Upsert one synthesized canonical sibling with cascade-state and source-health diffing.
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

/// Reconcile one event's derived rows inside an open transaction.
///
/// Acquires the sorted advisory-lock set (parent + prev + predecessor + competitor)
/// before mutating, re-verifies the lock set after locking (returning
/// `ReconcileLockSetChanged` on drift), and returns the sorted parent hashes whose
/// cascade state changed.
pub(crate) async fn reconcile_one_event_in_txn<C: GenericClient>(
    client: &C,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
    primary: PrimaryDiff<'_>,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
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
    let sh_primary_before = match primary {
        PrimaryDiff::Wrapper(_) | PrimaryDiff::BulkImport => None,
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

    rebuild_parent_read_model(
        client,
        &event.btc_parent_header_hash,
        Some(&classification),
        nbits_table,
    )
    .await?;
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

/// Deserialize and classify the event's stored parent header, reusing `preclassified` when present.
pub(crate) async fn classify_event_parent<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
) -> Result<(Header, ParentClassification)> {
    classify_event_parent_with_policy(client, event, classifier, preclassified, false).await
}

async fn classify_event_parent_with_policy<C: GenericClient>(
    client: &C,
    event: &MergeMiningEvent,
    classifier: &ConfiguredParentClassifier,
    preclassified: Option<PreclassifiedParent>,
    strict_core_errors: bool,
) -> Result<(Header, ParentClassification)> {
    let header: Header = deserialize(&event.btc_parent_header_bytes)
        .with_context(|| format!("deserialize parent header for event {}", event.id))?;
    let live = match preclassified {
        Some(preclassified) => Some(preclassified.classification),
        None if classifier.is_enabled() => {
            let preflight =
                load_parent_preflight(client, &event.btc_parent_prev_header_hash).await?;
            Some(if strict_core_errors {
                classifier
                    .classify_parent_strict(&header, preflight)
                    .await?
            } else {
                classifier.classify_parent(&header, preflight).await?
            })
        }
        None => None,
    };
    let classification = resolve_parent_classification(&header, live)?;
    Ok((header, classification))
}

/// Preclassify an event's parent before acquiring per-block locks.
pub(crate) async fn preclassify_event_parent<C: GenericClient>(
    client: &C,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<Option<PreclassifiedParent>> {
    preclassify_event_parent_with_policy(client, event_id, classifier, false).await
}

/// Preclassify an event's parent with strict Core RPC error propagation.
pub(crate) async fn preclassify_event_parent_strict<C: GenericClient>(
    client: &C,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<Option<PreclassifiedParent>> {
    preclassify_event_parent_with_policy(client, event_id, classifier, true).await
}

async fn preclassify_event_parent_with_policy<C: GenericClient>(
    client: &C,
    event_id: i64,
    classifier: &ConfiguredParentClassifier,
    strict_core_errors: bool,
) -> Result<Option<PreclassifiedParent>> {
    let event = load_event(client, event_id).await?;
    if event.skips_parent_read_model() {
        return Ok(None);
    }
    if mmm_capture::invalid_blocks::lookup(&event.btc_parent_header_hash).is_none()
        && !classifier.is_enabled()
    {
        return Ok(None);
    }
    let (_, classification) =
        classify_event_parent_with_policy(client, &event, classifier, None, strict_core_errors)
            .await?;
    Ok(Some(PreclassifiedParent::for_event(
        &event,
        classification,
        strict_core_errors,
    )))
}

/// Sorted, deduplicated advisory-lock hash set for an event and its classification.
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

/// Append predecessor and competitor block hashes implied by `classification`.
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

/// True if every hash in `required` is present in sorted `locked`.
pub(crate) fn lock_hashes_cover(locked: &[Vec<u8>], required: &[Vec<u8>]) -> bool {
    required
        .iter()
        .all(|hash| locked.binary_search(hash).is_ok())
}

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

/// Acquire parent advisory lock(s) before a revoke/restore `revoked_at` UPDATE.
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

/// Reconcile a block-grain cascade item, delegating to `reconcile_one_event` when an anchor event exists.
pub(crate) async fn reconcile_one_block(
    client: &mut Client,
    hash: &[u8],
    classifier: &ConfiguredParentClassifier,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
) -> Result<Vec<Vec<u8>>> {
    reconcile_one_block_with_policy(client, hash, classifier, nbits_table, false, None).await
}

/// Reconcile one durable suffix-queue primary while propagating Core transport failures.
pub(crate) async fn reconcile_one_block_strict(
    client: &mut Client,
    hash: &[u8],
    classifier: &ConfiguredParentClassifier,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
    durable: &DurableCascade<'_>,
) -> Result<Vec<Vec<u8>>> {
    reconcile_one_block_with_policy(client, hash, classifier, nbits_table, true, Some(*durable))
        .await
}

async fn reconcile_one_block_with_policy(
    client: &mut Client,
    hash: &[u8],
    classifier: &ConfiguredParentClassifier,
    nbits_table: Option<&mmm_capture::nbits_table::NbitsTable>,
    strict_core_errors: bool,
    durable: Option<DurableCascade<'_>>,
) -> Result<Vec<Vec<u8>>> {
    if let Some(event_id) = find_anchor_event_for_block(client, hash).await? {
        return reconcile_one_event_with_cascade(
            client,
            event_id,
            classifier,
            None,
            nbits_table,
            strict_core_errors,
            durable,
        )
        .await;
    }

    let txn = client
        .transaction()
        .await
        .context("begin block reconcile")?;
    crate::mutation::lock_core_classification_view_shared(&txn, nbits_table).await?;
    lock_block_hash(&txn, hash).await?;
    let before = load_block_cascade_state(&txn, hash).await?;
    let sh_before = crate::source_health_sql::snapshot_parent_contribution(&txn, hash).await?;
    rebuild_parent_read_model(&txn, hash, None, nbits_table).await?;
    let after = load_block_cascade_state(&txn, hash).await?;
    let sh_after = crate::source_health_sql::snapshot_parent_contribution(&txn, hash).await?;
    crate::source_health_sql::apply_source_health_diff(&txn, &sh_before, &sh_after).await?;
    let changed_hashes = if before != after {
        vec![hash.to_vec()]
    } else {
        Vec::new()
    };
    if let Some(durable) = &durable {
        durable.settle(&txn, &changed_hashes).await?;
    }
    txn.commit().await.context("commit block reconcile")?;
    Ok(changed_hashes)
}
