//! CLI configuration and source-registry resolution for monitor-evidence import.
//!
//! Historical import has one input contract: the normalized per-chain
//! `results/monitor-evidence/<chain>_monitor_evidence.csv` publication. Source
//! lifecycle controls reconciliation behavior, never CSV shape.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use mmm_capture::source_registry::{SOURCE_REGISTRY, SourceKind, SourceLifecycle};

pub(super) const PINNED_RESEARCH_COMMIT: &str = "2146c204a8a203c59534a1b23b04f447a47b499e";
pub(super) const OPERATOR_CSV_PROVENANCE: &str = "operator-csv";
pub(super) const DEFAULT_MANIFEST_PATH: &str = "data/historical/historical-source-manifest.json";
const RESEARCH_ROOT_ENV: &str = "MERGE_MINING_RESEARCH_DIR";
const DEFAULT_BATCH_SIZE: usize = 500;

/// Registry-backed source metadata for one published per-chain artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoricalChainSpec {
    pub(super) chain: &'static str,
    pub(super) source_code: &'static str,
    pub(super) lifecycle: SourceLifecycle,
}

impl HistoricalChainSpec {
    pub(super) const fn is_authoritative(self) -> bool {
        matches!(
            self.lifecycle,
            SourceLifecycle::Historical | SourceLifecycle::Partial
        )
    }
}

fn build_importable_chains() -> Vec<HistoricalChainSpec> {
    SOURCE_REGISTRY
        .iter()
        .filter(|source| {
            source.kind == SourceKind::Auxpow && source.lifecycle != SourceLifecycle::Catalogued
        })
        .map(|source| HistoricalChainSpec {
            chain: source.chain,
            source_code: source.code,
            lifecycle: source.lifecycle,
        })
        .collect()
}

static IMPORTABLE_CHAINS: LazyLock<Vec<HistoricalChainSpec>> =
    LazyLock::new(build_importable_chains);

pub(super) fn importable_chains() -> &'static [HistoricalChainSpec] {
    &IMPORTABLE_CHAINS
}

pub(super) fn historical_chain_spec(chain: &str) -> Option<&'static HistoricalChainSpec> {
    IMPORTABLE_CHAINS.iter().find(|spec| spec.chain == chain)
}

/// Resolved parameters for one normalized per-chain import.
#[derive(Debug, Clone)]
pub struct HistoricalImportConfig {
    pub chain: String,
    pub csv_path: PathBuf,
    pub manifest_path: Option<PathBuf>,
    pub artifact_root: Option<PathBuf>,
    pub require_pinned_checkout: bool,
    pub batch_size: usize,
    pub limit: Option<usize>,
    /// Allows canonical/stale rows to be loaded without live Bitcoin Core.
    /// Strict/weak unknown rows still require Core absence attestation.
    pub allow_unclassified: bool,
    /// Allows a deliberately membership-free diagnostic database.
    pub allow_empty_known_stales: bool,
}

/// Resolved options for the manifest-driven full publication import.
#[derive(Debug, Clone)]
pub struct HistoricalImportAllConfig {
    pub manifest_path: PathBuf,
    pub artifact_root: PathBuf,
    pub require_pinned_checkout: bool,
    pub batch_size: usize,
    pub allow_unclassified: bool,
    pub allow_empty_known_stales: bool,
}

impl HistoricalImportConfig {
    /// Build an additive fixture/operator override. The CSV must still use the
    /// normalized schema, but publication digest, count checks, and
    /// authoritative removal are intentionally absent.
    pub fn for_csv(chain: impl Into<String>, csv_path: impl Into<PathBuf>) -> Self {
        Self {
            chain: chain.into(),
            csv_path: csv_path.into(),
            manifest_path: None,
            artifact_root: None,
            require_pinned_checkout: false,
            batch_size: DEFAULT_BATCH_SIZE,
            limit: None,
            allow_unclassified: false,
            allow_empty_known_stales: false,
        }
    }

