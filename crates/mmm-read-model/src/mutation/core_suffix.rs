//! Atomic near-tip Bitcoin Core canonical suffix replacement.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use tokio_postgres::{Client, GenericClient, Transaction};

use mmm_bitcoin_core::{
    BitcoinCoreBlockCoinbase, ConfiguredParentClassifier, HeightSource, ParentClassification,
};
use mmm_capture::capture::ParentKind;

use super::core_suffix_status::{
    ReplacementPending, clear_pending_error_if_queue_empty, clear_pending_error_in_transaction,
    lock_sync_state, mark_replacement_pending,
};
use super::{PrimaryDiff, lock_core_canonical_view_exclusive};
use crate::source_health_sql::MultiParentSourceHealthBracket;
use crate::{
    DEFAULT_CASCADE_BUDGET, PreclassifiedParent, ReconcileCascadeBudgetExhausted,
    find_anchor_event_for_block, lock_block_hashes, reconcile_one_block_strict,
    reconcile_one_event_in_txn, upsert_core_canonical_header_with_coinbase,
};

const SYNC_MODE_CONTIGUOUS: &str = "contiguous";

/// One fully-fetched active-chain block in the replacement suffix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreCanonicalReplacement {
    pub height: i32,
    pub header: Header,
    pub coinbase: BitcoinCoreBlockCoinbase,
}

/// One local canonical row observed while the producer planned the repair.
///
/// The producer may supply rows below the common ancestor from its wider
/// detection view. Only the topology at and above the ancestor is replaced and
/// transactionally re-checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedCoreCanonicalRow {
    pub height: i32,
    pub hash: Vec<u8>,
    pub prev_hash: Vec<u8>,
}

/// Counts and boundaries from a committed suffix replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreSuffixReplacementSummary {
    pub common_ancestor_height: i32,
    pub replaced_from_height: i32,
    pub replaced_through_height: i32,
    pub displaced_blocks: usize,
    pub queued_hashes: usize,
}

#[derive(Debug, Clone)]
struct LockedCanonicalRow {
    height: i32,
    hash: Vec<u8>,
    prev_hash: Vec<u8>,
    coinbase_txid: Option<Vec<u8>>,
    coinbase_script: Option<Vec<u8>>,
    coinbase_outputs: Option<Vec<u8>>,
}

struct SuffixMutationPlan {
    replacement_hashes: BTreeMap<i32, Vec<u8>>,
    displaced: Vec<LockedCanonicalRow>,
    affected_hashes: Vec<Vec<u8>>,
}

impl LockedCanonicalRow {
    fn expected(&self) -> ExpectedCoreCanonicalRow {
        ExpectedCoreCanonicalRow {
            height: self.height,
            hash: self.hash.clone(),
            prev_hash: self.prev_hash.clone(),
        }
    }

    fn coinbase(&self) -> Option<BitcoinCoreBlockCoinbase> {
        Some(BitcoinCoreBlockCoinbase {
            txid: self.coinbase_txid.clone()?,
            script: self.coinbase_script.clone()?,
            outputs: self.coinbase_outputs.clone()?,
        })
    }
}

/// Replace a bounded canonical suffix and enqueue its post-commit cascades.
///
/// Every replacement has a complete Core coinbase before this function starts.
/// After taking the shared cache lock and exclusive canonical-view barrier, the
/// transaction re-checks the producer's canonical view and sync cursor, locks every
/// old/new/predecessor/competitor hash in global order,
/// promotes the new suffix, demotes displaced rows to Core-attested stale rows,
/// rebinds stale rows whose canonical competitor was displaced, reconciles any
/// active events, advances the cursor only when the common ancestor connects to
/// the proven prefix, and durably enqueues every replacement and displaced
/// hash. Drain with [`drain_core_reconcile_queue`] after commit.
pub async fn replace_core_canonical_suffix(
    client: &mut Client,
    source_id: i64,
    expected_contiguous_complete_height: i32,
    common_ancestor_height: i32,
    expected_local: &[ExpectedCoreCanonicalRow],
    replacements: &[CoreCanonicalReplacement],
) -> Result<CoreSuffixReplacementSummary> {
    replace_core_canonical_suffix_validated(
        client,
        source_id,
        expected_contiguous_complete_height,
        common_ancestor_height,
        expected_local,
        replacements,
        (async |_txn| Ok(()), async |_txn| Ok(())),
    )
    .await
}

