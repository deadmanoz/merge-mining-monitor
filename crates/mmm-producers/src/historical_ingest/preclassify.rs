//! Bounded Bitcoin-parent preclassification for historical publication rows.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::Hash as _;
use futures::{StreamExt, TryStreamExt, stream};
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

/// Validate every normalized candidate and resolve parent classifications before
/// the chain transaction begins.
///
/// Positive Core classifications and Core-absence results are cached by parent
/// hash so repeated observations across a chain do not repeat RPC work. The
/// first pass batches only unresolved unique parents, so cached and repeated
/// rows cannot consume classifier capacity. A second sequential pass applies
/// the publication decision gate in source order. Re-reading the artifact keeps
/// memory bounded without sacrificing concurrency or deterministic validation.
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
    let mut batch = Vec::with_capacity(concurrency);
    let mut pending_hashes = HashSet::with_capacity(concurrency);
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
            if !classifications.contains_key(&parent_hash) && pending_hashes.insert(parent_hash) {
                batch.push(candidate);
                if batch.len() == concurrency {
                    resolve_candidate_batch(client, classifier, &batch, classifications).await?;
                    batch.clear();
                    pending_hashes.clear();
                }
            }
        }
    }
    resolve_candidate_batch(client, classifier, &batch, classifications).await?;
    if rows != expected_rows {
        bail!(
            "normalized artifact {} changed during preflight: expected {expected_rows} rows, parsed {rows}",
            config.csv_path.display()
        );
    }
    drop(reader);

    if classifier.is_enabled() {
        validate_publication_decisions(client, classifier, config, spec, artifact, classifications)
            .await?;
    }
    Ok(())
}

/// Resolve one bounded group of unique parents concurrently. The Core RPC
/// client supplies the bound, so the importer fills its existing request
/// capacity without creating a second concurrency setting. Repeated parents
/// still share one classification across the complete publication.
pub(super) async fn resolve_candidate_batch<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    batch: &[ImportCandidate],
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    if !classifier.is_enabled() {
        return Ok(());
    }

    let mut pending_hashes = HashSet::new();
    let mut pending = Vec::new();
    for (index, candidate) in batch.iter().enumerate() {
        let parent_hash = candidate
            .evidence
            .btc_parent_header
            .block_hash()
            .to_byte_array()
            .to_vec();
        if !classifications.contains_key(&parent_hash) && pending_hashes.insert(parent_hash.clone())
        {
            pending.push((parent_hash, index));
        }
    }

    let resolved = stream::iter(pending)
        .map(|(parent_hash, index)| async move {
            let candidate = &batch[index];
            let prev_hash = candidate
                .evidence
                .btc_parent_header
                .prev_blockhash
                .to_byte_array()
                .to_vec();
            let classification = classifier
                .classify_parent_deferred(
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
            Ok::<_, anyhow::Error>((parent_hash, classification))
        })
        .buffer_unordered(classifier.max_concurrency())
        .try_collect::<Vec<_>>()
        .await?;
    classifications.extend(resolved);
    Ok(())
}

/// Re-read a classified artifact and apply its publication decision gate in
/// source order. All parent results are already cached, so this pass performs
/// no Bitcoin Core or predecessor-preflight I/O.
async fn validate_publication_decisions<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    spec: &HistoricalChainSpec,
    artifact: &mut ArtifactPreflight,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    let expected_rows = artifact.row_count;
    let (mut reader, layout) = artifact.open_reader(spec)?;
    let mut rows = 0_u64;
    for (offset, record) in reader.records().enumerate() {
        let record = record.with_context(|| {
            format!(
                "parse normalized artifact {} row {} during decision validation",
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
        let decision =
            import_decision(client, classifier, config, &candidate, classifications).await?;
        if config.manifest_path.is_some()
            && let ImportDecision::Skip(reason) = decision
        {
            bail!(
                "published artifact {} row {} would be skipped as {}",
                config.csv_path.display(),
                offset + 2,
                reason.as_str()
            );
        }
    }
    if rows != expected_rows {
        bail!(
            "normalized artifact {} changed during decision validation: expected {expected_rows} rows, parsed {rows}",
            config.csv_path.display()
        );
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
            .classify_parent(&candidate.evidence.btc_parent_header, preflight)
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
    match classification.kind {
        ParentKind::Canonical
            if candidate.source_classification == SourceClassification::Canonical =>
        {
            Ok(ImportDecision::CapturePreclassified(Box::new(
                classification,
            )))
        }
        ParentKind::Stale
            if matches!(
                candidate.source_classification,
                SourceClassification::Stale | SourceClassification::StaleDescendant
            ) || matches!(
                candidate.relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) =>
        {
            Ok(ImportDecision::CapturePreclassified(Box::new(
                classification,
            )))
        }
        ParentKind::Canonical | ParentKind::Stale => {
            Ok(ImportDecision::Skip(SkipReason::ClassificationMismatch))
        }
        ParentKind::Near => Ok(ImportDecision::Skip(SkipReason::Unclassified)),
        ParentKind::Unknown => {
            if matches!(
                candidate.relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) {
                let mut known_branch = classification;
                known_branch.core_absence_attested = false;
                Ok(ImportDecision::CapturePreclassified(Box::new(known_branch)))
            } else if !classification.core_absence_attested
                || candidate.source_classification != SourceClassification::Unknown
            {
                Ok(ImportDecision::Skip(SkipReason::Unclassified))
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
