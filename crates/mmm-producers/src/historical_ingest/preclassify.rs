//! Bounded Bitcoin-parent preclassification for historical publication rows.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::Hash as _;
use futures::{TryStreamExt, stream::FuturesUnordered};
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::btc_orphan::BtcOrphanVerdict;
use mmm_capture::capture::ParentKind;
use mmm_capture::nbits_table::NbitsTable;
use tokio_postgres::GenericClient;

use super::config::{HistoricalChainSpec, HistoricalImportConfig};
use super::csv_source::{
    ImportCandidate, RelevanceSelection, SkipReason, SourceClassification, candidate_from_record,
};
use super::publication::ArtifactPreflight;

/// The per-row verdict, deciding which capture path (if any) a candidate takes.
pub(super) enum ImportDecision {
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
    nbits_table: &NbitsTable,
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
        let candidate = match candidate_from_record(
            spec,
            &layout,
            &record,
            config.publication_ref(),
            Some(nbits_table),
        ) {
            Ok(candidate) => candidate,
            Err(SkipReason::MissingChildIdentity) => continue,
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
        if preflight_skips_catalogued_error_block(config, &candidate, offset + 2)? {
            continue;
        }
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
/// own labels. Historical imports require a Core classifier, so only matching
/// or independently attested rows reach the preclassified capture path.
pub(super) async fn import_decision<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    candidate: &ImportCandidate,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<ImportDecision> {
    if let Some(reason) = catalogued_error_block_skip_reason(candidate) {
        return Ok(ImportDecision::Skip(reason));
    }
    if !classifier.is_enabled() {
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

/// Decide one witness from the dedicated error-observation aggregate. The
/// aggregate is not a valid-evidence override: it must name a catalogue entry,
/// retain the catalogue's exact rejection reason, and reconcile to that entry
/// through the shared Core-plus-catalogue resolver.
pub(super) async fn import_error_observation_decision<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    candidate: &ImportCandidate,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<ImportDecision> {
    if candidate.source_classification != SourceClassification::ErrorBlock {
        return Ok(ImportDecision::Skip(SkipReason::TaxonomyMismatch));
    }
    if !classifier.is_enabled() {
        return Ok(ImportDecision::Skip(SkipReason::Unclassified));
    }
    let parent_hash = candidate
        .evidence
        .btc_parent_header
        .block_hash()
        .to_byte_array()
        .to_vec();
    let Some(catalogue) = mmm_capture::error_blocks::lookup(&parent_hash) else {
        return Ok(ImportDecision::Skip(SkipReason::UnsupportedClassification));
    };
    if candidate.historical_provenance.btc_height != Some(catalogue.height)
        || candidate.error_rejection_reason.as_deref() != Some(catalogue.rejection_reason)
    {
        return Ok(ImportDecision::Skip(SkipReason::EvidenceMismatch));
    }
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
        let live = classifier
            .classify_parent_strict(&candidate.evidence.btc_parent_header, preflight)
            .await
            .with_context(|| {
                format!(
                    "preclassify historical error parent {}",
                    candidate.btc_parent_display_hash
                )
            })?;
        let classification = mmm_read_model::resolve_parent_classification(
            &candidate.evidence.btc_parent_header,
            Some(live),
        )?;
        classifications.insert(parent_hash, classification.clone());
        classification
    };
    if classification.kind != ParentKind::ErrorBlock
        || classification.height != Some(catalogue.height)
        || classification.rejection_reason.as_deref() != Some(catalogue.rejection_reason)
    {
        return Ok(ImportDecision::Skip(SkipReason::ClassificationMismatch));
    }
    Ok(ImportDecision::CapturePreclassified(Box::new(
        classification,
    )))
}

/// Keep catalogued consensus-invalid parents out of the historical valid-evidence
/// import path before a Core classifier decides how to handle the row. Live
/// capture still records these as `error_block`.
fn catalogued_error_block_skip_reason(candidate: &ImportCandidate) -> Option<SkipReason> {
    let parent_hash = candidate
        .evidence
        .btc_parent_header
        .block_hash()
        .to_byte_array();
    mmm_capture::error_blocks::lookup(&parent_hash).map(|_| SkipReason::UnsupportedClassification)
}

fn preflight_skips_catalogued_error_block(
    config: &HistoricalImportConfig,
    candidate: &ImportCandidate,
    row_number: usize,
) -> Result<bool> {
    let Some(reason) = catalogued_error_block_skip_reason(candidate) else {
        return Ok(false);
    };
    if config.manifest_path.is_some() {
        bail!(
            "published artifact {} row {} would be skipped as {}",
            config.csv_path.display(),
            row_number,
            reason.as_str()
        );
    }
    Ok(true)
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
        // A historical source can preserve a parent that was canonical when
        // observed but is now in Core's stale index. Core's direct stale
        // attestation is stronger than the publication snapshot, so retain the
        // source provenance while writing the current stale classification.
        ParentKind::Stale
            if source_classification == SourceClassification::Canonical
                && classification.core_attested =>
        {
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
        // The normalized publication deliberately contains only final valid
        // evidence. A catalogue match in an override is not a stale/orphan
        // row, so refuse it rather than importing invalid work through this
        // valid-evidence path.
        ParentKind::ErrorBlock => ImportDecision::Skip(SkipReason::UnsupportedClassification),
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

#[cfg(test)]
mod tests {
    use super::*;
    use mmm_bitcoin_core::HeightSource;

    fn stale_classification(core_attested: bool) -> ParentClassification {
        ParentClassification {
            kind: ParentKind::Stale,
            height: Some(874_036),
            height_source: Some(HeightSource::BitcoinCore),
            prev_hash: vec![1; 32],
            canonical_predecessor_header: None,
            canonical_competitor_header: None,
            canonical_competitor_hash: Some(vec![2; 32]),
            coinbase: None,
            difficulty_epoch_ok: Some(true),
            rejection_reason: None,
            live_observed: true,
            core_attested,
            core_absence_attested: false,
        }
    }

    #[test]
    fn core_attested_stale_overrides_a_canonical_source_snapshot() {
        let decision = classified_import_decision(
            SourceClassification::Canonical,
            None,
            None,
            stale_classification(true),
        );
        assert!(matches!(decision, ImportDecision::CapturePreclassified(_)));
    }

    #[test]
    fn unattested_stale_does_not_override_a_canonical_source_snapshot() {
        let decision = classified_import_decision(
            SourceClassification::Canonical,
            None,
            None,
            stale_classification(false),
        );
        assert!(matches!(
            decision,
            ImportDecision::Skip(SkipReason::ClassificationMismatch)
        ));
    }
}
