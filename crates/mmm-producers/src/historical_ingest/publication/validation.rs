//! Manifest artifact-set validation.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};

use super::{ArtifactRole, PublicationArtifact, PublicationManifest, valid_sha256};
use crate::historical_ingest::config::is_historical_import_chain;

#[derive(Default)]
struct ArtifactInventory {
    event_chains: BTreeSet<String>,
    aggregate_chains: BTreeSet<String>,
    error_observation_rows: Option<u64>,
    event_rows: u64,
    aggregate_rows: u64,
}

pub(super) fn validate_artifact_set(
    manifest: &PublicationManifest,
    registry_chains: &BTreeSet<String>,
) -> Result<()> {
    let mut inventory = ArtifactInventory::default();
    for artifact in &manifest.artifacts {
        ensure!(
            valid_sha256(&artifact.sha256),
            "artifact {} has invalid sha256",
            artifact.chain
        );
        match artifact.role {
            ArtifactRole::Event => {
                ensure!(
                    artifact.counts.ordinary_total() == artifact.row_count
                        && artifact.counts.error_block == 0,
                    "event artifact {} row_count does not equal normal classification counts",
                    artifact.chain
                );
                ensure!(
                    registry_chains.contains(&artifact.chain),
                    "event artifact for unknown or non-importable chain {:?}",
                    artifact.chain
                );
                ensure!(
                    artifact.parent_only_rows <= artifact.counts.canonical,
                    "event artifact {} parent-only rows exceed canonical rows",
                    artifact.chain
                );
                ensure!(
                    inventory.event_chains.insert(artifact.chain.clone()),
                    "duplicate event artifact for {:?}",
                    artifact.chain
                );
                inventory.event_rows += artifact.row_count;
            }
            ArtifactRole::Aggregate => {
                ensure!(
                    artifact.parent_only_rows == 0,
                    "aggregate artifact {} cannot declare parent-only rows",
                    artifact.chain
                );
                ensure!(
                    artifact.counts.ordinary_total() == artifact.row_count
                        && artifact.counts.error_block == 0,
                    "aggregate artifact {} row_count does not equal normal classification counts",
                    artifact.chain
                );
                ensure!(
                    artifact.chain == "stale-descendants",
                    "unknown aggregate artifact {:?}",
                    artifact.chain
                );
                ensure!(
                    inventory.aggregate_chains.insert(artifact.chain.clone()),
                    "duplicate aggregate artifact for {:?}",
                    artifact.chain
                );
                inventory.aggregate_rows += artifact.row_count;
            }
            ArtifactRole::ErrorObservation => {
                ensure!(
                    artifact.parent_only_rows == 0,
                    "error-observation artifact cannot declare parent-only rows"
                );
                ensure!(
                    artifact.chain == "error-block-observations",
                    "unknown error-observation artifact {:?}",
                    artifact.chain
                );
                ensure!(
                    artifact.counts.ordinary_total() == 0
                        && artifact.counts.error_block == artifact.row_count,
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
                    inventory
                        .error_observation_rows
                        .replace(artifact.row_count)
                        .is_none(),
                    "duplicate error-observation artifact"
                );
            }
        }
    }
    validate_inventory(manifest, registry_chains, inventory)
}

pub(super) fn validate_parent_only_rows(
    expected: &PublicationArtifact,
    actual: u64,
    path: &Path,
) -> Result<()> {
    ensure!(
        actual == expected.parent_only_rows,
        "artifact parent-only row-count mismatch for {}: expected {}, got {}",
        path.display(),
        expected.parent_only_rows,
        actual
    );
    Ok(())
}