/// Validated form of [`replace_core_canonical_suffix`]. The caller's source
/// check runs after the exclusive view barrier is acquired and again after the
/// mutation is staged, immediately before commit. Either failure rolls the
/// whole suffix replacement back.
pub async fn replace_core_canonical_suffix_validated<VBefore, VAfter>(
    client: &mut Client,
    source_id: i64,
    expected_contiguous_complete_height: i32,
    common_ancestor_height: i32,
    expected_local: &[ExpectedCoreCanonicalRow],
    replacements: &[CoreCanonicalReplacement],
    validators: (VBefore, VAfter),
) -> Result<CoreSuffixReplacementSummary>
where
    VBefore: AsyncFnOnce(&Transaction<'_>) -> Result<()>,
    VAfter: AsyncFnOnce(&Transaction<'_>) -> Result<()>,
{
    let (validate_before, validate_after) = validators;
    validate_replacement(common_ancestor_height, expected_local, replacements)?;
    let first_height = replacements[0].height;
    let target = replacements
        .last()
        .expect("replacement validated as non-empty");
    let target_tip_height = target.height;
    let target_tip_hash = target.header.block_hash();
    let expected_suffix =
        normalized_expected_suffix(expected_local, common_ancestor_height, target_tip_height);
    let mut lock_hashes = expected_suffix
        .iter()
        .flat_map(|row| [row.hash.clone(), row.prev_hash.clone()])
        .collect::<Vec<_>>();
    for replacement in replacements {
        lock_hashes.push(replacement.header.block_hash().to_byte_array().to_vec());
        lock_hashes.push(replacement.header.prev_blockhash.to_byte_array().to_vec());
    }

    // READ COMMITTED is deliberate. The exclusive advisory request may block
    // behind an in-flight capture; PostgreSQL then gives the following
    // statements fresh post-wait snapshots, so the topology/event rereads see
    // that capture's commit. A SERIALIZABLE snapshot established by the
    // blocking lock statement could be stale after the wait.
    let txn = client
        .transaction()
        .await
        .context("begin Bitcoin Core canonical suffix replacement")?;
    // Cache refresh drains committed suffix work while holding its exclusive
    // lock. Taking the shared counterpart first makes a suffix either commit
    // before that drain check or wait until the refresh has completed.
    mmm_store::lock_bitcoin_core_header_cache_shared_in_transaction(&txn).await?;
    lock_core_canonical_view_exclusive(&txn).await?;
    validate_before(&txn).await?;
    let contiguous_complete_height =
        lock_and_verify_sync_state(&txn, source_id, expected_contiguous_complete_height).await?;
    lock_block_hashes(&txn, &lock_hashes).await?;
    let current =
        load_locked_canonical_rows(&txn, common_ancestor_height, target_tip_height).await?;
    verify_expected_rows(&current, &expected_suffix)?;

    let plan = plan_suffix_mutation(&current, replacements);
    let bracket = MultiParentSourceHealthBracket::open(&txn, &plan.affected_hashes).await?;
    apply_suffix_mutation(&txn, replacements, &plan).await?;

    enqueue_core_reconcile_hashes(
        &txn,
        source_id,
        &plan.affected_hashes,
        "enqueue Bitcoin Core suffix reconcile seeds",
    )
    .await?;
    let next_cch = if common_ancestor_height <= contiguous_complete_height {
        target_tip_height
    } else {
        contiguous_complete_height
    };
    let pending = ReplacementPending {
        contiguous_complete_height: next_cch,
        common_ancestor_height,
        first_height,
        target_tip_height,
        target_tip_hash,
        displaced_blocks: plan.displaced.len(),
        queued_hashes: plan.affected_hashes.len(),
    };
    mark_replacement_pending(&txn, source_id, &pending).await?;
    bracket.close(&txn).await?;
    validate_after(&txn).await?;
    txn.commit()
        .await
        .context("commit Bitcoin Core canonical suffix replacement")?;

    Ok(CoreSuffixReplacementSummary {
        common_ancestor_height,
        replaced_from_height: first_height,
        replaced_through_height: target_tip_height,
        displaced_blocks: plan.displaced.len(),
        queued_hashes: plan.affected_hashes.len(),
    })
}

fn plan_suffix_mutation(
    current: &[LockedCanonicalRow],
    replacements: &[CoreCanonicalReplacement],
) -> SuffixMutationPlan {
    let replacement_hashes = replacements
        .iter()
        .map(|replacement| {
            (
                replacement.height,
                replacement.header.block_hash().to_byte_array().to_vec(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let first_height = replacements[0].height;
    let displaced = current
        .iter()
        .filter(|row| {
            row.height >= first_height && replacement_hashes.get(&row.height) != Some(&row.hash)
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut affected_hashes = replacement_hashes
        .values()
        .cloned()
        .chain(displaced.iter().map(|row| row.hash.clone()))
        .collect::<Vec<_>>();
    affected_hashes.sort();
    affected_hashes.dedup();
    SuffixMutationPlan {
        replacement_hashes,
        displaced,
        affected_hashes,
    }
}

async fn apply_suffix_mutation<C: GenericClient>(
    client: &C,
    replacements: &[CoreCanonicalReplacement],
    plan: &SuffixMutationPlan,
) -> Result<()> {
    for replacement in replacements {
        upsert_core_canonical_header_with_coinbase(
            client,
            &replacement.header,
            replacement.height,
            Some(replacement.coinbase.clone()),
        )
        .await?;
    }
    for old in &plan.displaced {
        let competitor = plan
            .replacement_hashes
            .get(&old.height)
            .context("replacement competitor missing for displaced canonical block")?;
        demote_displaced_parent(client, old, competitor).await?;
        rebind_stale_competitors(client, old, competitor).await?;
    }
    for replacement in replacements {
        reconcile_replacement_parent(client, replacement).await?;
    }
    Ok(())
}

async fn rebind_stale_competitors<C: GenericClient>(
    client: &C,
    displaced: &LockedCanonicalRow,
    replacement_hash: &[u8],
) -> Result<()> {
    // The caller holds the exclusive canonical-view barrier, which serializes
    // every production kind/competitor writer. One set-based update per
    // displaced height keeps arbitrary stale fanout out of the Rust lock set;
    // the replacement seed discovers these rows during durable expansion.
    client
        .execute(
            "UPDATE block SET canonical_competitor_hash = $2, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE kind = 'stale' AND btc_height = $3 \
               AND canonical_competitor_hash = $1",
            &[&displaced.hash, &replacement_hash, &displaced.height],
        )
        .await
        .context("rebind stale competitors after Bitcoin Core suffix replacement")?;
    Ok(())
}

fn validate_replacement(
    common_ancestor_height: i32,
    expected_local: &[ExpectedCoreCanonicalRow],
    replacements: &[CoreCanonicalReplacement],
) -> Result<()> {
    let Some(first) = replacements.first() else {
        bail!("Bitcoin Core canonical suffix replacement must not be empty");
    };
    let expected_first = common_ancestor_height
        .checked_add(1)
        .context("Bitcoin Core suffix common ancestor height overflow")?;
    if first.height != expected_first {
        bail!(
            "replacement suffix starts at {} instead of {expected_first}",
            first.height
        );
    }
    for pair in replacements.windows(2) {
        if pair[1].height != pair[0].height + 1 {
            bail!("replacement suffix is not height-contiguous");
        }
        if pair[1].header.prev_blockhash != pair[0].header.block_hash() {
            bail!(
                "replacement suffix header link mismatch at height {}",
                pair[1].height
            );
        }
    }
    let ancestor_rows = expected_local
        .iter()
        .filter(|row| row.height == common_ancestor_height)
        .collect::<Vec<_>>();
    let [ancestor] = ancestor_rows.as_slice() else {
        bail!("expected local view must contain exactly one common-ancestor row");
    };
    if ancestor.hash.as_slice() != first.header.prev_blockhash.to_byte_array().as_slice() {
        bail!("replacement suffix does not link to the expected local common ancestor");
    }
    for row in expected_local {
        if row.hash.len() != 32 || row.prev_hash.len() != 32 {
            bail!("expected local canonical hashes must be 32 bytes");
        }
    }
    Ok(())
}

fn normalized_expected_suffix(
    expected: &[ExpectedCoreCanonicalRow],
    from: i32,
    through: i32,
) -> Vec<ExpectedCoreCanonicalRow> {
    let mut rows = expected
        .iter()
        .filter(|row| row.height >= from && row.height <= through)
        .cloned()
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| (left.height, &left.hash).cmp(&(right.height, &right.hash)));
    rows
}

async fn lock_and_verify_sync_state<C: GenericClient>(
    client: &C,
    source_id: i64,
    expected_cch: i32,
) -> Result<i32> {
    let row = client
        .query_opt(
            "SELECT contiguous_complete_height FROM bitcoin_core_sync_state \
             WHERE source_id = $1 AND sync_mode = $2 FOR UPDATE",
            &[&source_id, &SYNC_MODE_CONTIGUOUS],
        )
        .await
        .context("lock Bitcoin Core sync state for suffix replacement")?
        .context("Bitcoin Core contiguous sync state is missing")?;
    let actual: i32 = row.get(0);
    if actual != expected_cch {
        bail!(
            "Bitcoin Core sync cursor changed while planning suffix replacement: expected {expected_cch}, found {actual}"
        );
    }
    Ok(actual)
}

async fn load_locked_canonical_rows<C: GenericClient>(
    client: &C,
    from: i32,
    through: i32,
) -> Result<Vec<LockedCanonicalRow>> {
    let rows = client
        .query(
            "SELECT btc_height, btc_header_hash, btc_prev_header_hash, \
                    btc_coinbase_txid, btc_coinbase_script, btc_coinbase_outputs \
             FROM block WHERE kind = 'canonical' AND btc_height BETWEEN $1 AND $2 \
             ORDER BY btc_height, btc_header_hash FOR UPDATE",
            &[&from, &through],
        )
        .await
        .context("lock local canonical rows for suffix replacement")?;
    Ok(rows
        .into_iter()
        .map(|row| LockedCanonicalRow {
            height: row.get(0),
            hash: row.get(1),
            prev_hash: row.get(2),
            coinbase_txid: row.get(3),
            coinbase_script: row.get(4),
            coinbase_outputs: row.get(5),
        })
        .collect())
}

fn verify_expected_rows(
    current: &[LockedCanonicalRow],
    expected: &[ExpectedCoreCanonicalRow],
) -> Result<()> {
    let actual = current
        .iter()
        .map(LockedCanonicalRow::expected)
        .collect::<Vec<_>>();
    if actual != expected {
        bail!("local canonical suffix changed while Bitcoin Core replacement was being fetched");
    }
    Ok(())
}

async fn demote_displaced_parent<C: GenericClient>(
    client: &C,
    old: &LockedCanonicalRow,
    competitor_hash: &[u8],
) -> Result<()> {
    if let Some(event_id) = find_anchor_event_for_block(client, &old.hash).await? {
        let classification = ParentClassification {
            kind: ParentKind::Stale,
            height: Some(old.height),
            height_source: Some(HeightSource::BitcoinCore),
            prev_hash: old.prev_hash.clone(),
            canonical_predecessor_header: None,
            canonical_competitor_header: None,
            canonical_competitor_hash: Some(competitor_hash.to_vec()),
            coinbase: old.coinbase(),
            difficulty_epoch_ok: Some(true),
            rejection_reason: None,
            live_observed: true,
            core_attested: true,
            core_absence_attested: false,
        };
        reconcile_one_event_in_txn(
            client,
            event_id,
            &ConfiguredParentClassifier::Disabled,
            Some(PreclassifiedParent::trusted(classification)),
            PrimaryDiff::BulkImport,
            None,
        )
        .await?;
    } else {
        client
            .execute(
                "UPDATE block SET kind = 'stale', btc_height = $2, \
                     btc_height_source = 'bitcoin-core', canonical_competitor_hash = $3, \
                     live_observed = TRUE, core_attested = TRUE, pow_validated = TRUE, \
                     difficulty_epoch_ok = TRUE, btc_orphan_class = NULL, \
                     error_block_reason = NULL, updated_at = extract(epoch from now())::bigint \
                 WHERE btc_header_hash = $1",
                &[&old.hash, &old.height, &competitor_hash],
            )
            .await
            .context("demote displaced Bitcoin Core canonical block")?;
    }
    Ok(())
}

async fn reconcile_replacement_parent<C: GenericClient>(
    client: &C,
    replacement: &CoreCanonicalReplacement,
) -> Result<()> {
    let hash = replacement.header.block_hash().to_byte_array().to_vec();
    let Some(event_id) = find_anchor_event_for_block(client, &hash).await? else {
        return Ok(());
    };
    let classification = ParentClassification {
        kind: ParentKind::Canonical,
        height: Some(replacement.height),
        height_source: Some(HeightSource::BitcoinCore),
        prev_hash: replacement.header.prev_blockhash.to_byte_array().to_vec(),
        canonical_predecessor_header: None,
        canonical_competitor_header: None,
        canonical_competitor_hash: None,
        coinbase: Some(replacement.coinbase.clone()),
        difficulty_epoch_ok: Some(true),
        rejection_reason: None,
        live_observed: true,
        core_attested: true,
        core_absence_attested: false,
    };
    reconcile_one_event_in_txn(
        client,
        event_id,
        &ConfiguredParentClassifier::Disabled,
        Some(PreclassifiedParent::trusted(classification)),
        PrimaryDiff::BulkImport,
        None,
    )
    .await?;
    Ok(())
}

async fn enqueue_core_reconcile_hashes<C: GenericClient>(
    client: &C,
    source_id: i64,
    hashes: &[Vec<u8>],
    context: &'static str,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO bitcoin_core_reconcile_queue ( \
                 source_id, btc_parent_header_hash, primary_pending \
             ) SELECT $1, hash, TRUE FROM unnest($2::bytea[]) AS pending(hash) \
             ON CONFLICT (source_id, btc_parent_header_hash) DO UPDATE SET \
                 primary_pending = TRUE, \
                 generation = bitcoin_core_reconcile_queue.generation + 1, \
                 updated_at = extract(epoch from now())::bigint",
            &[&source_id, &hashes],
        )
        .await
        .context(context)?;
    Ok(())
}

/// Drain durable dependent-cascade seeds for one Bitcoin source.
///
/// Each row alternates through a durable two-phase worklist. A
/// `primary_pending` row strictly reclassifies and reconciles that parent, then
/// becomes an expansion row;
/// expansion enqueues every dependent parent and deletes its seed in one
/// transaction. Replaying an idempotent primary still expands it, so neither a
/// crash nor a cascade-budget exit can lose an in-memory frontier. Generation
/// predicates prevent older work from consuming a newer change to the same
/// hash. The pending source error clears only when the queue is empty while
/// holding the source's sync-state row lock. Strict configured classification
/// makes a transient Core failure retain the durable primary for a later drain.
pub async fn drain_core_reconcile_queue(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
) -> Result<()> {
    drain_core_reconcile_queue_with_budget(client, source_id, classifier, DEFAULT_CASCADE_BUDGET)
        .await
}

#[cfg(feature = "db-integration")]
#[doc(hidden)]
pub async fn drain_core_reconcile_queue_with_budget_for_test(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    cascade_budget: usize,
) -> Result<()> {
    drain_core_reconcile_queue_with_budget(client, source_id, classifier, cascade_budget).await
}

async fn drain_core_reconcile_queue_with_budget(
    client: &mut Client,
    source_id: i64,
    classifier: &ConfiguredParentClassifier,
    cascade_budget: usize,
) -> Result<()> {
    let mut parents_reconciled = 0_usize;
    loop {
        let Some(work) = load_next_core_reconcile_work(client, source_id).await? else {
            clear_pending_error_if_queue_empty(client, source_id).await?;
            return Ok(());
        };

        if work.primary_pending {
            if parents_reconciled >= cascade_budget {
                return Err(ReconcileCascadeBudgetExhausted {
                    budget: cascade_budget,
                }
                .into());
            }
            reconcile_one_block_strict(client, &work.hash, classifier, None)
                .await
                .with_context(|| {
                    format!(
                        "reconcile durable Bitcoin Core dependent {}",
                        hex::encode(&work.hash)
                    )
                })?;
            parents_reconciled += 1;
            mark_core_primary_reconciled(client, source_id, &work).await?;
        } else {
            expand_core_reconcile_work(client, source_id, &work).await?;
        }
    }
}

struct CoreReconcileWork {
    hash: Vec<u8>,
    primary_pending: bool,
    generation: i64,
}

async fn load_next_core_reconcile_work(
    client: &Client,
    source_id: i64,
) -> Result<Option<CoreReconcileWork>> {
    Ok(client
        .query_opt(
            "SELECT btc_parent_header_hash, primary_pending, generation \
             FROM bitcoin_core_reconcile_queue WHERE source_id = $1 \
             ORDER BY enqueued_at, btc_parent_header_hash LIMIT 1",
            &[&source_id],
        )
        .await
        .context("load Bitcoin Core suffix reconcile work")?
        .map(|row| CoreReconcileWork {
            hash: row.get(0),
            primary_pending: row.get(1),
            generation: row.get(2),
        }))
}

/// Mark a successfully reconciled parent ready for durable dependent
/// expansion. A process exit before this compare-and-swap leaves
/// `primary_pending` true, so restart safely repeats the idempotent primary.
async fn mark_core_primary_reconciled(
    client: &Client,
    source_id: i64,
    work: &CoreReconcileWork,
) -> Result<()> {
    client
        .execute(
            "UPDATE bitcoin_core_reconcile_queue SET primary_pending = FALSE, \
                 generation = generation + 1, \
                 updated_at = extract(epoch from now())::bigint \
             WHERE source_id = $1 AND btc_parent_header_hash = $2 \
               AND primary_pending = TRUE AND generation = $3",
            &[&source_id, &work.hash, &work.generation],
        )
        .await
        .context("mark Bitcoin Core dependent primary reconciled")?;
    Ok(())
}

/// Expand one durable hash to every parent that depends on it, atomically with
/// deleting the expanded row. Dependent rows enter as `primary_pending`, so a
/// crash after any earlier parent commit cannot strand a deeper frontier in
/// process memory. Expansion is unconditional after primary replay: even an
/// idempotent replay following a crash recreates its grandchildren.
async fn expand_core_reconcile_work(
    client: &mut Client,
    source_id: i64,
    work: &CoreReconcileWork,
) -> Result<()> {
    let txn = client
        .transaction()
        .await
        .context("begin Bitcoin Core durable dependent expansion")?;
    lock_sync_state(&txn, source_id).await?;
    let current = txn
        .query_opt(
            "SELECT primary_pending, generation \
             FROM bitcoin_core_reconcile_queue \
             WHERE source_id = $1 AND btc_parent_header_hash = $2 FOR UPDATE",
            &[&source_id, &work.hash],
        )
        .await
        .context("lock Bitcoin Core durable expansion work")?;
    let Some(current) = current else {
        txn.rollback()
            .await
            .context("rollback superseded Bitcoin Core expansion")?;
        return Ok(());
    };
    let current_primary: bool = current.get(0);
    let current_generation: i64 = current.get(1);
    if current_primary || current_generation != work.generation {
        txn.rollback()
            .await
            .context("rollback changed Bitcoin Core expansion generation")?;
        return Ok(());
    }

    let dependents = txn
        .query(
            "SELECT DISTINCT dependent_hash FROM ( \
                 SELECT e.btc_parent_header_hash AS dependent_hash \
                 FROM merge_mining_event e \
                 WHERE e.btc_parent_prev_header_hash = $1 \
                   AND e.btc_parent_kind <> 'near' \
                   AND e.pow_validates_btc_target \
                   AND e.revoked_at IS NULL \
                 UNION \
                 SELECT b.btc_header_hash AS dependent_hash \
                 FROM block b WHERE b.btc_prev_header_hash = $1 \
                 UNION \
                 SELECT b.btc_header_hash AS dependent_hash \
                 FROM block b \
                 WHERE b.kind = 'stale' AND b.canonical_competitor_hash = $1 \
             ) AS edges \
             WHERE dependent_hash <> $1 \
             ORDER BY dependent_hash",
            &[&work.hash],
        )
        .await
        .context("discover durable Bitcoin Core dependents")?
        .into_iter()
        .map(|row| row.get::<_, Vec<u8>>(0))
        .collect::<Vec<_>>();
    if !dependents.is_empty() {
        enqueue_core_reconcile_hashes(
            &txn,
            source_id,
            &dependents,
            "enqueue durable Bitcoin Core dependent primaries",
        )
        .await?;
    }
    txn.execute(
        "DELETE FROM bitcoin_core_reconcile_queue \
         WHERE source_id = $1 AND btc_parent_header_hash = $2 \
           AND primary_pending = FALSE AND generation = $3",
        &[&source_id, &work.hash, &work.generation],
    )
    .await
    .context("complete Bitcoin Core dependent expansion")?;
    clear_pending_error_in_transaction(&txn, source_id).await?;
    txn.commit()
        .await
        .context("commit Bitcoin Core durable dependent expansion")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::block::Version;
    use bitcoin::hash_types::TxMerkleNode;
    use bitcoin::{BlockHash, CompactTarget, hashes::Hash};

    fn header(prev: BlockHash, nonce: u32) -> Header {
        Header {
            version: Version::ONE,
            prev_blockhash: prev,
            merkle_root: TxMerkleNode::all_zeros(),
            time: nonce,
            bits: CompactTarget::from_consensus(0x207f_ffff),
            nonce,
        }
    }

    fn coinbase() -> BitcoinCoreBlockCoinbase {
        BitcoinCoreBlockCoinbase {
            txid: vec![1; 32],
            script: vec![2],
            outputs: vec![3],
        }
    }

    #[test]
    fn validates_linked_suffix_and_broad_expected_view() {
        let ancestor = header(BlockHash::all_zeros(), 1);
        let first = header(ancestor.block_hash(), 2);
        let second = header(first.block_hash(), 3);
        let replacements = vec![
            CoreCanonicalReplacement {
                height: 11,
                header: first,
                coinbase: coinbase(),
            },
            CoreCanonicalReplacement {
                height: 12,
                header: second,
                coinbase: coinbase(),
            },
        ];
        let expected = vec![
            ExpectedCoreCanonicalRow {
                height: 9,
                hash: vec![9; 32],
                prev_hash: vec![8; 32],
            },
            ExpectedCoreCanonicalRow {
                height: 10,
                hash: ancestor.block_hash().to_byte_array().to_vec(),
                prev_hash: ancestor.prev_blockhash.to_byte_array().to_vec(),
            },
        ];
        assert!(validate_replacement(10, &expected, &replacements).is_ok());
    }

    #[test]
    fn rejects_suffix_not_linked_to_common_ancestor() {
        let first = header(BlockHash::all_zeros(), 2);
        let replacements = vec![CoreCanonicalReplacement {
            height: 11,
            header: first,
            coinbase: coinbase(),
        }];
        let expected = vec![ExpectedCoreCanonicalRow {
            height: 10,
            hash: vec![7; 32],
            prev_hash: vec![6; 32],
        }];
        assert!(validate_replacement(10, &expected, &replacements).is_err());
    }
}