    pub(super) fn publication_ref(&self) -> &'static str {
        if self.manifest_path.is_some() {
            PINNED_RESEARCH_COMMIT
        } else {
            OPERATOR_CSV_PROVENANCE
        }
    }

    pub(super) fn is_authoritative_snapshot(&self, spec: &HistoricalChainSpec) -> bool {
        self.manifest_path.is_some() && self.limit.is_none() && spec.is_authoritative()
    }

    /// Parse `import-dataset <chain> [flags...]`.
    ///
    /// Without `--csv`, the monitor-owned publication manifest selects the
    /// artifact beneath `MERGE_MINING_RESEARCH_DIR` or `--artifact-root`.
    /// An explicitly supplied artifact root is accepted by content identity;
    /// an environment-selected git checkout must itself be at the pinned merge.
    pub fn from_args(mut args: std::env::Args) -> Result<Self> {
        let chain = args
            .next()
            .ok_or_else(|| anyhow::anyhow!(usage_message()))?;
        if matches!(chain.as_str(), "-h" | "--help") {
            bail!(usage_message());
        }
        historical_chain_spec(&chain)
            .ok_or_else(|| anyhow::anyhow!("unsupported published chain {chain:?}"))?;

        let mut csv_path = None;
        let mut manifest_path = PathBuf::from(DEFAULT_MANIFEST_PATH);
        let mut artifact_root = None;
        let mut batch_size = DEFAULT_BATCH_SIZE;
        let mut limit = None;
        let mut allow_unclassified = false;
        let mut allow_empty_known_stales = false;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--csv" => csv_path = Some(next_path(&mut args, "--csv")?),
                "--manifest" => {
                    manifest_path = next_path(&mut args, "--manifest")?;
                }
                "--artifact-root" => {
                    artifact_root = Some(next_path(&mut args, "--artifact-root")?);
                }
                "--batch-size" => {
                    batch_size = next_usize(&mut args, "--batch-size")?;
                    if batch_size == 0 {
                        bail!("--batch-size must be greater than zero");
                    }
                }
                "--limit" => limit = Some(next_usize(&mut args, "--limit")?),
                "--allow-unclassified" => allow_unclassified = true,
                "--allow-empty-known-stales" => {
                    allow_empty_known_stales = true;
                }
                "-h" | "--help" => bail!(usage_message()),
                other => bail!(
                    "unknown import-dataset argument {other:?}\n{}",
                    usage_message()
                ),
            }
        }

        if csv_path.is_some() && artifact_root.is_some() {
            bail!("--csv and --artifact-root are mutually exclusive");
        }
        if let Some(csv_path) = csv_path {
            let mut config = Self::for_csv(chain, csv_path);
            config.batch_size = batch_size;
            config.limit = limit;
            config.allow_unclassified = allow_unclassified;
            config.allow_empty_known_stales = allow_empty_known_stales;
            return Ok(config);
        }

        let explicit_artifact_root = artifact_root.is_some();
        let artifact_root = artifact_root
            .or_else(|| std::env::var_os(RESEARCH_ROOT_ENV).map(PathBuf::from))
            .ok_or_else(|| {
                anyhow::anyhow!("set {RESEARCH_ROOT_ENV}, pass --artifact-root, or pass --csv")
            })?;
        let csv_path = resolve_manifest_csv_path(&chain, &manifest_path, &artifact_root)?;
        Ok(Self {
            chain,
            csv_path,
            manifest_path: Some(manifest_path),
            artifact_root: Some(artifact_root),
            require_pinned_checkout: !explicit_artifact_root,
            batch_size,
            limit,
            allow_unclassified,
            allow_empty_known_stales,
        })
    }
}

