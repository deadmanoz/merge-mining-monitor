//! Skip unchanged publication artifacts and seed empty receipt tables.

use anyhow::{Context, Result, ensure};
use mmm_store::{
    HistoricalImportArtifact, count_historical_import_artifacts, load_historical_import_artifacts,
    seed_historical_import_artifacts, upsert_historical_import_artifact,
};
use serde::Deserialize;
use tokio_postgres::{Client, GenericClient};

use super::super::config::{HistoricalImportConfig, PINNED_RESEARCH_COMMIT};
use super::super::publication::{
    ArtifactRole, PublicationArtifact, PublicationManifest, load_publication_manifest,
};

const IMPORTED_ARTIFACT_SEED: &str =
    include_str!("../../../../../data/historical/imported-artifact-seed.json");

#[derive(Debug, Deserialize)]
struct SeedFile {
    schema_version: u32,
    source_repo: String,
    source_repo_commit: String,
    artifacts: Vec<SeedArtifact>,
}

#[derive(Debug, Deserialize)]
struct SeedArtifact {
    role: String,
    chain: String,
    sha256: String,
    size_bytes: i64,
    row_count: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ImportPlan {
    pub work_chain: Vec<bool>,
    pub work_error_observations: bool,
    pub skipped_unchanged: u64,
}

pub(super) fn packaged_seed_artifacts() -> Result<Vec<HistoricalImportArtifact>> {
    parse_seed(IMPORTED_ARTIFACT_SEED)
}

pub(super) fn parse_seed(json: &str) -> Result<Vec<HistoricalImportArtifact>> {
    let seed: SeedFile = serde_json::from_str(json).context("parse imported-artifact seed")?;
    ensure!(seed.schema_version == 1, "unsupported seed schema_version");
    ensure!(
        seed.source_repo == "merge-mining-research",
        "unexpected seed source_repo"
    );
    ensure!(
        seed.source_repo_commit.len() == 40
            && seed
                .source_repo_commit
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "seed source_repo_commit must be a 40-character hex SHA"
    );
    ensure!(
        !seed.artifacts.is_empty(),
        "imported-artifact seed is empty"
    );
    seed.artifacts
        .into_iter()
        .map(|artifact| {
            ensure!(
                matches!(
                    artifact.role.as_str(),
                    "event" | "error_observation" | "aggregate"
                ),
                "seed role {} is not a publication role",
                artifact.role
            );
            ensure!(
                artifact.sha256.len() == 64
                    && artifact
                        .sha256
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "seed sha256 for {} is not lowercase hex",
                artifact.chain
            );
            Ok(HistoricalImportArtifact {
                role: artifact.role,
                chain: artifact.chain,
                sha256: artifact.sha256,
                size_bytes: artifact.size_bytes,
                row_count: artifact.row_count,
                source_repo_commit: seed.source_repo_commit.clone(),
            })
        })
        .collect()
}

pub(super) fn receipt_from_artifact(
    artifact: &PublicationArtifact,
    source_repo_commit: &str,
) -> HistoricalImportArtifact {
    HistoricalImportArtifact {
        role: artifact.role.as_str().to_owned(),
        chain: artifact.chain.clone(),
        sha256: artifact.sha256.clone(),
        size_bytes: i64::try_from(artifact.size_bytes).expect("artifact size fits i64"),
        row_count: i64::try_from(artifact.row_count).expect("artifact rows fit i64"),
        source_repo_commit: source_repo_commit.to_owned(),
    }
}

pub(super) fn sha_is_unchanged(
    receipts: &[HistoricalImportArtifact],
    role: ArtifactRole,
    chain: &str,
    sha256: &str,
) -> bool {
    receipts.iter().any(|receipt| {
        receipt.role == role.as_str() && receipt.chain == chain && receipt.sha256 == sha256
    })
}

pub(super) fn plan_publication_import(
    configs: &[HistoricalImportConfig],
    has_error_observations: bool,
    manifest: &PublicationManifest,
    receipts: &[HistoricalImportArtifact],
) -> Result<ImportPlan> {
    let mut plan = ImportPlan {
        work_chain: Vec::with_capacity(configs.len()),
        work_error_observations: has_error_observations,
        skipped_unchanged: 0,
    };
    for config in configs {
        let artifact = manifest.event_artifact(&config.chain).with_context(|| {
            format!(
                "publication manifest has no event artifact for {:?}",
                config.chain
            )
        })?;
        let skip = sha_is_unchanged(
            receipts,
            ArtifactRole::Event,
            &artifact.chain,
            &artifact.sha256,
        );
        plan.work_chain.push(!skip);
        if skip {
            plan.skipped_unchanged += 1;
        }
    }
    if has_error_observations {
        let artifact = manifest
            .error_observation_artifact()
            .context("publication manifest has no error-observation artifact")?;
        if sha_is_unchanged(
            receipts,
            ArtifactRole::ErrorObservation,
            &artifact.chain,
            &artifact.sha256,
        ) {
            plan.work_error_observations = false;
            plan.skipped_unchanged += 1;
        }
    }
    Ok(plan)
}

pub(super) async fn load_or_seed_receipts<C: GenericClient>(
    client: &C,
) -> Result<Vec<HistoricalImportArtifact>> {
    if count_historical_import_artifacts(client).await? > 0 {
        return load_historical_import_artifacts(client).await;
    }
    let seeded = seed_historical_import_artifacts(client, &packaged_seed_artifacts()?).await?;
    tracing::info!(
        seeded,
        "seeded historical import receipts from last imported pin"
    );
    load_historical_import_artifacts(client).await
}

pub(super) async fn record_event_receipt(
    client: &Client,
    config: &HistoricalImportConfig,
) -> Result<()> {
    let manifest_path = config
        .manifest_path
        .as_deref()
        .context("publication import requires a manifest")?;
    let manifest = load_publication_manifest(manifest_path)?;
    record_event_receipt_from_manifest(client, &manifest, &config.chain).await
}

pub(super) async fn record_event_receipt_from_manifest(
    client: &Client,
    manifest: &PublicationManifest,
    chain: &str,
) -> Result<()> {
    let artifact = manifest
        .event_artifact(chain)
        .with_context(|| format!("publication manifest has no event artifact for {chain:?}"))?;
    upsert_historical_import_artifact(
        client,
        &receipt_from_artifact(artifact, PINNED_RESEARCH_COMMIT.as_str()),
    )
    .await
}

pub(super) async fn record_error_observation_receipt(
    client: &Client,
    manifest: &PublicationManifest,
) -> Result<()> {
    let artifact = manifest
        .error_observation_artifact()
        .context("publication manifest has no error-observation artifact")?;
    upsert_historical_import_artifact(
        client,
        &receipt_from_artifact(artifact, PINNED_RESEARCH_COMMIT.as_str()),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_seed_has_the_last_imported_error_obs_and_hathor() {
        let artifacts = packaged_seed_artifacts().expect("packaged seed");
        assert_eq!(artifacts.len(), 28);
        assert!(
            artifacts.iter().all(|row| {
                row.source_repo_commit == "091e01a53da8497a986caf03bf142d8d65ac0110"
            })
        );
        let error_obs = artifacts
            .iter()
            .find(|row| row.chain == "error-block-observations")
            .expect("error-obs seed");
        assert_eq!(error_obs.role, "error_observation");
        assert_eq!(error_obs.row_count, 73);
        let hathor = artifacts
            .iter()
            .find(|row| row.chain == "hathor")
            .expect("hathor seed");
        assert_eq!(hathor.row_count, 6);
    }

    #[test]
    fn matching_receipt_skips_and_changed_sha_does_not() {
        let receipts = vec![HistoricalImportArtifact {
            role: "event".into(),
            chain: "devcoin".into(),
            sha256: "ab".repeat(32),
            size_bytes: 1,
            row_count: 1,
            source_repo_commit: "0".repeat(40),
        }];
        assert!(sha_is_unchanged(
            &receipts,
            ArtifactRole::Event,
            "devcoin",
            &"ab".repeat(32)
        ));
        assert!(!sha_is_unchanged(
            &receipts,
            ArtifactRole::Event,
            "devcoin",
            &"cd".repeat(32)
        ));
        assert!(!sha_is_unchanged(
            &receipts,
            ArtifactRole::ErrorObservation,
            "devcoin",
            &"ab".repeat(32)
        ));
    }
}
