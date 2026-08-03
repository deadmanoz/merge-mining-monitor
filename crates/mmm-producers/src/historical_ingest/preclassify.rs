//! Bounded Bitcoin-parent preclassification for historical publication rows.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::Hash as _;
use futures::{TryStreamExt, stream::FuturesUnordered};
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::btc_orphan::BtcOrphanVerdict;
use mmm_capture::capture::ParentKind;
use tokio_postgres::GenericClient;

use super::config::{HistoricalChainSpec, HistoricalImportConfig};
use super::csv_source::{
    ImportCandidate, RelevanceSelection, SkipReason, SourceClassification, candidate_from_record,
};
use super::publication::ArtifactPreflight;

/// The per-row verdict, deciding which capture path (if any) a candidate takes.
pub(super) enum ImportDecision {
    CaptureUnclassified,
    CapturePreclassified(Box<ParentClassification>),
    Skip(SkipReason),
}

struct PendingDecision {
    row_number: usize,
    parent_hash: Vec<u8>,
    source_classification: SourceClassification,
    relevance_selection: Option<RelevanceSelection>,
    orphan_verdict: Option<BtcOrphanVerdict>,
}

/// Validate every normalized candidate and resolve parent classifications before
/// the chain transaction begins.
///
/// Positive Core classifications and Core-absence results are cached by parent
/// hash so repeated observations across a chain do not repeat RPC work. The
/// pass keeps a rolling bounded set of unresolved unique parents, so cached
/// and repeated rows cannot consume classifier capacity and each completed slot
/// is refilled without waiting for slower peers. Manifest decisions queue in
/// source order and drain as their classifications arrive, stopping promptly on
/// the first known-fatal row while bounding speculative lookahead.
pub(super) async fn preflight_and_classify_candidates<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    spec: &HistoricalChainSpec,
    artifact: &mut ArtifactPreflight,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    let expected_rows = artifact.row_count;
    let (mut reader, layout) = artifact.open_reader(spec)?;
    let concurrency = classifier.max_concurrency();
    let mut in_flight = FuturesUnordered::new();
    let mut pending_hashes = HashSet::with_capacity(concurrency);
    let mut pending_decisions = VecDeque::new();
    let max_pending_decisions = config.batch_size.max(concurrency);
    let mut rows = 0_u64;
    for (offset, record) in reader.records().enumerate() {
        let record = record.with_context(|| {
            format!(
                "parse normalized artifact {} row {}",
                config.csv_path.display(),
                offset + 2
            )
        })?;
        rows += 1;
        let candidate =
            match candidate_from_record(spec, &layout, &record, config.publication_ref()) {
                Ok(candidate) => candidate,
                Err(_) if config.manifest_path.is_none() => continue,
                Err(reason) => {
                    bail!(
                        "normalized artifact {} row {} failed {}",
                        config.csv_path.display(),
                        offset + 2,
                        reason.as_str()
                    );
                }
            };
        if classifier.is_enabled() {
            let parent_hash = candidate
                .evidence
                .btc_parent_header
                .block_hash()
                .to_byte_array()
                .to_vec();
            if config.manifest_path.is_some() {
                pending_decisions.push_back(PendingDecision {
                    row_number: offset + 2,
                    parent_hash: parent_hash.clone(),
                    source_classification: candidate.source_classification,
                    relevance_selection: candidate.relevance_selection,
                    orphan_verdict: candidate.orphan_verdict,
                });
            }
            if !classifications.contains_key(&parent_hash)
                && pending_hashes.insert(parent_hash.clone())
            {
                in_flight.push(resolve_candidate(
                    client,
                    classifier,
                    candidate,
                    parent_hash,
                ));
            }
            drain_ready_publication_decisions(config, &mut pending_decisions, classifications)?;
            while in_flight.len() >= concurrency || pending_decisions.len() >= max_pending_decisions
            {
                let Some((resolved_hash, classification)) = in_flight.try_next().await? else {
                    break;
                };
                pending_hashes.remove(&resolved_hash);
                classifications.insert(resolved_hash, classification);
                drain_ready_publication_decisions(config, &mut pending_decisions, classifications)?;
            }
        }
    }
    while let Some((resolved_hash, classification)) = in_flight.try_next().await? {
        pending_hashes.remove(&resolved_hash);
        classifications.insert(resolved_hash, classification);
        drain_ready_publication_decisions(config, &mut pending_decisions, classifications)?;
    }
    drain_ready_publication_decisions(config, &mut pending_decisions, classifications)?;
    if !pending_decisions.is_empty() {
        bail!(
            "normalized artifact {} retained {} unresolved publication decisions",
            config.csv_path.display(),
            pending_decisions.len()
        );
    }
    if rows != expected_rows {
        bail!(
            "normalized artifact {} changed during preflight: expected {expected_rows} rows, parsed {rows}",
            config.csv_path.display()
        );
    }
    Ok(())
}

