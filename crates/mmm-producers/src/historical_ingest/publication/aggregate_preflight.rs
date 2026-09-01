//! Full-publication aggregate artifact preflight.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};

use super::{
    ArtifactPreflight, ErrorObservationPreflight, PublicationArtifact, inspect_csv,
    inspect_error_observation_csv, inspect_rows, load_publication_manifest, verify_checkout_pin,
    verify_research_manifest,
};
use crate::historical_ingest::config::{
    HistoricalImportAllConfig, HistoricalImportConfig, historical_chain_spec,
};

/// A complete publication whose provenance and every input artifact have been
/// verified before the import runner can mutate the database.
#[derive(Debug)]
pub(in crate::historical_ingest) struct PreparedPublication {
    pub(in crate::historical_ingest) configs: Vec<HistoricalImportConfig>,
    pub(in crate::historical_ingest) event_artifacts: Vec<ArtifactPreflight>,
    pub(in crate::historical_ingest) error_observations: ErrorObservationPreflight,
}

/// Cross the external publication seam once: load the monitor-owned manifest,
/// verify its pinned Research checkout and source manifest, then inspect every
/// aggregate and event artifact in deterministic chain order.
pub(in crate::historical_ingest) fn preflight_publication(
    config: &HistoricalImportAllConfig,
) -> Result<PreparedPublication> {
    let expected_error_parents = mmm_capture::error_blocks::hashes().collect();
    preflight_publication_against(config, &expected_error_parents)
}

#[cfg(test)]
pub(super) fn preflight_publication_for_test(
    config: &HistoricalImportAllConfig,
    expected_error_parents: &BTreeSet<[u8; 32]>,
) -> Result<PreparedPublication> {
    preflight_publication_against(config, expected_error_parents)
}

fn preflight_publication_against(
    config: &HistoricalImportAllConfig,
    expected_error_parents: &BTreeSet<[u8; 32]>,
) -> Result<PreparedPublication> {
    let manifest = load_publication_manifest(&config.manifest_path)?;
    if config.require_pinned_checkout {
        verify_checkout_pin(&config.artifact_root)?;
    }
    verify_research_manifest(&config.artifact_root, &manifest)?;
    for artifact in manifest.aggregate_artifacts() {
        inspect_aggregate_csv(&config.artifact_root.join(&artifact.csv_path), artifact)?;
    }
    let error_artifact = manifest
        .error_observation_artifact()
        .context("validated publication requires error observations")?;
    let error_observations = inspect_error_observation_csv(
        &config.artifact_root.join(&error_artifact.csv_path),
        Some(error_artifact),
    )?;
    let observed_error_parents = error_observations
        .state_rows
        .iter()
        .map(|row| row.btc_parent_header_hash)
        .collect::<BTreeSet<_>>();
    ensure!(
        observed_error_parents == *expected_error_parents,
        "error-observation artifact does not cover the pinned error-block catalogue"
    );

    let mut expected_events = manifest.event_artifacts().collect::<Vec<_>>();
    expected_events.sort_by_key(|artifact| artifact.chain.as_str());
    let mut configs = Vec::with_capacity(expected_events.len());
    let mut event_artifacts = Vec::with_capacity(expected_events.len());
    for expected in expected_events {
        let spec = historical_chain_spec(&expected.chain)
            .expect("manifest validation guarantees registered event chains");
        let csv_path = config.artifact_root.join(&expected.csv_path);
        event_artifacts.push(inspect_csv(&csv_path, spec, Some(expected))?);
        configs.push(config.event_config(expected));
    }
    Ok(PreparedPublication {
        configs,
        event_artifacts,
        error_observations,
    })
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