impl HistoricalImportAllConfig {
    pub fn from_args(mut args: std::env::Args) -> Result<Self> {
        let mut manifest_path = PathBuf::from(DEFAULT_MANIFEST_PATH);
        let mut artifact_root = None;
        let mut batch_size = DEFAULT_BATCH_SIZE;
        let mut allow_unclassified = false;
        let mut allow_empty_known_stales = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--manifest" => manifest_path = next_path(&mut args, "--manifest")?,
                "--artifact-root" => {
                    artifact_root = Some(next_path(&mut args, "--artifact-root")?);
                }
                "--batch-size" => {
                    batch_size = next_usize(&mut args, "--batch-size")?;
                    if batch_size == 0 {
                        bail!("--batch-size must be greater than zero");
                    }
                }
                "--allow-unclassified" => allow_unclassified = true,
                "--allow-empty-known-stales" => allow_empty_known_stales = true,
                "-h" | "--help" => bail!(import_all_usage_message()),
                other => bail!(
                    "unknown import-all argument {other:?}\n{}",
                    import_all_usage_message()
                ),
            }
        }
        let explicit_artifact_root = artifact_root.is_some();
        let artifact_root = artifact_root
            .or_else(|| std::env::var_os(RESEARCH_ROOT_ENV).map(PathBuf::from))
            .ok_or_else(|| anyhow::anyhow!("set {RESEARCH_ROOT_ENV} or pass --artifact-root"))?;
        Ok(Self {
            manifest_path,
            artifact_root,
            require_pinned_checkout: !explicit_artifact_root,
            batch_size,
            allow_unclassified,
            allow_empty_known_stales,
        })
    }

    pub(super) fn chain_configs(&self) -> Result<Vec<HistoricalImportConfig>> {
        let manifest = super::publication::load_publication_manifest(&self.manifest_path)?;
        let mut artifacts = manifest.event_artifacts().collect::<Vec<_>>();
        artifacts.sort_by_key(|artifact| artifact.chain.as_str());
        artifacts
            .into_iter()
            .map(|artifact| {
                Ok(HistoricalImportConfig {
                    chain: artifact.chain.clone(),
                    csv_path: self.artifact_root.join(&artifact.csv_path),
                    manifest_path: Some(self.manifest_path.clone()),
                    artifact_root: Some(self.artifact_root.clone()),
                    require_pinned_checkout: self.require_pinned_checkout,
                    batch_size: self.batch_size,
                    limit: None,
                    allow_unclassified: self.allow_unclassified,
                    allow_empty_known_stales: self.allow_empty_known_stales,
                })
            })
            .collect()
    }
}

fn next_path(args: &mut std::env::Args, flag: &str) -> Result<PathBuf> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a path"))
}

fn next_usize(args: &mut std::env::Args, flag: &str) -> Result<usize> {
    let value = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))?;
    value
        .parse()
        .with_context(|| format!("{flag} must be a non-negative integer"))
}

fn usage_message() -> &'static str {
    "usage: import-dataset <chain> [--csv PATH | --artifact-root DIR] \
     [--manifest PATH] [--batch-size N] [--limit N] [--allow-unclassified] \
     [--allow-empty-known-stales]"
}

fn import_all_usage_message() -> &'static str {
    "usage: import-all [--artifact-root DIR] [--manifest PATH] [--batch-size N] \
     [--allow-unclassified] [--allow-empty-known-stales]"
}

fn resolve_manifest_csv_path(
    chain: &str,
    manifest_path: &Path,
    artifact_root: &Path,
) -> Result<PathBuf> {
    let manifest = super::publication::load_publication_manifest(manifest_path)?;
    let artifact = manifest.event_artifact(chain).ok_or_else(|| {
        anyhow::anyhow!("publication manifest has no event artifact for {chain:?}")
    })?;
    Ok(artifact_root.join(&artifact.csv_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_defines_all_twenty_seven_published_chain_sources() {
        assert_eq!(importable_chains().len(), 27);
        let mut seen = std::collections::BTreeSet::new();
        for spec in importable_chains() {
            assert!(spec.source_code.starts_with("auxpow:"));
            assert!(seen.insert(spec.chain), "duplicate chain {}", spec.chain);
        }
        assert_eq!(
            historical_chain_spec("doichain").map(|spec| spec.lifecycle),
            Some(SourceLifecycle::Surveyed)
        );
        assert_eq!(
            historical_chain_spec("namecoin").map(|spec| spec.lifecycle),
            Some(SourceLifecycle::Live)
        );
        assert!(historical_chain_spec("jax-network").is_none());
    }

    #[test]
    fn authoritative_semantics_follow_lifecycle_only() {
        assert!(
            historical_chain_spec("devcoin")
                .copied()
                .expect("devcoin")
                .is_authoritative()
        );
        assert!(
            historical_chain_spec("vcash")
                .copied()
                .expect("vcash")
                .is_authoritative()
        );
        assert!(
            !historical_chain_spec("namecoin")
                .copied()
                .expect("namecoin")
                .is_authoritative()
        );
        assert!(
            !historical_chain_spec("doichain")
                .copied()
                .expect("doichain")
                .is_authoritative()
        );
    }
}
