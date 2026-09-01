use std::collections::BTreeMap;
use std::path::Path;

use bitcoin::hashes::{Hash as _, sha256};
use mmm_capture::source_registry::SourceLifecycle;

use super::{
    ArtifactRole, NORMALIZED_COLUMNS, PINNED_RESEARCH_COMMIT, PreparedPublication,
    PublicationArtifact, PublicationCounts, PublicationManifest, RSK_SIDECAR_COLUMNS,
    importable_chains,
};

const TEST_EVENT_ROWS: u64 = 3;
const TEST_AGGREGATE_ROWS: u64 = 2;
pub(super) const TEST_ERROR_OBSERVATION_ROWS: u64 = 2;

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
                TEST_EVENT_ROWS
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
                parent_only_rows: 0,
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
        row_count: TEST_AGGREGATE_ROWS,
        parent_only_rows: 0,
        size_bytes: 1,
        sha256: "0".repeat(64),
        counts: PublicationCounts {
            stale_descendant: TEST_AGGREGATE_ROWS,
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
        total_event_rows: TEST_EVENT_ROWS,
        aggregate_rows: TEST_AGGREGATE_ROWS,
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
    manifest.error_observation_rows = TEST_ERROR_OBSERVATION_ROWS;
    manifest.artifacts.push(PublicationArtifact {
        chain: "error-block-observations".to_owned(),
        csv_path: "results/monitor-evidence/error-block-observations_monitor_evidence.csv"
            .to_owned(),
        role: ArtifactRole::ErrorObservation,
        row_count: TEST_ERROR_OBSERVATION_ROWS,
        parent_only_rows: 0,
        size_bytes: 1,
        sha256: "0".repeat(64),
        counts: PublicationCounts {
            error_block: TEST_ERROR_OBSERVATION_ROWS,
            ..PublicationCounts::default()
        },
        source_chain_counts: BTreeMap::from([
            ("devcoin".to_owned(), 1),
            ("namecoin".to_owned(), 1),
        ]),
    });
}

pub(super) fn write_empty_event_artifacts(root: &Path, manifest: &mut PublicationManifest) {
    manifest.total_event_rows = 0;
    for artifact in manifest
        .artifacts
        .iter_mut()
        .filter(|artifact| artifact.role == ArtifactRole::Event)
    {
        artifact.row_count = 0;
        artifact.counts = PublicationCounts::default();
        let mut columns = NORMALIZED_COLUMNS.to_vec();
        if artifact.chain == "rsk" {
            columns.extend_from_slice(RSK_SIDECAR_COLUMNS);
        }
        let csv = format!("{}\n", columns.join(","));
        std::fs::write(root.join(&artifact.csv_path), &csv).expect("write event fixture");
        artifact.size_bytes = csv.len() as u64;
        artifact.sha256 = sha256::Hash::hash(csv.as_bytes()).to_string();
    }
}

pub(super) fn write_identity_free_canonical_event(
    root: &Path,
    manifest: &mut PublicationManifest,
    chain: &str,
) {
    let column = |name: &str| {
        NORMALIZED_COLUMNS
            .iter()
            .position(|column| *column == name)
            .unwrap_or_else(|| panic!("missing {name} column"))
    };
    let mut row = vec![String::new(); NORMALIZED_COLUMNS.len()];
    row[column("chain")] = chain.to_owned();
    row[column("source_kind")] = "full_inventory".to_owned();
    row[column("source_path")] = "fixture/canonical.csv".to_owned();
    row[column("source_row_number")] = "1".to_owned();
    row[column("artifact_scope")] = "full_classifier_inventory".to_owned();
    row[column("provenance")] = "fixture".to_owned();
    row[column("btc_height")] = "0".to_owned();
    row[column("btc_header_hash")] =
        "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f".to_owned();
    row[column("btc_prev_hash")] = "0".repeat(64);
    row[column("btc_header_hex")] =
        "0100000000000000000000000000000000000000000000000000000000000000\
        000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4\
        b1e5e4a29ab5f49ffff001d1dac2b7c"
            .replace(char::is_whitespace, "");
    row[column("classification")] = "canonical".to_owned();
    let csv = format!("{}\n{}\n", NORMALIZED_COLUMNS.join(","), row.join(","));

    manifest.total_event_rows = 1;
    let artifact = manifest
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.chain == chain)
        .unwrap_or_else(|| panic!("missing {chain} event artifact"));
    std::fs::write(root.join(&artifact.csv_path), &csv).expect("write canonical parent fixture");
    artifact.row_count = 1;
    artifact.parent_only_rows = 1;
    artifact.counts.canonical = 1;
    artifact.size_bytes = csv.len() as u64;
    artifact.sha256 = sha256::Hash::hash(csv.as_bytes()).to_string();
}

pub(super) fn assert_parent_only_fixture(prepared: &PreparedPublication) {
    assert_eq!(prepared.configs.len(), importable_chains().len());
    assert_eq!(prepared.event_artifacts.len(), prepared.configs.len());
    assert!(
        prepared
            .configs
            .iter()
            .zip(&prepared.event_artifacts)
            .all(|(config, artifact)| config.chain == artifact.chain)
    );
    assert_eq!(
        prepared.error_observations.row_count,
        TEST_ERROR_OBSERVATION_ROWS
    );
    assert!(
        prepared
            .event_artifacts
            .iter()
            .all(|artifact| artifact.state_rows.is_empty())
    );
}
