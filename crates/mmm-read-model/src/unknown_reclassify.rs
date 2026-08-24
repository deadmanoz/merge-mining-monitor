//! Targeted reclassification of unresolved parent headers.

use super::*;

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
}

impl Default for ReclassifyUnknownParentsConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
            recheck_orphans: false,
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
                _ => false,
            })
        })?;
        if config.batch_size <= 0 {
            bail!("--batch-size must be positive");
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
    run_reclassify_unknown_parents_with_policy(client, classifier, config, false).await
}

/// Reclassify unknown parents while requiring fresh Core evidence for each
/// scanned candidate. Cache refreshes use this mode so a Core transport failure
/// leaves their durable retry marker set instead of being recorded as an
/// unattested `unknown` result.
pub async fn run_reclassify_unknown_parents_strict(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: ReclassifyUnknownParentsConfig,
) -> Result<usize> {
    run_reclassify_unknown_parents_with_policy(client, classifier, config, true).await
}

async fn run_reclassify_unknown_parents_with_policy(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: ReclassifyUnknownParentsConfig,
    strict_core_errors: bool,
) -> Result<usize> {
    if !classifier.is_enabled() {
        bail!("reclassify-unknown-parents requires BITCOIN_RPC_URL");
    }
    // Degraded-state guard (research repo's lesson): with an EMPTY known-stale
    // membership the compute_block_orphan_class gate cannot exclude a known stale,
    // so this pass may label known stales strict/weak. Warn loudly rather than
    // silently proceeding as if membership were consulted; import it with
    // import-known-stales.
    if mmm_store::count_known_stale_blocks(client).await? == 0 {
        tracing::warn!(
            "reclassify-unknown-parents: known_stale_block is EMPTY; known stales cannot be \
             excluded and may be labelled strict/weak. Import the upstream stale-blocks dataset \
             with import-known-stales."
        );
    }
    let nbits_table = mmm_store::load_bitcoin_core_nbits_table(client).await?;
    let mut changed = 0;
    let mut cursor: Option<(i64, i64)> = None;
    loop {
        let cursor_height = cursor.map(|(child_height, _)| child_height);
        let cursor_id = cursor.map(|(_, id)| id);
        // Skip parents already resolved by a prior pass: canonical/stale promotion
        // moves the event kind away from 'unknown' (already excluded), and an
        // orphan-classified parent keeps `btc_parent_kind = 'unknown'` but has a
        // non-NULL `block.btc_orphan_class`. Without the block join those orphan
        // rows would be rescanned forever. `--recheck-orphans` ($4) re-includes
        // them. Rows still pending (NULL after an above-horizon verdict) stay
        // eligible so a later Core-cache refresh picks them up.
        let rows = client
            .query(
                "SELECT id, sort_child_height, before_class \
                 FROM ( \
                     SELECT DISTINCT ON (e.btc_parent_header_hash) \
                            e.id, COALESCE(e.child_height::bigint, 2147483648::bigint) AS sort_child_height, \
                            b.btc_orphan_class AS before_class \
                     FROM merge_mining_event e \
                     LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                     WHERE e.btc_parent_kind = 'unknown' \
                       AND e.pow_validates_btc_target \
                       AND e.revoked_at IS NULL \
                       AND ($4 OR b.btc_orphan_class IS NULL) \
                     ORDER BY e.btc_parent_header_hash, e.child_height NULLS LAST, e.id \
                 ) candidates \
                 WHERE $2::bigint IS NULL \
                    OR (sort_child_height, id) > ($2::bigint, $3::bigint) \
                 ORDER BY sort_child_height, id \
                 LIMIT $1",
                &[
                    &config.batch_size,
                    &cursor_height,
                    &cursor_id,
                    &config.recheck_orphans,
                ],
            )
            .await
            .context("load unknown parents for reclassification")?;
        if rows.is_empty() {
            break;
        }

        for row in rows {
            let event_id: i64 = row.get(0);
            let child_height: i64 = row.get(1);
            // The parent's orphan class BEFORE this pass reconciles it. Captured at
            // scan time so progress counts a REAL transition, not merely an
            // already-classified parent re-included by --recheck-orphans (DISTINCT
            // ON keeps one candidate row per parent, so no in-batch reconcile of a
            // sibling event can stale this value).
            let before_class: Option<String> = row.get(2);
            cursor = Some((child_height, event_id));

            let preclassified = if strict_core_errors {
                preclassify_event_parent_strict(client, event_id, classifier).await?
            } else {
                None
            };
            reconcile_from_merge_mining_event_with_preclassification(
                client,
                event_id,
                classifier,
                preclassified,
                Some(&nbits_table),
            )
            .await?;
            // Count progress only on a genuine change: a canonical/stale promotion
            // (event kind leaves 'unknown') or a different orphan class than before
            // (NULL -> non-NULL on a first pass, or a verdict change on --recheck).
            // A re-included parent whose class is unchanged, and an above-horizon
            // pending verdict (still NULL), are NOT counted, so `count=0` keeps
            // meaning "no scanned parent changed" even across repeated rechecks.
            let progress_row = client
                .query_one(
                    "SELECT e.btc_parent_kind, b.btc_orphan_class \
                     FROM merge_mining_event e \
                     LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                     WHERE e.id = $1",
                    &[&event_id],
                )
                .await
                .with_context(|| format!("reload reclassified event {event_id}"))?;
            let kind: String = progress_row.get(0);
            let orphan_class: Option<String> = progress_row.get(1);
            if kind != ParentKind::Unknown.as_db_str() || orphan_class != before_class {
                changed += 1;
            }
        }
    }
    Ok(changed)
}
