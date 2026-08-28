//! Skip unchanged publication artifacts and optionally seed empty receipt tables.

use anyhow::{Context, Result, ensure};
use mmm_store::{
    HistoricalImportArtifact, count_historical_import_artifacts, load_historical_import_artifacts,
    seed_historical_import_artifacts, upsert_historical_import_artifact,
};
use serde::Deserialize;
use tokio_postgres::{Client, GenericClient};

use super::super::config::PINNED_RESEARCH_COMMIT;
use super::super::publication::{ArtifactRole, PublicationArtifact};

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
    event_identities: &[PublicationArtifact],
    error_identity: Option<&PublicationArtifact>,
    receipts: &[HistoricalImportArtifact],
) -> ImportPlan {
    let mut plan = ImportPlan {
        work_chain: Vec::with_capacity(event_identities.len()),
        work_error_observations: error_identity.is_some(),
        skipped_unchanged: 0,
    };
    for artifact in event_identities {
        let skip = sha_is_unchanged(
            receipts,
            artifact.role,
            &artifact.chain,
            &artifact.sha256,
        );
        plan.work_chain.push(!skip);
        if skip {
            plan.skipped_unchanged += 1;
        }
    }
    if let Some(artifact) = error_identity
        && sha_is_unchanged(
            receipts,
            artifact.role,
            &artifact.chain,
            &artifact.sha256,
        )
    {
        plan.work_error_observations = false;
        plan.skipped_unchanged += 1;
    }
    plan
}

pub(super) async fn load_or_seed_receipts<C: GenericClient>(
    client: &C,
    seed_imported_receipts: bool,
) -> Result<Vec<HistoricalImportArtifact>> {
    let existing = count_historical_import_artifacts(client).await?;
    if should_seed_empty_receipts(seed_imported_receipts, existing) {
        let seeded = seed_historical_import_artifacts(client, &packaged_seed_artifacts()?).await?;
        tracing::info!(
            seeded,
            "seeded historical import receipts from last imported pin"
        );
    }
    load_historical_import_artifacts(client).await
}

fn should_seed_empty_receipts(requested: bool, existing: i64) -> bool {
    requested && existing == 0
}

pub(super) async fn record_receipt(
    client: &Client,
    artifact: &PublicationArtifact,
) -> Result<()> {
    upsert_historical_import_artifact(
        client,
        &receipt_from_artifact(artifact, PINNED_RESEARCH_COMMIT.as_str()),
    )
    .await
}

pub(super) async fn record_receipts(
    client: &Client,
    artifacts: &[PublicationArtifact],
) -> Result<()> {
    for artifact in artifacts {
        record_receipt(client, artifact).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_receipts_seed_only_when_explicitly_requested() {
        assert!(!should_seed_empty_receipts(false, 0));
        assert!(should_seed_empty_receipts(true, 0));
        assert!(!should_seed_empty_receipts(true, 3));
        assert!(!should_seed_empty_receipts(false, 3));
    }

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

    fn test_identity(chain: &str, sha256: &str, role: ArtifactRole) -> PublicationArtifact {
        PublicationArtifact {
            chain: chain.to_owned(),
            csv_path: String::new(),
            role,
            row_count: 1,
            size_bytes: 1,
            sha256: sha256.to_owned(),
            counts: Default::default(),
            source_chain_counts: Default::default(),
        }
    }

    #[test]
    fn plan_uses_preflight_identity_sha() {
        let receipts = vec![HistoricalImportArtifact {
            role: "event".into(),
            chain: "devcoin".into(),
            sha256: "ab".repeat(32),
            size_bytes: 1,
            row_count: 1,
            source_repo_commit: "0".repeat(40),
        }];
        let unchanged = [test_identity(
            "devcoin",
            &"ab".repeat(32),
            ArtifactRole::Event,
        )];
        let skipped = plan_publication_import(&unchanged, None, &receipts);
        assert_eq!(skipped.work_chain, vec![false]);
        assert_eq!(skipped.skipped_unchanged, 1);

        let changed = [test_identity(
            "devcoin",
            &"cd".repeat(32),
            ArtifactRole::Event,
        )];
        let work = plan_publication_import(&changed, None, &receipts);
        assert_eq!(work.work_chain, vec![true]);
        assert_eq!(work.skipped_unchanged, 0);
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
