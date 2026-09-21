//! Targeted reclassification of unresolved parent headers.

use super::*;
use crate::mutation::{
    CORE_RECOVERY_BATCH, drain_core_reconcile_queue_batch, enqueue_core_reconcile_primaries,
};
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;

const DEFAULT_BATCH_SIZE: i64 = 100;

/// Config for `run_reclassify_unknown_parents`. By default re-scans only parents
/// with no `block.btc_orphan_class` yet; `recheck_orphans` re-includes
/// already-orphan-classified parents after a Core-cache refresh or classifier-logic
/// change. `batch_size` bounds the keyset page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclassifyUnknownParentsConfig {
    pub batch_size: i64,
    /// Re-include parents whose `block.btc_orphan_class` is already set (default
    /// skips them). Use after refreshing the Core header cache or changing classifier logic
    /// change to re-evaluate previously classified orphans.
    pub recheck_orphans: bool,
    /// Consume the recheck the Core-header cache scheduled (`--scheduled`)
    /// instead of scanning by hand: the scope and the cursor come from the
    /// cache state, one page per cache-lock hold, resumable after a kill.
    pub scheduled: bool,
}

impl Default for ReclassifyUnknownParentsConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            recheck_orphans: false,
            scheduled: false,
        }
    }
}

impl ReclassifyUnknownParentsConfig {
    /// Parse `reclassify-unknown-parents` CLI flags via the shared `cli_args` flag-walk.
    /// Rejects non-positive `--batch-size`.
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut config = Self::default();
        cli_args::drive_args("reclassify-unknown-parents", args, |flag, cur| {
            Ok(match flag {
                "--batch-size" => {
                    config.batch_size = cur.parse("--batch-size")?;
                    true
                }
                "--recheck-orphans" => {
                    config.recheck_orphans = true;
                    true
                }
                "--scheduled" => {
                    config.scheduled = true;
                    true
                }
                _ => false,
            })
        })?;
        if config.batch_size <= 0 {
            bail!("--batch-size must be positive");
        }
        if config.scheduled && config.recheck_orphans {
            bail!("--scheduled takes its scope from the cache state; drop --recheck-orphans");
        }
        Ok(config)
    }
}

/// Re-run parent classification over `unknown`-kind parents and count genuine
/// transitions. Requires a Core-enabled classifier (else bails). Keyset-paginates
/// distinct parent headers; a parent already orphan-classified
/// (`block.btc_orphan_class` non-NULL) is skipped unless `recheck_orphans`. Counts
/// a change only on a real transition (promotion off `unknown`, or a different
/// orphan class than the pre-pass value captured at scan time), so `count=0` keeps
/// meaning "nothing changed" across repeated rechecks. Above-horizon pending
/// verdicts stay NULL and remain eligible for a later Core-cache refresh.
pub async fn run_reclassify_unknown_parents(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: ReclassifyUnknownParentsConfig,
) -> Result<usize> {
    if !classifier.is_enabled() {
        bail!("reclassify-unknown-parents requires BITCOIN_RPC_URL");
    }
    warn_if_known_stales_empty(client).await?;
    let scope = mmm_store::RecheckScope {
        orphans: config.recheck_orphans,
        sources: None,
    };
    let mut changed = 0;
    let mut cursor: Option<(i64, i64)> = None;
    let progress = crate::classifier_progress("reclassify-unknown-parents", None, classifier);
    loop {
        let page = load_candidate_page(client, &scope, cursor, config.batch_size).await?;
        let Some(last) = page.last().map(|candidate| candidate.cursor()) else {
            break;
        };
        changed += reclassify_page(client, classifier, &page).await?;
        progress.advance(page.len() as u64);
        cursor = Some(last);
    }
    progress.finish();
    Ok(changed)
}

