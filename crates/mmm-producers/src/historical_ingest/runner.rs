//! The historical-import engine: stream a recovered-evidence CSV row by row and
//! persist each accepted row through the standard producer write path.
//!
//! This is the only I/O layer of the importer. It pairs the pure `csv_source`
//! parse with the live Core `ConfiguredParentClassifier` to decide each row,
//! then writes only `merge_mining_event` (plus, for RSK, the 1:1
//! `rsk_merge_mining_evidence` sidecar) via `mmm_store`, routing through
//! `read_model::mutation` so the derived tables
//! follow the same path as live producers. Per-row failures are tallied as
//! skips, never aborts; only setup failures and capture errors propagate.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{BlockKind, ConfiguredParentClassifier, ParentClassification};
use mmm_capture::btc_orphan::BtcOrphanVerdict;
use mmm_capture::capture::{
    ClassificationProof, ParentKind, ResolvedPoolAttributions, build_event_payload_from_evidence,
    now_epoch_seconds, resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{capture_in_txn, capture_preclassified_in_txn};
use mmm_store::{
    upsert_merge_mining_event_with_attributions, upsert_pool_snapshot,
    write_elastos_capture_in_txn, write_rsk_capture_in_txn,
};
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
    /// Unknown rows whose PERSISTED `btc_orphan_class` is `excluded`, from
    /// either exclusion path: the known-stale membership gate or the
    /// wrong-difficulty-epoch check, both of which can override a local
    /// strict/weak verdict at write time.
    pub excluded: u64,
    /// Ingested unknown rows whose persisted class is still NULL (beyond the
    /// committed nBits table horizon, or a row reconciliation left without a
    /// block row).
    pub pending: u64,
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
    chain: &'a str,
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
    /// Buckets by the PERSISTED `block.kind` and `btc_orphan_class` read back
    /// after the write, never the incoming payload: capture reconciliation can
    /// retain an existing canonical/stale row when a later classification is
    /// unknown (`effective_classification`), and the read-model's known-stale
    /// membership gate can persist `excluded` for a row whose offline verdict
    /// was strict/weak. The summary reports what was stored. Call exactly once
    /// per persisted row, paired with `ingested += 1`.
    fn record_persisted(
        &mut self,
        persisted: Option<(BlockKind, Option<String>)>,
        candidate: &ImportCandidate,
    ) {
        match persisted {
            Some((BlockKind::Canonical, _)) => self.canonical += 1,
            Some((BlockKind::Stale, _)) => self.stale += 1,
            Some((BlockKind::Unknown, class)) => match class.as_deref() {
                Some("strict_btc_orphan") => self.strict_orphans += 1,
                Some("weak_btc_orphan") => self.weak_orphans += 1,
                Some("excluded") => self.excluded += 1,
                _ => self.pending += 1,
            },
            None => self.pending += 1,
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
            "historical import: rows_seen={} candidates={} ingested={} canonical={} stale={} strict_btc_orphan={} weak_btc_orphan={} excluded={} pending={} known_direct_branch_attestations={} known_descendant_branch_attestations={} skipped={}",
            self.rows_seen,
            self.candidates,
            self.ingested,
            self.canonical,
            self.stale,
            self.strict_orphans,
            self.weak_orphans,
            self.excluded,
            self.pending,
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
    // Known-stale membership guard (research repo's lesson: refuse to run without
    // the upstream stale-blocks dataset). An empty membership means the
    // compute_block_orphan_class gate cannot exclude a known stale, so any orphan
    // row this import ingests could be mislabelled strict/weak. Refuse by default;
    // --allow-empty-known-stales opts out for a deliberately membership-free run.
    let known_stale_count = mmm_store::count_known_stale_blocks(client).await?;
    if known_stale_count == 0 && !config.allow_empty_known_stales {
        bail!(
            "known_stale_block is empty: import the upstream stale-blocks dataset with \
             import-known-stales before import-dataset, or pass --allow-empty-known-stales to \
             run without known-stale exclusion (known stales may be mislabelled strict/weak)"
        );
    }
    info!(
        chain = %config.chain,
        known_stale_count,
        "starting historical import with known-stale membership"
    );
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
                chain: spec.chain,
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
/// With no classifier (`--allow-unclassified`): non-unknown rows capture
/// unclassified, unknown rows are skipped. With a classifier: Canonical/Stale
/// capture preclassified; `Near` is skipped; `Unknown` first holds any
/// known-branch selection as `KnownBranchNotClassified` (an externally
/// attested stale-branch member is never persisted as a BTC orphan), then
/// captures preclassified only when Core-absence is attested, the dataset
/// classified it as unknown, and the local orphan verdict is Strict/Weak;
/// everything else,
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
            && candidate.source_classification != SourceClassification::Unknown
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
            if candidate.source_classification != SourceClassification::Unknown {
                return Ok(ImportDecision::Skip(SkipReason::Unclassified));
            }
            if matches!(
                candidate.relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) {
                // Externally attested stale-branch member: hold it for Core or
                // an imported predecessor to place. Checked BEFORE the orphan
                // verdict so a Core-absent known-branch row whose local nBits
                // verdict happens to be strict/weak is never persisted as a
                // BTC orphan in contradiction of its attestation.
                Ok(ImportDecision::Skip(SkipReason::KnownBranchNotClassified))
            } else if matches!(
                candidate.orphan_verdict,
                Some(BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak)
            ) {
                Ok(ImportDecision::CapturePreclassified(Box::new(
                    classification,
                )))
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
    // RSK rows carry a 1:1 `rsk_merge_mining_evidence` payload that must land
    // in the same transaction as the event (mmm-api hard-errors on any
    // `auxpow:rsk` event without its sidecar row). Elastos routes through its
    // reactivating writer so a conflict with a live row auto-revoked
    // `ELASTOS_REVOKE_NON_BTC` clears that reversible, evidence-based
    // revocation, exactly as a live re-Valid capture would -- but ONLY on the
    // Core-attested (preclassified) path: an `--allow-unclassified` run gates
    // rows on nothing stronger than the header's own encoded target, which is
    // weaker than the Elastos producer's validity gate, so it must never
    // resurrect a revoked row. Sticky and manual revocations stay untouched
    // either way. Hathor deliberately stays on the generic upsert: its
    // reversible revocations (voided/superseded) track CURRENT child-DAG
    // state that a historical observation must not resurrect, and its writer
    // requires the RFC 0006 sidecar the exports cannot supply. Every other
    // chain writes the event alone. `pool_identity_id` stays NULL here --
    // the `reclassify-pools` late-fill path resolves it from the registry.
    let rsk_evidence = candidate.rsk_evidence.as_ref();
    let use_elastos_writer =
        context.chain == "elastos" && matches!(decision, ImportDecision::CapturePreclassified(_));
    let upsert = async |txn: &tokio_postgres::Transaction<'_>,
                        source_id: i64,
                        payload: &mmm_capture::capture::MergeMiningEventPayload| {
        match rsk_evidence {
            Some(evidence) => write_rsk_capture_in_txn(txn, source_id, payload, evidence).await,
            None if use_elastos_writer => {
                write_elastos_capture_in_txn(txn, source_id, payload).await
            }
            None => upsert_merge_mining_event_with_attributions(txn, source_id, payload).await,
        }
    };
    match decision {
        ImportDecision::CaptureUnclassified => {
            capture_in_txn(
                client,
                context.source_id,
                context.classifier,
                &mut payload,
                "Historical dataset",
                upsert,
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
                upsert,
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
    let parent_hash = candidate
        .evidence
        .btc_parent_header
        .block_hash()
        .to_byte_array()
        .to_vec();
    let persisted = mmm_read_model::load_persisted_kind_and_orphan_class(client, &parent_hash)
        .await
        .context("read back persisted block kind and orphan class for the import summary")?;
    summary.record_persisted(persisted, &candidate);
    Ok(())
}
