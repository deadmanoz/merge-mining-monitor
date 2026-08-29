//! Full-publication aggregate artifact preflight.

use anyhow::{Result, ensure};

use super::{
    ErrorObservationPreflight, PublicationArtifact, inspect_error_observation_csv, inspect_rows,
    load_publication_manifest, verify_checkout_pin, verify_research_manifest,
};
use crate::historical_ingest::config::HistoricalImportAllConfig;

pub(crate) fn preflight_required_aggregate_artifacts(
    config: &HistoricalImportAllConfig,
) -> Result<Option<ErrorObservationPreflight>> {
    let manifest = load_publication_manifest(&config.manifest_path)?;
    if config.require_pinned_checkout {
        verify_checkout_pin(&config.artifact_root)?;
    }
    verify_research_manifest(&config.artifact_root, &manifest)?;
    for artifact in manifest.aggregate_artifacts() {
        inspect_aggregate_csv(&config.artifact_root.join(&artifact.csv_path), artifact)?;
    }
    manifest
        .error_observation_artifact()
        .map(|artifact| {
            inspect_error_observation_csv(
                &config.artifact_root.join(&artifact.csv_path),
                Some(artifact),
            )
        })
        .transpose()
}

fn inspect_aggregate_csv(path: &std::path::Path, expected: &PublicationArtifact) -> Result<()> {
    let mut file = super::open_artifact_file(path, Some(expected))?;
    let mut reader = csv::Reader::from_reader(&mut file);
    let (row_count, counts, _) = inspect_rows(&mut reader, path, &expected.chain, None, true)?;
    ensure!(
        row_count == expected.row_count,
        "artifact row-count mismatch for {}: expected {}, got {}",
        path.display(),
        expected.row_count,
        row_count
    );
    ensure!(
        counts == expected.counts,
        "artifact classification-count mismatch for {}: expected {:?}, got {:?}",
        path.display(),
        expected.counts,
        counts
    );
    Ok(())
}
