//! Targeted reclassification of unresolved parent headers.

use super::*;
use crate::mutation::{
    CORE_RECOVERY_BATCH, drain_core_reconcile_queue_batch, enqueue_core_reconcile_primaries,
};
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;

const DEFAULT_BATCH_SIZE: i64 = 100;

/// Config for `run_reclassify_unknown_parents`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclassifyUnknownParentsConfig {
    pub batch_size: i64,
    /// Re-include parents whose `block.btc_orphan_class` is already set.
    pub recheck_orphans: bool,
    /// Consume the recheck scheduled by the Core-header cache (`--scheduled`),
    /// one page per cache-lock hold, resumable from the persisted cursor.
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

/// Keyset-paginated reclassification over `unknown`-kind parents, returning the count of
/// genuine transitions (off `unknown` or a changed `btc_orphan_class`).
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

/// Warn if `known_stale_block` is empty, since `compute_block_orphan_class` cannot exclude known stales.
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

/// Load the next keyset page of distinct `unknown` parents after `cursor` within `scope`.
/// Skips parents with a non-NULL `block.btc_orphan_class` unless `scope.orphans` is set.
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

/// Count candidates in `page` that promoted off `unknown` or changed `btc_orphan_class`.
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

/// Enqueue `page` as durable `bitcoin_core_reconcile_queue` primaries and advance the cursor atomically.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScheduledRecheckReport {
    pub pages: usize,
    pub candidates: usize,
    pub changed: usize,
    pub acknowledged: Option<i64>,
}

/// Consume the scheduled Core-header-cache recheck one page per exclusive cache-lock hold.
///
/// Each hold first drains `bitcoin_core_reconcile_queue`; once empty, it enqueues the next
/// candidate page into that queue and advances the persisted cursor atomically. Invalidating
/// cache triggers reset the bound pass and merge its scope back into pending; additive triggers
/// accumulate for a follow-up pass after the current generation is acknowledged.
pub async fn run_scheduled_recheck(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    page_size: i64,
) -> Result<ScheduledRecheckReport> {
    run_scheduled_recheck_bounded(client, classifier, page_size, None).await
}

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