fn validate_inventory(
    manifest: &PublicationManifest,
    registry_chains: &BTreeSet<String>,
    inventory: ArtifactInventory,
) -> Result<()> {
    ensure!(
        inventory.event_chains == *registry_chains,
        "event artifact set does not match the source registry"
    );
    ensure!(
        inventory.aggregate_chains == BTreeSet::from(["stale-descendants".to_owned()]),
        "stale-descendants aggregate artifact is required"
    );
    // The pinned Research commit plus per-artifact digests define the exact
    // publication. These totals verify that the manifest agrees with itself
    // without duplicating release-specific values in executable code.
    ensure!(
        inventory.event_rows == manifest.total_event_rows,
        "total_event_rows does not equal event artifact rows"
    );
    ensure!(
        inventory.aggregate_rows == manifest.aggregate_rows,
        "aggregate_rows does not equal aggregate artifact rows"
    );
    let error_observation_rows = inventory
        .error_observation_rows
        .context("error-observation aggregate artifact is required")?;
    ensure!(
        manifest.error_observation_rows == error_observation_rows,
        "error_observation_rows does not equal the error-observation artifact"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::historical_ingest::config::historical_chain_spec;
    use crate::historical_ingest::publication::test_support::{
        temp_path, valid_manifest, write_identity_free_canonical_event,
    };
    use bitcoin::hashes::{Hash as _, sha256};

    #[test]
    fn parent_only_count_must_match_manifest() {
        let manifest = valid_manifest();
        let artifact = manifest.event_artifacts().next().expect("event artifact");
        assert!(validate_parent_only_rows(artifact, 0, Path::new("fixture.csv")).is_ok());
        let error = validate_parent_only_rows(artifact, 1, Path::new("fixture.csv"))
            .expect_err("unexpected parent-only row must fail");
        assert!(error.to_string().contains("expected 0, got 1"));
    }

    #[test]
    fn parent_only_manifest_counts_are_role_bounded() {
        for (role, expected_message) in [
            (
                ArtifactRole::Event,
                "parent-only rows exceed canonical rows",
            ),
            (ArtifactRole::Aggregate, "cannot declare parent-only rows"),
            (
                ArtifactRole::ErrorObservation,
                "cannot declare parent-only rows",
            ),
        ] {
            let mut manifest = valid_manifest();
            let artifact = manifest
                .artifacts
                .iter_mut()
                .find(|artifact| artifact.role == role)
                .expect("artifact role");
            artifact.parent_only_rows = artifact.counts.canonical + 1;
            let error = super::super::validate_manifest(&manifest).expect_err("invalid count");
            assert!(error.to_string().contains(expected_message));
        }
    }

    #[test]
    fn artifact_scan_rejects_parent_only_count_mismatch() {
        let root = temp_path("parent-only-mismatch");
        std::fs::create_dir_all(root.join("results/monitor-evidence"))
            .expect("create fixture directory");
        let mut manifest = valid_manifest();
        write_identity_free_canonical_event(&root, &mut manifest, "devcoin");
        let mut expected = manifest
            .event_artifact("devcoin")
            .expect("artifact")
            .clone();
        expected.parent_only_rows = 0;
        let path = root.join(&expected.csv_path);
        let spec = historical_chain_spec("devcoin").expect("devcoin spec");
        let error = super::super::inspect_csv(&path, spec, Some(&expected))
            .expect_err("mismatched parent-only count must fail");
        assert!(error.to_string().contains("parent-only row-count mismatch"));

        let original = std::fs::read_to_string(&path).expect("read fixture");
        let mut lines = original.lines();
        let header = lines.next().expect("fixture header");
        let mut fields = lines
            .next()
            .expect("fixture row")
            .split(',')
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let column = |name: &str| {
            super::super::NORMALIZED_COLUMNS
                .iter()
                .position(|column| *column == name)
                .unwrap_or_else(|| panic!("missing {name} column"))
        };
        fields[column("classification")] = "stale".to_owned();
        fields[column("relevance_reason")] = "valid_direct_stale".to_owned();
        let csv = format!("{header}\n{}\n", fields.join(","));
        std::fs::write(&path, &csv).expect("write non-canonical fixture");
        expected.size_bytes = csv.len() as u64;
        expected.sha256 = sha256::Hash::hash(csv.as_bytes()).to_string();
        expected.counts.canonical = 0;
        expected.counts.stale = 1;
        let error = super::super::inspect_csv(&path, spec, Some(&expected))
            .expect_err("identity-free non-canonical row must fail");
        std::fs::remove_dir_all(&root).expect("remove fixture directory");
        assert!(
            error.to_string().contains("empty_field"),
            "unexpected error: {error:#}"
        );
    }
}
