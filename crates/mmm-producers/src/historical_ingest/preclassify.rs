//! Bounded Bitcoin-parent preclassification for historical publication rows.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use bitcoin::hashes::Hash as _;
use futures::{StreamExt, TryStreamExt, stream};
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use tokio_postgres::GenericClient;

use super::csv_source::ImportCandidate;

pub(super) struct PreflightCandidate {
    pub(super) row_number: usize,
    pub(super) candidate: ImportCandidate,
}

/// Resolve one bounded group of unique parents concurrently. The Core RPC
/// client supplies the bound, so the importer fills its existing request
/// capacity without creating a second concurrency setting. Repeated parents
/// still share one classification across the complete publication.
pub(super) async fn resolve_candidate_batch<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    batch: &[PreflightCandidate],
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    if !classifier.is_enabled() {
        return Ok(());
    }

    let mut pending_hashes = HashSet::new();
    let mut pending = Vec::new();
    for (index, row) in batch.iter().enumerate() {
        let parent_hash = row
            .candidate
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
            let candidate = &batch[index].candidate;
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