/// Degraded-state guard (research repo's lesson): with an EMPTY known-stale
/// membership the compute_block_orphan_class gate cannot exclude a known stale,
/// so this pass may label known stales strict/weak. Warn loudly rather than
/// silently proceeding as if membership were consulted; import it with
/// import-known-stales.
async fn warn_if_known_stales_empty(client: &Client) -> Result<()> {
    if mmm_store::count_known_stale_blocks(client).await? == 0 {
        tracing::warn!(
            "reclassify-unknown-parents: known_stale_block is EMPTY; known stales cannot be \
             excluded and may be labelled strict/weak. Import the upstream stale-blocks dataset \
             with import-known-stales."
        );
    }
    Ok(())
}

/// One unknown parent to reclassify: the representative event, its keyset
/// position, and the parent's orphan class before this pass touched it.
struct Candidate {
    event_id: i64,
    parent_hash: Vec<u8>,
    sort_child_height: i64,
    before_class: Option<String>,
}

impl Candidate {
    fn cursor(&self) -> (i64, i64) {
        (self.sort_child_height, self.event_id)
    }
}

/// Load the next page of distinct unknown parents after `cursor`, within
/// `scope`.
///
/// Skips parents already resolved by a prior pass: canonical/stale promotion
/// moves the event kind away from `unknown` (already excluded), and an
/// orphan-classified parent keeps `btc_parent_kind = 'unknown'` but has a
/// non-NULL `block.btc_orphan_class`; without the block join those orphan
/// rows would be rescanned forever. `scope.orphans` re-includes them. Rows
/// still pending (NULL after an above-horizon verdict) stay eligible so a
/// later Core-cache refresh picks them up. `scope.sources` limits the
/// candidates to events witnessed by those sources.
async fn load_candidate_page(
    client: &Client,
    scope: &mmm_store::RecheckScope,
    cursor: Option<(i64, i64)>,
    page_size: i64,
) -> Result<Vec<Candidate>> {
    let cursor_height = cursor.map(|(child_height, _)| child_height);
    let cursor_id = cursor.map(|(_, id)| id);
    let rows = client
        .query(
            "SELECT id, btc_parent_header_hash, sort_child_height, before_class \
             FROM ( \
                 SELECT DISTINCT ON (e.btc_parent_header_hash) \
                        e.id, e.btc_parent_header_hash, \
                        COALESCE(e.child_height::bigint, 2147483648::bigint) AS sort_child_height, \
                        b.btc_orphan_class AS before_class \
                 FROM merge_mining_event e \
                 LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                 WHERE e.btc_parent_kind = 'unknown' \
                   AND e.pow_validates_btc_target \
                   AND e.revoked_at IS NULL \
                   AND ($4 OR b.btc_orphan_class IS NULL) \
                   AND ($5::text[] IS NULL \
                        OR e.source_id IN (SELECT id FROM source WHERE code = ANY($5::text[]))) \
                 ORDER BY e.btc_parent_header_hash, e.child_height NULLS LAST, e.id \
             ) candidates \
             WHERE $2::bigint IS NULL \
                OR (sort_child_height, id) > ($2::bigint, $3::bigint) \
             ORDER BY sort_child_height, id \
             LIMIT $1",
            &[
                &page_size,
                &cursor_height,
                &cursor_id,
                &scope.orphans,
                &scope.sources,
            ],
        )
        .await
        .context("load unknown parents for reclassification")?;
    Ok(rows
        .iter()
        .map(|row| Candidate {
            event_id: row.get(0),
            parent_hash: row.get(1),
            sort_child_height: row.get(2),
            before_class: row.get(3),
        })
        .collect())
}

/// Reclassify one page by hand, each parent with its in-memory dependent
/// cascade, and count genuine transitions.
async fn reclassify_page(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    page: &[Candidate],
) -> Result<usize> {
    for candidate in page {
        reconcile_from_merge_mining_event_with_preclassification(
            client,
            candidate.event_id,
            classifier,
            None,
            None,
        )
        .await?;
    }
    count_changed(client, page).await
}

