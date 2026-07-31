//! Monitor-owned provenance and artifact preflight for the research publication.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail, ensure};
use bitcoin::hashes::{Hash as _, HashEngine as _, sha256};
use mmm_capture::source_registry::SourceLifecycle;
use serde::Deserialize;

use super::config::{
    HistoricalChainSpec, HistoricalImportConfig, PINNED_RESEARCH_COMMIT, importable_chains,
};
use super::csv_source::{CsvLayout, PublicationCategory, publication_category};

pub(super) const NORMALIZED_COLUMNS: &[&str] = &[
    "chain",
    "source_kind",
    "source_path",
    "source_row_number",
    "artifact_scope",
    "provenance",
    "child_height",
    "child_block_hash",
    "child_header_hex",
    "child_block_time",
    "child_nbits",
    "btc_height",
    "btc_header_hash",
    "btc_prev_hash",
    "btc_time",
    "btc_bits",
    "btc_nonce",
    "btc_header_hex",
    "coinbase_scriptsig_hex",
    "coinbase_outputs",
    "full_coinbase_hex",
    "classification",
    "validation_status",
    "expected_nbits",
    "rejection_reason",
    "btc_stale_relevance",
    "relevance_reason",
];

const RSK_SIDECAR_COLUMNS: &[&str] = &[
    "rsk_miner",
    "merge_mining_hash",
    "is_uncle",
    "uncle_index",
    "uncle_parent_height",
    "rsk_merkle_proof",
    "rsk_coinbase_tail",
];

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(super) struct PublicationManifest {
    schema_version: u32,
    source_repo: String,
    source_repo_commit: String,
    publication_manifest_path: String,
    publication_manifest_sha256: String,
    total_event_rows: u64,
    aggregate_rows: u64,
    required_columns: Vec<String>,
    artifacts: Vec<PublicationArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(super) struct PublicationArtifact {
    pub(super) chain: String,
    pub(super) csv_path: String,
    pub(super) role: ArtifactRole,
    pub(super) row_count: u64,
    pub(super) size_bytes: u64,
    pub(super) sha256: String,
    pub(super) counts: PublicationCounts,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize))]
#[serde(rename_all = "snake_case")]
pub(super) enum ArtifactRole {
    Event,
    Aggregate,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(serde::Serialize))]
pub(super) struct PublicationCounts {
    pub(super) canonical: u64,
    pub(super) stale: u64,
    pub(super) stale_descendant: u64,
    pub(super) strict_btc_orphan: u64,
    pub(super) weak_btc_orphan: u64,
}

impl PublicationCounts {
    fn total(self) -> u64 {
        self.canonical
            + self.stale
            + self.stale_descendant
            + self.strict_btc_orphan
            + self.weak_btc_orphan
    }
}

impl PublicationManifest {
    pub(super) fn event_artifact(&self, chain: &str) -> Option<&PublicationArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.role == ArtifactRole::Event && artifact.chain == chain)
    }

    pub(super) fn event_artifacts(&self) -> impl Iterator<Item = &PublicationArtifact> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.role == ArtifactRole::Event)
    }

    fn aggregate_artifacts(&self) -> impl Iterator<Item = &PublicationArtifact> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.role == ArtifactRole::Aggregate)
    }
}

pub(super) fn load_publication_manifest(path: &Path) -> Result<PublicationManifest> {
    let manifest: PublicationManifest = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    validate_manifest(&manifest).with_context(|| format!("validate {}", path.display()))?;
    Ok(manifest)
}

