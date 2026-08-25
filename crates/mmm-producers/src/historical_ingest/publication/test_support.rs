use std::collections::BTreeMap;

use mmm_capture::source_registry::SourceLifecycle;

use super::{
    ArtifactRole, ERROR_OBSERVATION_ROW_COUNT, NORMALIZED_COLUMNS, PINNED_RESEARCH_COMMIT,
    PublicationArtifact, PublicationCounts, PublicationManifest, importable_chains,
};

pub(super) fn temp_path(label: &str) -> std::path::PathBuf {
    let suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "mmm-publication-{label}-{}-{suffix}",
        std::process::id()
    ))
}

pub(super) fn valid_manifest() -> PublicationManifest {
    let mut assigned_total = false;
    let mut artifacts = importable_chains()
        .iter()
        .map(|spec| {
            let row_count = if !assigned_total && spec.lifecycle != SourceLifecycle::Surveyed {
                assigned_total = true;
                576_662
            } else {
                0
            };
            PublicationArtifact {
                chain: spec.chain.to_owned(),
                csv_path: format!(
                    "results/monitor-evidence/{}_monitor_evidence.csv",
                    spec.chain
                ),
                role: ArtifactRole::Event,
                row_count,
                size_bytes: 1,
                sha256: "0".repeat(64),
                counts: PublicationCounts {
                    canonical: row_count,
                    ..PublicationCounts::default()
                },
                source_chain_counts: BTreeMap::new(),
            }
        })
        .collect::<Vec<_>>();
    artifacts.push(PublicationArtifact {
        chain: "stale-descendants".to_owned(),
        csv_path: "results/stale-descendants.csv".to_owned(),
        role: ArtifactRole::Aggregate,
        row_count: 21,
        size_bytes: 1,
        sha256: "0".repeat(64),
        counts: PublicationCounts {
            stale_descendant: 21,
            ..PublicationCounts::default()
        },
        source_chain_counts: BTreeMap::new(),
    });
    let mut manifest = PublicationManifest {
        schema_version: 2,
        source_repo: "merge-mining-research".to_owned(),
        source_repo_commit: PINNED_RESEARCH_COMMIT.as_str().to_owned(),
        publication_manifest_path: "results/monitor-evidence/manifest.json".to_owned(),
        publication_manifest_sha256: "0".repeat(64),
        total_event_rows: 576_662,
        aggregate_rows: 21,
        error_observation_rows: 0,
        required_columns: NORMALIZED_COLUMNS
            .iter()
            .map(|column| (*column).to_owned())
            .collect(),
        artifacts,
    };
    add_error_observation_artifact(&mut manifest);
    manifest
}

fn add_error_observation_artifact(manifest: &mut PublicationManifest) {
    manifest.error_observation_rows = ERROR_OBSERVATION_ROW_COUNT;
    manifest.artifacts.push(PublicationArtifact {
        chain: "error-block-observations".to_owned(),
        csv_path: "results/monitor-evidence/error-block-observations_monitor_evidence.csv"
            .to_owned(),
        role: ArtifactRole::ErrorObservation,
        row_count: ERROR_OBSERVATION_ROW_COUNT,
        size_bytes: 1,
        sha256: "0".repeat(64),
        counts: PublicationCounts {
            error_block: ERROR_OBSERVATION_ROW_COUNT,
            ..PublicationCounts::default()
        },
        source_chain_counts: BTreeMap::from([
            ("devcoin".to_owned(), 16),
            ("elastos".to_owned(), 1),
            ("emercoin".to_owned(), 1),
            ("groupcoin".to_owned(), 1),
            ("i0coin".to_owned(), 1),
            ("ixcoin".to_owned(), 13),
            ("namecoin".to_owned(), 32),
            ("rsk".to_owned(), 5),
            ("syscoin".to_owned(), 2),
            ("unobtanium".to_owned(), 1),
        ]),
    });
}