/// Count genuine transitions in a page: a canonical/stale promotion (event
/// kind leaves `unknown`) or a different orphan class than before (NULL to
/// non-NULL on a first pass, or a verdict change on a recheck). A re-included
/// parent whose class is unchanged, and an above-horizon pending verdict
/// (still NULL), are NOT counted, so a count of zero keeps meaning "no
/// scanned parent changed" even across repeated rechecks. `before_class` was
/// captured at scan time (DISTINCT ON keeps one candidate row per parent, so
/// no in-page reconcile of a sibling event can stale it).
async fn count_changed(client: &Client, page: &[Candidate]) -> Result<usize> {
    let event_ids: Vec<i64> = page.iter().map(|candidate| candidate.event_id).collect();
    let rows = client
        .query(
            "SELECT e.id, e.btc_parent_kind, b.btc_orphan_class \
             FROM merge_mining_event e \
             LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
             WHERE e.id = ANY($1::bigint[])",
            &[&event_ids],
        )
        .await
        .context("reload reclassified events")?;
    let after: HashMap<i64, (String, Option<String>)> = rows
        .into_iter()
        .map(|row| (row.get(0), (row.get(1), row.get(2))))
        .collect();
    Ok(page
        .iter()
        .filter(|candidate| {
            after
                .get(&candidate.event_id)
                .is_some_and(|(kind, orphan_class)| {
                    kind != ParentKind::Unknown.as_db_str()
                        || *orphan_class != candidate.before_class
                })
        })
        .count())
}

/// Hand one page to the durable Core suffix cascade and advance the cursor
/// past it in the same transaction: every candidate becomes a strict
/// primary of `bitcoin_core_reconcile_queue`, whose drain reconciles it and
/// expands its dependents durably, so a kill between a parent's commit and
/// its descendants' reconciliation loses nothing the cursor has passed.
async fn queue_page(
    client: &mut Client,
    bitcoin_source_id: i64,
    generation: i64,
    page: &[Candidate],
    last: (i64, i64),
) -> Result<()> {
    let hashes: Vec<Vec<u8>> = page
        .iter()
        .map(|candidate| candidate.parent_hash.clone())
        .collect();
    let txn = client
        .transaction()
        .await
        .context("begin scheduled recheck page")?;
    enqueue_core_reconcile_primaries(&txn, bitcoin_source_id, &hashes).await?;
    mmm_store::persist_recheck_cursor(&txn, generation, last).await?;
    txn.commit().await.context("commit scheduled recheck page")
}

/// What a scheduled recheck run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScheduledRecheckReport {
    /// Pages processed in this run, including a final empty page.
    pub pages: usize,
    /// Candidates reclassified in this run.
    pub candidates: usize,
    /// Genuine transitions among them.
    pub changed: usize,
    /// The last generation acknowledged in this run, when it finished a pass.
    pub acknowledged: Option<i64>,
}

/// Consume the recheck the Core-header cache scheduled, one page per
/// exclusive cache-lock hold, so live capture interleaves with it and a kill
/// at any point resumes from the persisted cursor.
///
/// Every page starts with the same handoff the cache refresh uses: under the
/// lock, drain one batch of the committed Core suffix cascade; if the queue is
/// not empty afterwards, release the lock and try again, so no page runs
/// while a cascade is unfinished. A page does not reconcile its candidates
/// itself: it enqueues them as durable primaries of that same cascade in the
/// transaction that advances the cursor, and the following holds drain them
/// (strict reconcile, then durable dependent expansion), so an interruption
/// between a parent's commit and its descendants' reconciliation leaves the
/// remaining work in the queue rather than in process memory, and the
/// cursor never passes work the queue does not hold. Then the pass: one
/// already bound continues from its cursor; none is bound after an
/// invalidating trigger (a shallow reorg, a boundary inside coverage, a
/// migration), which clears the pass and folds its scope back into the
/// pending scope, so the new pass starts over with the merged scope and the
/// candidates behind the old cursor are seen again under the new state.
/// Additive triggers (every Bitcoin block advances the horizon) never clear a
/// pass: they accumulate, and an empty page acknowledges the pass generation
/// and binds a follow-up pass over the accumulated scope, which normally
/// covers only rows still without a verdict. The run ends when nothing is
/// pending.
pub async fn run_scheduled_recheck(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    page_size: i64,
) -> Result<ScheduledRecheckReport> {
    run_scheduled_recheck_bounded(client, classifier, page_size, None).await
}