fn validate_manifest(manifest: &PublicationManifest) -> Result<()> {
    ensure!(manifest.schema_version == 2, "unsupported schema_version");
    ensure!(
        manifest.source_repo == "merge-mining-research",
        "unexpected source_repo"
    );
    ensure!(
        manifest.source_repo_commit == PINNED_RESEARCH_COMMIT,
        "source_repo_commit must be pinned to {PINNED_RESEARCH_COMMIT}"
    );
    ensure!(
        valid_sha256(&manifest.publication_manifest_sha256),
        "invalid publication_manifest_sha256"
    );
    ensure!(
        manifest
            .required_columns
            .iter()
            .map(String::as_str)
            .eq(NORMALIZED_COLUMNS.iter().copied()),
        "required_columns do not match the normalized schema"
    );

    let registry_chains = importable_chains()
        .iter()
        .map(|spec| spec.chain.to_owned())
        .collect::<BTreeSet<_>>();
    let (event_chains, aggregate_chains, event_rows, aggregate_rows) =
        validate_artifact_set(manifest, &registry_chains)?;
    ensure!(
        event_chains == registry_chains,
        "event artifact set does not match the source registry"
    );
    ensure!(
        aggregate_chains == BTreeSet::from(["stale-descendants".to_owned()]),
        "stale-descendants aggregate artifact is required"
    );
    ensure!(
        event_rows == manifest.total_event_rows,
        "total_event_rows does not equal event artifact rows"
    );
    ensure!(
        aggregate_rows == manifest.aggregate_rows,
        "aggregate_rows does not equal aggregate artifact rows"
    );
    ensure!(
        manifest.total_event_rows == 576_662,
        "unexpected pinned publication event total"
    );
    ensure!(
        manifest.aggregate_rows == 20,
        "unexpected pinned publication aggregate total"
    );
    for spec in importable_chains() {
        let artifact = manifest
            .event_artifact(spec.chain)
            .expect("registry equality checked above");
        if spec.lifecycle == SourceLifecycle::Surveyed {
            ensure!(
                artifact.row_count == 0,
                "Surveyed source {} must have a zero-row artifact",
                spec.chain
            );
        }
    }
    Ok(())
}

fn validate_artifact_set(
    manifest: &PublicationManifest,
    registry_chains: &BTreeSet<String>,
) -> Result<(BTreeSet<String>, BTreeSet<String>, u64, u64)> {
    let mut event_chains = BTreeSet::new();
    let mut aggregate_chains = BTreeSet::new();
    let mut event_rows = 0_u64;
    let mut aggregate_rows = 0_u64;
    for artifact in &manifest.artifacts {
        ensure!(
            valid_sha256(&artifact.sha256),
            "artifact {} has invalid sha256",
            artifact.chain
        );
        ensure!(
            artifact.counts.total() == artifact.row_count,
            "artifact {} row_count does not equal classification counts",
            artifact.chain
        );
        match artifact.role {
            ArtifactRole::Event => {
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
        }
    }
    Ok((event_chains, aggregate_chains, event_rows, aggregate_rows))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug)]
pub(super) struct ArtifactPreflight {
    pub(super) row_count: u64,
    pub(super) counts: PublicationCounts,
    file: File,
}

impl ArtifactPreflight {
    pub(super) fn open_reader<'a>(
        &'a mut self,
        spec: &HistoricalChainSpec,
    ) -> Result<(csv::Reader<&'a mut File>, CsvLayout)> {
        self.file
            .seek(SeekFrom::Start(0))
            .context("rewind verified historical artifact")?;
        let mut reader = csv::Reader::from_reader(&mut self.file);
        let layout = CsvLayout::new(
            reader.headers().context("read historical CSV header")?,
            spec,
        )?;
        Ok((reader, layout))
    }
}

