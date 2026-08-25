//! Manifest artifact-set validation.

use std::collections::BTreeSet;

use anyhow::{Result, ensure};

use super::{ArtifactRole, PublicationManifest, valid_sha256};
use crate::historical_ingest::config::is_historical_import_chain;

pub(super) struct ArtifactSetValidation {
    pub(super) event_chains: BTreeSet<String>,
    pub(super) aggregate_chains: BTreeSet<String>,
    pub(super) error_observation: Option<u64>,
    pub(super) event_rows: u64,
    pub(super) aggregate_rows: u64,
}

pub(super) fn validate_artifact_set(
    manifest: &PublicationManifest,
    registry_chains: &BTreeSet<String>,
) -> Result<ArtifactSetValidation> {
    let mut event_chains = BTreeSet::new();
    let mut aggregate_chains = BTreeSet::new();
    let mut event_rows = 0_u64;
    let mut aggregate_rows = 0_u64;
    let mut error_observation = None;
    for artifact in &manifest.artifacts {
        ensure!(
            valid_sha256(&artifact.sha256),
            "artifact {} has invalid sha256",
            artifact.chain
        );
        match artifact.role {
            ArtifactRole::Event => {
                ensure!(
                    artifact.counts.total() == artifact.row_count
                        && artifact.counts.error_total() == 0,
                    "event artifact {} row_count does not equal normal classification counts",
                    artifact.chain
                );
                ensure!(
                    registry_chains.contains(&artifact.chain),
                    "event artifact for unknown or non-importable chain {:?}",
                    artifact.chain
                );
                ensure!(
                    event_chains.insert(artifact.chain.clone()),
                    "duplicate event artifact for {:?}",
                    artifact.chain
                );
                event_rows += artifact.row_count;
            }
            ArtifactRole::Aggregate => {
                ensure!(
                    artifact.counts.total() == artifact.row_count
                        && artifact.counts.error_total() == 0,
                    "aggregate artifact {} row_count does not equal normal classification counts",
                    artifact.chain
                );
                ensure!(
                    artifact.chain == "stale-descendants",
                    "unknown aggregate artifact {:?}",
                    artifact.chain
                );
                ensure!(
                    aggregate_chains.insert(artifact.chain.clone()),
                    "duplicate aggregate artifact for {:?}",
                    artifact.chain
                );
                aggregate_rows += artifact.row_count;
            }
            ArtifactRole::ErrorObservation => {
                ensure!(
                    artifact.chain == "error-block-observations",
                    "unknown error-observation artifact {:?}",
                    artifact.chain
                );
                ensure!(
                    artifact.counts.total() == 0
                        && artifact.counts.error_total() == artifact.row_count,
                    "error-observation artifact row_count does not equal error_block count"
                );
                ensure!(
                    !artifact.source_chain_counts.is_empty(),
                    "error-observation artifact requires source-chain counts"
                );
                for chain in artifact.source_chain_counts.keys() {
                    ensure!(
                        is_historical_import_chain(chain),
                        "error-observation artifact includes unknown or surveyed chain {chain:?}"
                    );
                }
                ensure!(
                    artifact.source_chain_counts.values().sum::<u64>() == artifact.row_count,
                    "error-observation source-chain counts do not equal row_count"
                );
                ensure!(
                    error_observation.replace(artifact.row_count).is_none(),
                    "duplicate error-observation artifact"
                );
            }
        }
    }
    Ok(ArtifactSetValidation {
        event_chains,
        aggregate_chains,
        error_observation,
        event_rows,
        aggregate_rows,
    })
}
