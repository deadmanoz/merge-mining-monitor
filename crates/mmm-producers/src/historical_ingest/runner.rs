//! The historical-import engine: stream a recovered-evidence CSV row by row and
//! persist each accepted row through the standard producer write path.
//!
//! This is the only I/O layer of the importer. It pairs the pure `csv_source`
//! parse with the live Core `ConfiguredParentClassifier` to decide each row,
//! then writes only `merge_mining_event` (plus the 1:1 sidecar) via
//! `mmm_store`, routing through `read_model::mutation` so the derived tables
//! follow the same path as live producers. Per-row failures are tallied as
//! skips, never aborts; only setup failures and capture errors propagate.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::btc_orphan::BtcOrphanVerdict;
use mmm_capture::capture::{
    ClassificationProof, ParentKind, ResolvedPoolAttributions, build_event_payload_from_evidence,
    now_epoch_seconds, resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{capture_in_txn, capture_preclassified_in_txn};
use mmm_store::{upsert_merge_mining_event_with_attributions, upsert_pool_snapshot};
use tokio_postgres::Client;
use tracing::info;

use super::config::{HistoricalImportConfig, historical_chain_spec};
use super::csv_source::{
    CsvLayout, ImportCandidate, RelevanceSelection, SkipReason, SourceClassification,
    candidate_from_record, load_relevance_filter,
};

/// Running tallies for one import, surfaced to the operator via `print`.
///
/// `rows_seen` counts every CSV record; `candidates` those that passed both the
/// parse gate and the decision gate; `ingested` those actually persisted. The
/// per-kind and per-attestation counters partition `ingested`, and `skipped`
/// maps each `SkipReason::as_str` to its drop count. Counters reconcile:
/// rows_seen = ingested + sum(skipped).
#[derive(Debug, Default, Clone)]
pub struct HistoricalImportSummary {
    pub rows_seen: u64,
    pub candidates: u64,
    pub ingested: u64,
    pub canonical: u64,
    pub stale: u64,
    pub strict_orphans: u64,
    pub weak_orphans: u64,
    pub known_direct_branch_attestations: u64,
    pub known_descendant_branch_attestations: u64,
    pub skipped: BTreeMap<&'static str, u64>,
}

/// The per-row verdict from `import_decision`, deciding which capture path (if
/// any) a candidate takes. `Skip` short-circuits before any DB write.
enum ImportDecision {
    /// Capture without a preset parent kind: the reconciler classifies later.
    CaptureUnclassified,
    /// Capture with a Core-attested classification already attached (boxed to
    /// keep the enum small).
    CapturePreclassified(Box<ParentClassification>),
    Skip(SkipReason),
}

/// Per-import shared state threaded into `import_candidate`, resolved once before
/// the row loop so each row reuses the same source id, classifier, pool resolver,
/// and slug-to-id map rather than recomputing them.
struct ImportContext<'a> {
    source_id: i64,
    classifier: &'a ConfiguredParentClassifier,
    resolver: &'a PoolResolver,
    pool_ids_by_slug: &'a HashMap<String, i64>,
}

impl HistoricalImportSummary {
    /// Increment the drop count for `reason`, keyed by its stable string.
    fn skip(&mut self, reason: SkipReason) {
        *self.skipped.entry(reason.as_str()).or_default() += 1;
    }

    /// Bump the per-kind and per-attestation counters after a successful persist.
    ///
    /// Partitions an ingested row by its persisted `ParentKind` (a `Near` here
    /// would contradict the decision gate and is recorded defensively as a
    /// skip), then layers on any known-branch attestation tally. Call exactly
    /// once per persisted row, paired with `ingested += 1`.
    fn record_persisted(&mut self, payload_kind: ParentKind, candidate: &ImportCandidate) {
        match payload_kind {
            ParentKind::Canonical => self.canonical += 1,
            ParentKind::Stale => self.stale += 1,
            ParentKind::Near => self.skip(SkipReason::TargetInvalid),
            ParentKind::Unknown => match candidate.orphan_verdict {
                Some(BtcOrphanVerdict::Strict) => self.strict_orphans += 1,
                Some(BtcOrphanVerdict::Weak) => self.weak_orphans += 1,
                _ => self.skip(SkipReason::Unclassified),
            },
        }
        match candidate.relevance_selection {
            Some(RelevanceSelection::KnownDirectStale) => {
                self.known_direct_branch_attestations += 1;
            }
            Some(RelevanceSelection::KnownStaleDescendant) => {
                self.known_descendant_branch_attestations += 1;
            }
            Some(RelevanceSelection::StrictBtcOrphan | RelevanceSelection::WeakBtcOrphan)
            | None => {}
        }
    }