/// Verify provenance, bytes, schema, and declared counts before any database
/// mutation. Fixture/operator `--csv` inputs receive the same schema and row
/// checks but have no pinned digest expectation.
pub(super) fn preflight_artifact(
    config: &HistoricalImportConfig,
    spec: &HistoricalChainSpec,
) -> Result<ArtifactPreflight> {
    let expected = if let Some(manifest_path) = config.manifest_path.as_deref() {
        let manifest = load_publication_manifest(manifest_path)?;
        let artifact_root = config
            .artifact_root
            .as_deref()
            .context("publication import requires artifact_root")?;
        if config.require_pinned_checkout {
            verify_checkout_pin(artifact_root)?;
        }
        verify_research_manifest(artifact_root, &manifest)?;
        Some(
            manifest
                .event_artifact(spec.chain)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "publication manifest has no event artifact for {:?}",
                        spec.chain
                    )
                })?,
        )
    } else {
        None
    };
    if let Some(expected) = &expected {
        let expected_path = config
            .artifact_root
            .as_deref()
            .expect("publication root checked above")
            .join(&expected.csv_path);
        ensure!(
            config.csv_path == expected_path,
            "configured CSV {} does not match manifest path {}",
            config.csv_path.display(),
            expected_path.display()
        );
    }
    inspect_csv(&config.csv_path, spec, expected.as_ref())
}

pub(super) fn preflight_required_aggregate_artifacts(
    config: &super::config::HistoricalImportAllConfig,
) -> Result<()> {
    let manifest = load_publication_manifest(&config.manifest_path)?;
    if config.require_pinned_checkout {
        verify_checkout_pin(&config.artifact_root)?;
    }
    verify_research_manifest(&config.artifact_root, &manifest)?;
    for artifact in manifest.aggregate_artifacts() {
        inspect_aggregate_csv(&config.artifact_root.join(&artifact.csv_path), artifact)?;
    }
    Ok(())
}

fn inspect_aggregate_csv(path: &Path, expected: &PublicationArtifact) -> Result<()> {
    let mut file = open_artifact_file(path, Some(expected))?;
    let mut reader = csv::Reader::from_reader(&mut file);
    let (row_count, counts) = inspect_rows(&mut reader, path, &expected.chain, true)?;
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

fn verify_checkout_pin(root: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .with_context(|| format!("inspect research checkout {}", root.display()))?;
    ensure!(
        output.status.success(),
        "{} is not a readable git checkout",
        root.display()
    );
    let head = String::from_utf8(output.stdout).context("research checkout HEAD is not UTF-8")?;
    ensure!(
        head.trim() == PINNED_RESEARCH_COMMIT,
        "research checkout {} is at {}, expected {}; use a checkout at the pinned merge or pass --artifact-root for a content-verified artifact directory",
        root.display(),
        head.trim(),
        PINNED_RESEARCH_COMMIT
    );
    Ok(())
}

fn verify_research_manifest(artifact_root: &Path, manifest: &PublicationManifest) -> Result<()> {
    let path = artifact_root.join(&manifest.publication_manifest_path);
    reject_lfs_pointer(&path)?;
    let actual = sha256_file(&path)?;
    ensure!(
        actual == manifest.publication_manifest_sha256,
        "research publication manifest checksum mismatch for {}: expected {}, got {}",
        path.display(),
        manifest.publication_manifest_sha256,
        actual
    );
    Ok(())
}

fn inspect_csv(
    path: &Path,
    spec: &HistoricalChainSpec,
    expected: Option<&PublicationArtifact>,
) -> Result<ArtifactPreflight> {
    let mut file = open_artifact_file(path, expected)?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    let mut reader = csv::Reader::from_reader(&mut file);
    verify_header(reader.headers()?, spec.chain)?;
    let (row_count, counts) = inspect_rows(&mut reader, path, spec.chain, expected.is_some())?;
    drop(reader);
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    if let Some(expected) = expected {
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
    }
    if spec.lifecycle == SourceLifecycle::Surveyed {
        ensure!(
            row_count == 0,
            "Surveyed source {} must remain zero-row until its lifecycle changes",
            spec.chain
        );
    }
    Ok(ArtifactPreflight {
        row_count,
        counts,
        file,
    })
}

fn open_artifact_file(path: &Path, expected: Option<&PublicationArtifact>) -> Result<File> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    reject_lfs_pointer_from(&mut file, path)?;
    if let Some(expected) = expected {
        let size = file
            .metadata()
            .with_context(|| format!("stat {}", path.display()))?
            .len();
        ensure!(
            size == expected.size_bytes,
            "artifact size mismatch for {}: expected {}, got {}",
            path.display(),
            expected.size_bytes,
            size
        );
        let actual = sha256_open_file(&mut file, path)?;
        ensure!(
            actual == expected.sha256,
            "artifact checksum mismatch for {}: expected {}, got {}",
            path.display(),
            expected.sha256,
            actual
        );
    }
    Ok(file)
}