/// [`run_scheduled_recheck`] that stops after `max_pages` pages, so
/// a test can leave a pass bound mid-way, the way a killed process does.
#[cfg(feature = "db-integration")]
pub async fn run_scheduled_recheck_pages_for_test(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    page_size: i64,
    max_pages: usize,
) -> Result<ScheduledRecheckReport> {
    run_scheduled_recheck_bounded(client, classifier, page_size, Some(max_pages)).await
}

async fn run_scheduled_recheck_bounded(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    page_size: i64,
    max_pages: Option<usize>,
) -> Result<ScheduledRecheckReport> {
    if !classifier.is_enabled() {
        bail!("reclassify-unknown-parents --scheduled requires BITCOIN_RPC_URL");
    }
    warn_if_known_stales_empty(client).await?;
    let bitcoin_source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    let mut report = ScheduledRecheckReport::default();
    let progress = crate::classifier_progress("scheduled-recheck", None, classifier);
    // The last page handed to the queue, kept until the drain finishes so its
    // transitions can be counted from the state the drain left.
    let mut queued: Option<Vec<Candidate>> = None;
    loop {
        mmm_store::lock_bitcoin_core_header_cache(client).await?;
        let step = async {
            let queue_empty = drain_core_reconcile_queue_batch(
                client,
                bitcoin_source_id,
                classifier,
                CORE_RECOVERY_BATCH,
            )
            .await?;
            if !queue_empty {
                return Ok(PageStep::RecoveryContinues);
            }
            if let Some(page) = queued.take() {
                progress.advance(page.len() as u64);
                report.changed += count_changed(client, &page).await?;
                tracing::info!(
                    pages = report.pages,
                    candidates = report.candidates,
                    changed = report.changed,
                    "scheduled recheck page complete"
                );
                if max_pages.is_some_and(|max| report.pages >= max) {
                    return Ok(PageStep::Bounded);
                }
            }
            let state = mmm_store::load_scheduled_recheck(client).await?;
            if !state.is_pending() {
                return Ok(PageStep::NothingPending);
            }
            let pass = match state.pass {
                Some(pass) => pass,
                None => mmm_store::bind_recheck_pass(client, state.pending_scope).await?,
            };
            let page = load_candidate_page(client, &pass.scope, pass.cursor, page_size).await?;
            report.pages += 1;
            let Some(last) = page.last().map(|candidate| candidate.cursor()) else {
                mmm_store::acknowledge_recheck_pass(client, pass.generation).await?;
                return Ok(PageStep::Acknowledged(pass.generation));
            };
            queue_page(client, bitcoin_source_id, pass.generation, &page, last).await?;
            report.candidates += page.len();
            tracing::info!(
                generation = pass.generation,
                pages = report.pages,
                candidates = page.len(),
                cursor_height = last.0,
                "scheduled recheck page queued"
            );
            queued = Some(page);
            Ok(PageStep::PageQueued)
        }
        .await;
        match mmm_store::finish_bitcoin_core_header_cache_operation(client, step).await? {
            PageStep::RecoveryContinues | PageStep::PageQueued => {}
            PageStep::NothingPending | PageStep::Bounded => {
                progress.finish();
                return Ok(report);
            }
            PageStep::Acknowledged(generation) => {
                tracing::info!(generation, "scheduled recheck pass acknowledged");
                report.acknowledged = Some(generation);
            }
        }
    }
}

enum PageStep {
    RecoveryContinues,
    NothingPending,
    PageQueued,
    Bounded,
    Acknowledged(i64),
}