    /// Print the one-line space-delimited summary to stdout (the operator-facing
    /// report; the `skipped` map renders as comma-joined `reason:count` pairs).
    pub fn print(&self) {
        println!(
            "historical import: rows_seen={} candidates={} ingested={} canonical={} stale={} strict_btc_orphan={} weak_btc_orphan={} known_direct_branch_attestations={} known_descendant_branch_attestations={} skipped={}",
            self.rows_seen,
            self.candidates,
            self.ingested,
            self.canonical,
            self.stale,
            self.strict_orphans,
            self.weak_orphans,
            self.known_direct_branch_attestations,
            self.known_descendant_branch_attestations,
            self.skipped
                .iter()
                .map(|(reason, count)| format!("{reason}:{count}"))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}

/// Stream the configured CSV and persist accepted rows, returning the tallies.
///
/// Refuses to run without a live classifier unless `--allow-unclassified` is set
/// (the orphan-import safety guard). Resolves the `source_id`, upserts the
/// embedded pool snapshot, loads the relevance filter, then iterates rows:
/// parse, decide, and capture, skipping (not aborting) on per-row failure.
/// Honors `--limit` (caps `ingested`) and logs progress every `batch_size`
/// ingests. Setup failures and capture errors propagate.
pub async fn run_historical_import(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
) -> Result<HistoricalImportSummary> {
    if !classifier.is_enabled() && !config.allow_unclassified {
        bail!(
            "BITCOIN_RPC_URL is required for import-dataset unless --allow-unclassified is passed"
        );
    }
    let spec = historical_chain_spec(&config.chain)
        .ok_or_else(|| anyhow::anyhow!("unsupported historical chain {:?}", config.chain))?;
    let source_id = mmm_store::get_source_id(client, spec.source_code).await?;
    let resolver = PoolResolver::from_default_snapshot().context("load embedded pool snapshot")?;
    let pool_ids_by_slug = upsert_pool_snapshot(client, resolver.snapshot()).await?;
    let relevance = load_relevance_filter(config.relevance_path.as_deref(), spec.chain)?;
    let file = std::fs::File::open(&config.csv_path)
        .with_context(|| format!("open historical CSV {}", config.csv_path.display()))?;
    let mut reader = csv::Reader::from_reader(file);
    let headers = reader
        .headers()
        .context("read historical CSV header")?
        .clone();
    let layout = CsvLayout::new(&headers, spec)?;
    let mut summary = HistoricalImportSummary::default();

    for record in reader.records() {
        summary.rows_seen += 1;
        let record = match record {
            Ok(record) => record,
            Err(_) => {
                summary.skip(SkipReason::Malformed);
                continue;
            }
        };
        let candidate = match candidate_from_record(spec, &layout, &record, &relevance) {
            Ok(candidate) => candidate,
            Err(reason) => {
                summary.skip(reason);
                continue;
            }
        };
        let decision = import_decision(client, classifier, config, &candidate).await?;
        if let ImportDecision::Skip(reason) = decision {
            summary.skip(reason);
            continue;
        }
        summary.candidates += 1;
        import_candidate(
            client,
            &ImportContext {
                source_id,
                classifier,
                resolver: &resolver,
                pool_ids_by_slug: &pool_ids_by_slug,
            },
            &mut summary,
            candidate,
            decision,
        )
        .await?;
        if let Some(limit) = config.limit
            && summary.ingested as usize >= limit
        {
            break;
        }
        if summary.ingested > 0 && summary.ingested.is_multiple_of(config.batch_size as u64) {
            info!(
                chain = spec.chain,
                ingested = summary.ingested,
                rows_seen = summary.rows_seen,
                "historical import progress"
            );
        }
    }
    Ok(summary)
}

/// Decide a candidate's fate, the layer where live Core classification meets the
/// dataset's own labels.
///
/// With no classifier (`--allow-unclassified`): non-orphan rows capture
/// unclassified, orphans are skipped. With a classifier: Canonical/Stale capture
/// preclassified; `Near` is skipped; `Unknown` captures preclassified only when
/// Core-absence is attested, the dataset called it an orphan, and the local
/// orphan verdict is Strict/Weak. An attested-orphan known-branch row that did
/// not classify becomes `KnownBranchNotClassified`; everything else,
/// `Unclassified`. Reads parent preflight by prev_blockhash in
/// `to_byte_array` (wire) order.
async fn import_decision(
    client: &Client,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    candidate: &ImportCandidate,
) -> Result<ImportDecision> {
    if !classifier.is_enabled() {
        if config.allow_unclassified
            && candidate.source_classification != SourceClassification::Orphan
        {
            return Ok(ImportDecision::CaptureUnclassified);
        }
        return Ok(ImportDecision::Skip(SkipReason::Unclassified));
    }
    let prev_hash = candidate
        .evidence
        .btc_parent_header
        .prev_blockhash
        .to_byte_array()
        .to_vec();
    let preflight = mmm_read_model::load_parent_preflight(client, &prev_hash).await?;
    let classification = classifier
        .classify_parent(&candidate.evidence.btc_parent_header, preflight)
        .await
        .with_context(|| {
            format!(
                "preclassify historical parent {}",
                candidate.btc_parent_display_hash
            )
        })?;
    match classification.kind {
        ParentKind::Canonical | ParentKind::Stale => Ok(ImportDecision::CapturePreclassified(
            Box::new(classification),
        )),
        ParentKind::Near => Ok(ImportDecision::Skip(SkipReason::Unclassified)),
        ParentKind::Unknown => {
            if !classification.core_absence_attested {
                return Ok(ImportDecision::Skip(SkipReason::Unclassified));
            }
            if candidate.source_classification != SourceClassification::Orphan {
                return Ok(ImportDecision::Skip(SkipReason::Unclassified));
            }
            if matches!(
                candidate.orphan_verdict,
                Some(BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak)
            ) {
                Ok(ImportDecision::CapturePreclassified(Box::new(
                    classification,
                )))
            } else if matches!(
                candidate.relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) {
                Ok(ImportDecision::Skip(SkipReason::KnownBranchNotClassified))
            } else {
                Ok(ImportDecision::Skip(SkipReason::Unclassified))
            }
        }
    }
}

/// Persist one decided candidate through the shared producer write path.
///
/// Resolves pool attribution from the parent coinbase, builds the standard event
/// payload, then routes to `capture_in_txn` (unclassified) or
/// `capture_preclassified_in_txn` (Core-attested) so the single transaction
/// writes `merge_mining_event` via `upsert_merge_mining_event_with_attributions`
/// and lets the read model derive the rest. `Skip` is unreachable here by
/// construction (the caller filters it out first). On success bumps `ingested`
/// and records the persisted kind.
async fn import_candidate(
    client: &mut Client,
    context: &ImportContext<'_>,
    summary: &mut HistoricalImportSummary,
    candidate: ImportCandidate,
    decision: ImportDecision,
) -> Result<()> {
    let attributions = candidate
        .evidence
        .btc_parent_coinbase_script
        .as_deref()
        .and_then(|script| {
            resolve_parent_pool_attribution_from_coinbase(
                script,
                &[],
                context.resolver,
                context.pool_ids_by_slug,
            )
        })
        .into_iter()
        .collect();
    let pool_attributions = ResolvedPoolAttributions { attributions };
    let mut payload = build_event_payload_from_evidence(
        candidate.evidence.clone(),
        pool_attributions,
        ClassificationProof::default(),
        now_epoch_seconds()?,
    )?;
    match decision {
        ImportDecision::CaptureUnclassified => {
            capture_in_txn(
                client,
                context.source_id,
                context.classifier,
                &mut payload,
                "Historical dataset",
                async |txn, source_id, payload| {
                    upsert_merge_mining_event_with_attributions(txn, source_id, payload).await
                },
            )
            .await
        }
        ImportDecision::CapturePreclassified(parent_classification) => {
            capture_preclassified_in_txn(
                client,
                context.source_id,
                context.classifier,
                &mut payload,
                *parent_classification,
                "Historical dataset",
                async |txn, source_id, payload| {
                    upsert_merge_mining_event_with_attributions(txn, source_id, payload).await
                },
            )
            .await
        }
        ImportDecision::Skip(_) => unreachable!("skip decisions do not reach import_candidate"),
    }
    .with_context(|| {
        format!(
            "capture historical parent {}",
            candidate.btc_parent_display_hash
        )
    })?;
    summary.ingested += 1;
    summary.record_persisted(payload.btc_parent_kind, &candidate);
    Ok(())
}