fn inspect_rows<R: Read>(
    reader: &mut csv::Reader<R>,
    path: &Path,
    chain: &str,
    require_valid_taxonomy: bool,
) -> Result<(u64, PublicationCounts)> {
    let headers = reader.headers()?.clone();
    let indices = CountIndices::new(&headers)?;
    let mut row_count = 0_u64;
    let mut counts = PublicationCounts::default();
    for (offset, record) in reader.records().enumerate() {
        let record =
            record.with_context(|| format!("parse {} row {}", path.display(), offset + 2))?;
        ensure!(
            record.get(indices.chain).map(str::trim) == Some(chain),
            "{} row {} has a mismatched chain field",
            path.display(),
            offset + 2
        );
        if let Err(error) = count_row(&record, indices, &mut counts)
            && require_valid_taxonomy
        {
            return Err(error)
                .with_context(|| format!("classify {} row {}", path.display(), offset + 2));
        }
        row_count += 1;
    }
    Ok((row_count, counts))
}

fn reject_lfs_pointer(path: &Path) -> Result<()> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    reject_lfs_pointer_from(&mut file, path)
}

fn reject_lfs_pointer_from(file: &mut File, path: &Path) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    let mut prefix = [0_u8; 128];
    let count = file
        .read(&mut prefix)
        .with_context(|| format!("read {}", path.display()))?;
    if prefix[..count].starts_with(b"version https://git-lfs.github.com/spec/v1") {
        bail!(
            "{} is a Git LFS pointer, not artifact content; run: git lfs pull --include=\"results/monitor-evidence/*_monitor_evidence.csv\"",
            path.display()
        );
    }
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    sha256_open_file(&mut file, path)
}

fn sha256_open_file(file: &mut File, path: &Path) -> Result<String> {
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    let mut engine = sha256::Hash::engine();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .with_context(|| format!("hash {}", path.display()))?;
        if count == 0 {
            break;
        }
        engine.input(&buffer[..count]);
    }
    let digest = sha256::Hash::from_engine(engine).to_string();
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    Ok(digest)
}