/// Resolve one parent while deferring the predecessor query until Core proves
/// the candidate itself absent.
async fn resolve_candidate<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    candidate: ImportCandidate,
    parent_hash: Vec<u8>,
) -> Result<(Vec<u8>, ParentClassification)> {
    let prev_hash = candidate
        .evidence
        .btc_parent_header
        .prev_blockhash
        .to_byte_array()
        .to_vec();
    let classification = classifier
        .classify_parent_deferred_strict(
            &candidate.evidence.btc_parent_header,
            mmm_read_model::load_parent_preflight(client, &prev_hash),
        )
        .await
        .with_context(|| {
            format!(
                "preclassify historical parent {}",
                candidate.btc_parent_display_hash
            )
        })?;
    Ok((parent_hash, classification))
}

/// Apply the ready prefix of manifest decisions without overtaking an earlier
/// unresolved row.
fn drain_ready_publication_decisions(
    config: &HistoricalImportConfig,
    pending: &mut VecDeque<PendingDecision>,
    classifications: &HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    while let Some(row) = pending.front() {
        let Some(classification) = classifications.get(&row.parent_hash) else {
            break;
        };
        let decision = classified_import_decision(
            row.source_classification,
            row.relevance_selection,
            row.orphan_verdict,
            classification.clone(),
        );
        if let ImportDecision::Skip(reason) = decision {
            bail!(
                "published artifact {} row {} would be skipped as {}",
                config.csv_path.display(),
                row.row_number,
                reason.as_str()
            );
        }
        pending.pop_front();
    }
    Ok(())
}

/// Decide a candidate's fate where live Core classification meets the dataset's
/// own labels. Without a classifier, non-unknown rows capture unclassified and
/// unknown rows are skipped. With a classifier, only matching or independently
/// attested rows reach the preclassified capture path.
pub(super) async fn import_decision<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    candidate: &ImportCandidate,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<ImportDecision> {
    if !classifier.is_enabled() {
        if config.allow_unclassified
            && candidate.source_classification != SourceClassification::Unknown
        {
            return Ok(ImportDecision::CaptureUnclassified);
        }
        return Ok(ImportDecision::Skip(SkipReason::Unclassified));
    }
    let parent_hash = candidate
        .evidence
        .btc_parent_header
        .block_hash()
        .to_byte_array()
        .to_vec();
    let prev_hash = candidate
        .evidence
        .btc_parent_header
        .prev_blockhash
        .to_byte_array()
        .to_vec();
    let classification = if let Some(classification) = classifications.get(&parent_hash) {
        classification.clone()
    } else {
        let preflight = mmm_read_model::load_parent_preflight(client, &prev_hash).await?;
        let classification = classifier
            .classify_parent_strict(&candidate.evidence.btc_parent_header, preflight)
            .await
            .with_context(|| {
                format!(
                    "preclassify historical parent {}",
                    candidate.btc_parent_display_hash
                )
            })?;
        classifications.insert(parent_hash, classification.clone());
        classification
    };
    Ok(classified_import_decision(
        candidate.source_classification,
        candidate.relevance_selection,
        candidate.orphan_verdict,
        classification,
    ))
}

fn classified_import_decision(
    source_classification: SourceClassification,
    relevance_selection: Option<RelevanceSelection>,
    orphan_verdict: Option<BtcOrphanVerdict>,
    classification: ParentClassification,
) -> ImportDecision {
    match classification.kind {
        ParentKind::Canonical if source_classification == SourceClassification::Canonical => {
            ImportDecision::CapturePreclassified(Box::new(classification))
        }
        ParentKind::Stale
            if matches!(
                source_classification,
                SourceClassification::Stale | SourceClassification::StaleDescendant
            ) || matches!(
                relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) =>
        {
            ImportDecision::CapturePreclassified(Box::new(classification))
        }
        ParentKind::Canonical | ParentKind::Stale => {
            ImportDecision::Skip(SkipReason::ClassificationMismatch)
        }
        ParentKind::Near => ImportDecision::Skip(SkipReason::Unclassified),
        ParentKind::Unknown => {
            if matches!(
                relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) {
                let mut known_branch = classification;
                known_branch.core_absence_attested = false;
                ImportDecision::CapturePreclassified(Box::new(known_branch))
            } else if !classification.core_absence_attested
                || source_classification != SourceClassification::Unknown
            {
                ImportDecision::Skip(SkipReason::Unclassified)
            } else if matches!(
                orphan_verdict,
                Some(BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak)
            ) {
                ImportDecision::CapturePreclassified(Box::new(classification))
            } else {
                ImportDecision::Skip(SkipReason::Unclassified)
            }
        }
    }
}