fn verify_header(headers: &csv::StringRecord, chain: &str) -> Result<()> {
    let mut expected = NORMALIZED_COLUMNS.to_vec();
    if chain == "rsk" {
        expected.extend_from_slice(RSK_SIDECAR_COLUMNS);
    }
    ensure!(
        headers.iter().map(str::trim).eq(expected.iter().copied()),
        "CSV header does not match the normalized {chain} schema"
    );
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct CountIndices {
    chain: usize,
    classification: usize,
    relevance: usize,
    relevance_reason: usize,
}

impl CountIndices {
    fn new(headers: &csv::StringRecord) -> Result<Self> {
        let by_name = headers
            .iter()
            .enumerate()
            .map(|(index, name)| (name.trim(), index))
            .collect::<BTreeMap<_, _>>();
        Ok(Self {
            chain: *by_name.get("chain").context("missing chain column")?,
            classification: *by_name
                .get("classification")
                .context("missing classification column")?,
            relevance: *by_name
                .get("btc_stale_relevance")
                .context("missing btc_stale_relevance column")?,
            relevance_reason: *by_name
                .get("relevance_reason")
                .context("missing relevance_reason column")?,
        })
    }
}

fn count_row(
    record: &csv::StringRecord,
    indices: CountIndices,
    counts: &mut PublicationCounts,
) -> Result<()> {
    let classification = record
        .get(indices.classification)
        .map(str::trim)
        .unwrap_or_default();
    let relevance = record
        .get(indices.relevance)
        .map(str::trim)
        .unwrap_or_default();
    let relevance_reason = record
        .get(indices.relevance_reason)
        .map(str::trim)
        .unwrap_or_default();
    match publication_category(classification, relevance, relevance_reason)
        .map_err(|reason| anyhow::anyhow!("{}", reason.as_str()))?
    {
        PublicationCategory::Canonical => counts.canonical += 1,
        PublicationCategory::Stale => counts.stale += 1,
        PublicationCategory::StaleDescendant => counts.stale_descendant += 1,
        PublicationCategory::StrictBtcOrphan => counts.strict_btc_orphan += 1,
        PublicationCategory::WeakBtcOrphan => counts.weak_btc_orphan += 1,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::historical_ingest::config::{HistoricalImportAllConfig, historical_chain_spec};

    fn temp_path(label: &str) -> std::path::PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "mmm-publication-{label}-{}-{suffix}",
            std::process::id()
        ))
    }

    fn valid_manifest() -> PublicationManifest {
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
                }
            })
            .collect::<Vec<_>>();
        artifacts.push(PublicationArtifact {
            chain: "stale-descendants".to_owned(),
            csv_path: "results/stale-descendants.csv".to_owned(),
            role: ArtifactRole::Aggregate,
            row_count: 20,
            size_bytes: 1,
            sha256: "0".repeat(64),
            counts: PublicationCounts {
                stale_descendant: 20,
                ..PublicationCounts::default()
            },
        });
        PublicationManifest {
            schema_version: 2,
            source_repo: "merge-mining-research".to_owned(),
            source_repo_commit: PINNED_RESEARCH_COMMIT.to_owned(),
            publication_manifest_path: "results/monitor-evidence/manifest.json".to_owned(),
            publication_manifest_sha256: "0".repeat(64),
            total_event_rows: 576_662,
            aggregate_rows: 20,
            required_columns: NORMALIZED_COLUMNS
                .iter()
                .map(|column| (*column).to_owned())
                .collect(),
            artifacts,
        }
    }

    #[test]
    fn normalized_schema_is_one_uniform_common_header() {
        assert_eq!(NORMALIZED_COLUMNS.len(), 27);
        assert_eq!(NORMALIZED_COLUMNS[6], "child_height");
        assert_eq!(NORMALIZED_COLUMNS[8], "child_header_hex");
        assert_eq!(NORMALIZED_COLUMNS[10], "child_nbits");
    }

    #[test]
    fn manifest_integrity_failures_are_rejected() {
        for (case, expected) in [
            ("schema", "unsupported schema_version"),
            ("commit", "source_repo_commit must be pinned"),
            (
                "columns",
                "required_columns do not match the normalized schema",
            ),
            (
                "classification_sum",
                "row_count does not equal classification counts",
            ),
            ("invalid_sha", "has invalid sha256"),
            (
                "missing_event",
                "event artifact set does not match the source registry",
            ),
            ("duplicate_event", "duplicate event artifact"),
            (
                "missing_aggregate",
                "stale-descendants aggregate artifact is required",
            ),
            (
                "aggregate_total",
                "aggregate_rows does not equal aggregate artifact rows",
            ),
        ] {
            let mut manifest = valid_manifest();
            match case {
                "schema" => manifest.schema_version = 3,
                "commit" => manifest.source_repo_commit = "f".repeat(40),
                "columns" => {
                    manifest.required_columns.pop();
                }
                "classification_sum" => {
                    let event = manifest
                        .artifacts
                        .iter_mut()
                        .find(|artifact| {
                            artifact.role == ArtifactRole::Event && artifact.row_count > 0
                        })
                        .expect("non-empty event artifact");
                    event.counts.canonical += 1;
                }
                "invalid_sha" => {
                    manifest.artifacts[0].sha256 = "F".repeat(64);
                }
                "missing_event" => {
                    let index = manifest
                        .artifacts
                        .iter()
                        .position(|artifact| artifact.role == ArtifactRole::Event)
                        .expect("event artifact");
                    manifest.artifacts.remove(index);
                }
                "duplicate_event" => {
                    let duplicate = manifest
                        .artifacts
                        .iter()
                        .find(|artifact| artifact.role == ArtifactRole::Event)
                        .expect("event artifact")
                        .clone();
                    manifest.artifacts.push(duplicate);
                }
                "missing_aggregate" => {
                    manifest
                        .artifacts
                        .retain(|artifact| artifact.role != ArtifactRole::Aggregate);
                }
                "aggregate_total" => manifest.aggregate_rows = 21,
                _ => unreachable!("table defines every case"),
            }
            let error = validate_manifest(&manifest).expect_err(case);
            assert!(
                error.to_string().contains(expected),
                "{case}: expected {expected:?}, got {error:#}"
            );
        }
    }

    #[test]
    fn artifact_preflight_rejects_checksum_and_row_count_drift() {
        let path = temp_path("preflight.csv");
        std::fs::write(&path, format!("{}\n", NORMALIZED_COLUMNS.join(",")))
            .expect("write fixture artifact");
        let size_bytes = std::fs::metadata(&path).expect("fixture metadata").len();
        let spec = historical_chain_spec("devcoin").expect("devcoin spec");
        let mut expected = PublicationArtifact {
            chain: "devcoin".to_owned(),
            csv_path: path.display().to_string(),
            role: ArtifactRole::Event,
            row_count: 0,
            size_bytes,
            sha256: "0".repeat(64),
            counts: PublicationCounts::default(),
        };

        let checksum_error =
            inspect_csv(&path, spec, Some(&expected)).expect_err("wrong checksum must fail");
        assert!(checksum_error.to_string().contains("checksum mismatch"));

        expected.sha256 = sha256_file(&path).expect("hash fixture");
        expected.row_count = 1;
        expected.counts.canonical = 1;
        let row_count_error =
            inspect_csv(&path, spec, Some(&expected)).expect_err("wrong row count must fail");
        std::fs::remove_file(&path).expect("remove fixture artifact");
        assert!(row_count_error.to_string().contains("row-count mismatch"));
    }

    #[test]
    fn verified_artifact_reader_survives_path_replacement() {
        let path = temp_path("snapshot.csv");
        let replacement = temp_path("replacement.csv");
        std::fs::write(&path, format!("{}\n", NORMALIZED_COLUMNS.join(",")))
            .expect("write original artifact");
        let spec = historical_chain_spec("devcoin").expect("devcoin spec");
        let mut artifact = inspect_csv(&path, spec, None).expect("preflight original artifact");

        let replacement_row = vec!["replacement"; NORMALIZED_COLUMNS.len()].join(",");
        std::fs::write(
            &replacement,
            format!("{}\n{replacement_row}\n", NORMALIZED_COLUMNS.join(",")),
        )
        .expect("write replacement artifact");
        std::fs::rename(&replacement, &path).expect("replace artifact path");

        let (mut reader, _) = artifact.open_reader(spec).expect("open verified snapshot");
        assert_eq!(reader.records().count(), 0);
        drop(reader);
        std::fs::remove_file(&path).expect("remove replacement artifact");
    }

    #[test]
    fn import_all_preflights_the_required_aggregate_artifact() {
        let root = temp_path("aggregate-root");
        let publication_dir = root.join("results/monitor-evidence");
        std::fs::create_dir_all(&publication_dir).expect("create publication fixture");

        let research_manifest = b"{\"fixture\":\"aggregate-preflight\"}\n";
        std::fs::write(publication_dir.join("manifest.json"), research_manifest)
            .expect("write research manifest");
        let aggregate_path = root.join("results/stale-descendants.csv");
        let mut aggregate =
            "chain,classification,btc_stale_relevance,relevance_reason\n".to_owned();
        for _ in 0..20 {
            aggregate.push_str("stale-descendants,stale_descendant,,valid_stale_descendant\n");
        }
        std::fs::write(&aggregate_path, &aggregate).expect("write aggregate fixture");

        let mut manifest = valid_manifest();
        manifest.publication_manifest_sha256 = sha256::Hash::hash(research_manifest).to_string();
        let aggregate_artifact = manifest
            .artifacts
            .iter_mut()
            .find(|artifact| artifact.role == ArtifactRole::Aggregate)
            .expect("aggregate artifact");
        aggregate_artifact.size_bytes = aggregate.len() as u64;
        aggregate_artifact.sha256 = sha256::Hash::hash(aggregate.as_bytes()).to_string();

        let manifest_path = root.join("monitor-manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize manifest"),
        )
        .expect("write monitor manifest");
        let config = HistoricalImportAllConfig {
            manifest_path,
            artifact_root: root.clone(),
            require_pinned_checkout: false,
            batch_size: 10,
            allow_unclassified: false,
            allow_empty_known_stales: true,
        };

        preflight_required_aggregate_artifacts(&config).expect("aggregate preflight");
        std::fs::remove_file(&aggregate_path).expect("remove aggregate fixture");
        let error = preflight_required_aggregate_artifacts(&config)
            .expect_err("missing aggregate must fail import-all preflight");
        assert!(error.to_string().contains("open"));
        std::fs::remove_dir_all(&root).expect("remove aggregate fixture root");
    }

    #[test]
    fn publication_counting_matches_research_taxonomy_precedence() {
        let indices = CountIndices {
            chain: 0,
            classification: 1,
            relevance: 2,
            relevance_reason: 3,
        };
        let mut counts = PublicationCounts::default();
        for fields in [
            ["devcoin", "canonical", "", ""],
            ["devcoin", "stale", "", "valid_direct_stale"],
            ["devcoin", "unknown", "", "valid_stale_descendant"],
            ["devcoin", "unknown", "", "valid_direct_stale"],
            ["devcoin", "unknown", "strict_btc_orphan", ""],
            ["devcoin", "unknown", "weak_btc_orphan", ""],
        ] {
            count_row(
                &csv::StringRecord::from(fields.as_slice()),
                indices,
                &mut counts,
            )
            .expect("published taxonomy row");
        }

        assert_eq!(
            counts,
            PublicationCounts {
                canonical: 1,
                stale: 2,
                stale_descendant: 1,
                strict_btc_orphan: 1,
                weak_btc_orphan: 1,
            }
        );

        for fields in [
            [
                "devcoin",
                "unknown",
                "strict_btc_orphan",
                "valid_stale_descendant",
            ],
            ["devcoin", "canonical", "", "valid_stale_descendant"],
        ] {
            let contradictory = csv::StringRecord::from(fields.as_slice());
            let error = count_row(&contradictory, indices, &mut counts)
                .expect_err("preflight and importer must reject contradictory taxonomy");
            assert!(error.to_string().contains("taxonomy_mismatch"));
        }
    }

    #[test]
    fn lfs_pointer_error_includes_the_recovery_command() {
        let path = std::env::temp_dir().join(format!("mmm-lfs-pointer-{}", std::process::id()));
        std::fs::write(
            &path,
            "version https://git-lfs.github.com/spec/v1\noid sha256:00\nsize 1\n",
        )
        .unwrap();
        let error = reject_lfs_pointer(&path).unwrap_err();
        std::fs::remove_file(&path).unwrap();
        assert!(error.to_string().contains(
            "git lfs pull --include=\"results/monitor-evidence/*_monitor_evidence.csv\""
        ));
    }
}
