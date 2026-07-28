//! CLI argument parsing and dataset-source discovery for `import-dataset`.
//!
//! Holds the `HISTORICAL_CHAINS` table (the recovered-evidence analogue of the
//! live `chains::spec` table) and the layered default-path search that
//! locates an evidence CSV when `--csv` is not given. All env reads for the
//! importer live here so the runner stays I/O-policy free.
//!
//! `HISTORICAL_CHAINS` is derived from `mmm_capture::source_registry`'s
//! `Historical`/`Partial` lifecycle rows, not hand-listed: the registry
//! already owns `source_code` and `chain` as the single source of truth, so
//! this module supplies only `height_column`, the one field the registry does
//! not carry (see `HEIGHT_COLUMNS`).

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};
use mmm_capture::source_registry::{SOURCE_REGISTRY, SourceLifecycle};
use serde::Deserialize;

const RESEARCH_ROOT_ENV: &str = "MERGE_MINING_RESEARCH_DIR";
const ARCHIVE_ROOT_ENV: &str = "MERGE_MINING_ARCHIVE_DIR";

/// Static per-chain row for one recovered full or partial merge-mining source.
///
/// The historical counterpart to `chains::spec`: `source_code` is the
/// `auxpow:<chain>` db source key the runner resolves to a `source_id`, and
/// `height_column` names the chain-specific child-height CSV column (chains with
/// a normalized `child_height` column use that literal instead).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoricalChainSpec {
    pub(super) chain: &'static str,
    /// `auxpow:<chain>` db source key, resolved to a `source_id` at import time.
    pub(super) source_code: &'static str,
    /// Chain-specific child-height column name in the source CSV.
    pub(super) height_column: &'static str,
}

impl HistoricalChainSpec {
    /// Explicit recovery artifacts carry authoritative child identity and time.
    /// They must never fall back to a synthetic hash or Bitcoin parent time.
    pub(super) fn requires_exact_child_fields(&self) -> bool {
        matches!(self.chain, "vcash" | "lyncoin" | "sixeleven")
    }
}

/// Resolved invocation parameters for one `import-dataset` run.
///
/// Produced by `from_args` with all defaults already applied: `csv_path` and
/// `relevance_path` are concrete (defaults resolved against the manifest and
/// archive roots), so the runner does no further path discovery.
#[derive(Debug, Clone)]
pub struct HistoricalImportConfig {
    pub chain: String,
    pub csv_path: PathBuf,
    pub relevance_path: Option<PathBuf>,
    pub batch_size: usize,
    pub limit: Option<usize>,
    /// When set, ingest canonical/stale rows without a live Bitcoin Core
    /// classifier (`BITCOIN_RPC_URL` unset). Unknown rows (admitted BTC-orphan
    /// and known-branch rows) still require live classification.
    pub allow_unclassified: bool,
    /// When set, run even though the `known_stale_block` membership is empty.
    /// The default refusal follows the research repo's lesson: without the
    /// upstream stale-blocks dataset a known stale cannot be excluded and may be
    /// mislabelled strict/weak, so the importer stops rather than ingest against
    /// a membership it cannot consult.
    pub allow_empty_known_stales: bool,
}

/// On-disk schema of `data/historical/historical-source-manifest.json`: a list of per-chain CSV
/// paths plus an optional repo base they are joined onto.
#[derive(Debug, Deserialize)]
struct HistoricalSourceManifest {
    source_repo_default_path: Option<String>,
    sources: Vec<HistoricalManifestSource>,
}

/// One `sources[]` entry: a chain key paired with its CSV path (relative to the
/// manifest's `source_repo_default_path`, or to the configured research root).
#[derive(Debug, Deserialize)]
struct HistoricalManifestSource {
    chain: String,
    csv_path: String,
}

/// Progress-log cadence and default ingest batch size when `--batch-size` is omitted.
const DEFAULT_BATCH_SIZE: usize = 500;

/// Chain-specific child-height CSV column name for each historical/partial
/// source. The one field a `SOURCE_REGISTRY` row does not carry: every other
/// `HistoricalChainSpec` field (`chain`, `source_code`) is read straight off
/// the matching registry row in `build_historical_chains`.
const HEIGHT_COLUMNS: &[(&str, &str)] = &[
    ("argentum", "arg_height"),
    ("bitcoin-vault", "btcv_height"),
    ("bitmark", "btmk_height"),
    ("coiledcoin", "clc_height"),
    ("crown", "crown_height"),
    ("devcoin", "dvc_height"),
    ("emercoin", "emc_height"),
    ("geistgeld", "geistgeld_height"),
    ("groupcoin", "groupcoin_height"),
    ("huntercoin", "huc_height"),
    ("i0coin", "child_height"),
    ("ixcoin", "ixc_height"),
    ("myriadcoin", "xmy_height"),
    ("terracoin", "trc_height"),
    ("unobtanium", "uno_height"),
    ("xaya", "child_height"),
    ("elcash", "elc_height"),
    ("vcash", "vcash_height"),
    ("lyncoin", "child_height"),
    ("sixeleven", "child_height"),
];

/// Look up `chain`'s CSV column name. Panics if a `Historical`/`Partial`
/// `SOURCE_REGISTRY` row has no matching `HEIGHT_COLUMNS` entry: a silent
/// fallback would ship an importer that cannot find its own height column, so
/// an incomplete side table must fail loudly at the first lookup instead.
fn height_column_for(chain: &'static str) -> &'static str {
    HEIGHT_COLUMNS
        .iter()
        .find(|(name, _)| *name == chain)
        .map(|(_, column)| *column)
        .unwrap_or_else(|| panic!("no height_column configured for historical chain {chain:?}"))
}

/// Build `HISTORICAL_CHAINS` from `SOURCE_REGISTRY`'s `Historical`/`Partial`
/// lifecycle rows. Extend the accepted set by adding a `historical_auxpow` or
/// `partial_auxpow` row to `SOURCE_REGISTRY` plus a `HEIGHT_COLUMNS` entry
/// here, never by cloning a module.
fn build_historical_chains() -> Vec<HistoricalChainSpec> {
    SOURCE_REGISTRY
        .iter()
        .filter(|def| {
            matches!(
                def.lifecycle,
                SourceLifecycle::Historical | SourceLifecycle::Partial
            )
        })
        .map(|def| HistoricalChainSpec {
            chain: def.chain,
            source_code: def.code,
            height_column: height_column_for(def.chain),
        })
        .collect()
}

/// The closed set of recovered full and partial merge-mining sources
/// accepted, computed once from `SOURCE_REGISTRY` and cached.
static HISTORICAL_CHAINS: LazyLock<Vec<HistoricalChainSpec>> =
    LazyLock::new(build_historical_chains);

/// Look up a chain's static spec by exact name, or `None` if unsupported.
pub(super) fn historical_chain_spec(chain: &str) -> Option<&'static HistoricalChainSpec> {
    HISTORICAL_CHAINS.iter().find(|spec| spec.chain == chain)
}

impl HistoricalImportConfig {
    /// Parse `import-dataset <chain> [flags...]` from CLI args (the leading
    /// chain is positional; the rest are flags). Rejects unknown chains, unknown
    /// flags, and a zero `--batch-size`. When `--csv` is omitted, resolves the
    /// CSV via `resolve_default_csv_path`; `--relevance` falls back to the
    /// default inventory only if that file exists. Errors carry `usage_message`.
    pub fn from_args(mut args: std::env::Args) -> Result<Self> {
        let chain = args
            .next()
            .ok_or_else(|| anyhow::anyhow!(usage_message()))?;
        if matches!(chain.as_str(), "-h" | "--help") {
            bail!(usage_message());
        }
        let spec = historical_chain_spec(&chain)
            .ok_or_else(|| anyhow::anyhow!("unsupported historical chain {chain:?}"))?;
        let mut csv_path = None;
        let mut manifest_path = PathBuf::from("data/historical/historical-source-manifest.json");
        let mut relevance_path = None;
        let mut batch_size = DEFAULT_BATCH_SIZE;
        let mut limit = None;
        let mut allow_unclassified = false;
        let mut allow_empty_known_stales = false;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--csv" => csv_path = Some(next_path(&mut args, "--csv")?),
                "--manifest" => manifest_path = next_path(&mut args, "--manifest")?,
                "--relevance" => relevance_path = Some(next_path(&mut args, "--relevance")?),
                "--batch-size" => {
                    batch_size = next_usize(&mut args, "--batch-size")?;
                    if batch_size == 0 {
                        bail!("--batch-size must be greater than zero");
                    }
                }
                "--limit" => limit = Some(next_usize(&mut args, "--limit")?),
                "--allow-unclassified" => allow_unclassified = true,
                "--allow-empty-known-stales" => allow_empty_known_stales = true,
                "-h" | "--help" => bail!(usage_message()),
                other => bail!(
                    "unknown import-dataset argument {other:?}\n{}",
                    usage_message()
                ),
            }
        }

        let csv_path = match csv_path {
            Some(path) => path,
            None => resolve_default_csv_path(spec, &manifest_path)?,
        };
        let relevance_path = relevance_path.or_else(default_relevance_path);
        Ok(Self {
            chain,
            csv_path,
            relevance_path,
            batch_size,
            limit,
            allow_unclassified,
            allow_empty_known_stales,
        })
    }
}

/// Consume the next arg as a path value for `flag`, erroring if it is missing.
fn next_path(args: &mut std::env::Args, flag: &str) -> Result<PathBuf> {
    args.next()
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a path"))
}

/// Consume the next arg and parse it as a non-negative integer for `flag`.
fn next_usize(args: &mut std::env::Args, flag: &str) -> Result<usize> {
    let value = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))?;
    value
        .parse()
        .with_context(|| format!("{flag} must be a non-negative integer"))
}

/// The single usage string surfaced on `--help` and on any arg error.
fn usage_message() -> &'static str {
    "usage: import-dataset <chain> [--csv PATH] [--manifest PATH] \
     [--relevance PATH] [--batch-size N] [--limit N] [--allow-unclassified] \
     [--allow-empty-known-stales]"
}

/// Pick the first existing CSV from the ordered `default_csv_candidates` list,
/// or error listing every path tried so a missing dataset is diagnosable.
fn resolve_default_csv_path(spec: &HistoricalChainSpec, manifest_path: &Path) -> Result<PathBuf> {
    let candidates = default_csv_candidates(spec, manifest_path)?;
    if candidates.is_empty() {
        bail!(
            "no default historical CSV search roots configured for {}; set {}, {}, or pass --csv",
            spec.chain,
            RESEARCH_ROOT_ENV,
            ARCHIVE_ROOT_ENV
        );
    }
    candidates
        .iter()
        .find(|path| path.is_file())
        .cloned()
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no default historical CSV found for {}; tried {}",
                spec.chain,
                candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

/// Build the ordered default-CSV search list, highest-confidence source first:
/// the committed canonical-blocks artifact for exact-child-field chains (the
/// compact monitor-evidence exports omit `child_block_time`, which those
/// chains require, so monitor exports are never probed for them), then for
/// other chains the research repo's committed monitor-evidence export, then
/// normalized full-evidence output, then the archive's classified export,
/// then research data drops, then any manifest-named path, then
/// validated-stales. Order is precedence: `resolve_default_csv_path` takes the
/// first that exists. Deduplicated so a repeated path is probed once.
fn default_csv_candidates(
    spec: &HistoricalChainSpec,
    manifest_path: &Path,
) -> Result<Vec<PathBuf>> {
    let mut candidates = Vec::new();
    let research = research_root();
    if let Some(research) = &research {
        if spec.requires_exact_child_fields() {
            candidates.push(
                research
                    .join("data/canonical")
                    .join(format!("{}_canonical_blocks.csv", spec.chain)),
            );
        } else {
            // Monitor-evidence exports omit `child_block_time`, which the
            // exact-child-field chains require, so those chains never probe
            // them: a present-but-incompatible export would be selected and
            // abort the import ahead of a compatible fallback.
            candidates.push(
                research
                    .join("results/monitor-evidence")
                    .join(format!("{}_monitor_evidence.csv", spec.chain)),
            );
        }
        candidates.push(
            research
                .join("results/full-evidence")
                .join(format!("{}_evidence.csv", spec.chain)),
        );
    }
    if let Some(archive) = archive_root() {
        candidates.push(
            archive
                .join("chains")
                .join(spec.chain)
                .join("classified")
                .join(format!("{}_stale_blocks.csv", spec.chain)),
        );
    }
    if let Some(research) = &research {
        candidates.push(
            research
                .join("data")
                .join(format!("{}_stale_blocks.csv", spec.chain)),
        );
    }
    if manifest_path.is_file()
        && let Some(path) = manifest_csv_path(spec.chain, manifest_path, research.as_deref())?
    {
        candidates.push(path);
    }
    if let Some(research) = &research {
        candidates.push(
            research
                .join("data")
                .join(format!("{}_validated_stales.csv", spec.chain)),
        );
    }
    dedup_paths(&mut candidates);
    Ok(candidates)
}

/// Classified stale-block archive root, configured via `MERGE_MINING_ARCHIVE_DIR`.
fn archive_root() -> Option<PathBuf> {
    std::env::var_os(ARCHIVE_ROOT_ENV).map(PathBuf::from)
}

/// Merge-mining research repo root, configured via `MERGE_MINING_RESEARCH_DIR`.
fn research_root() -> Option<PathBuf> {
    std::env::var_os(RESEARCH_ROOT_ENV).map(PathBuf::from)
}

/// Default relevance inventory under the research root, but only if it exists:
/// returning `None` when absent keeps `--relevance` opt-out, not opt-out-broken.
fn default_relevance_path() -> Option<PathBuf> {
    let path = research_root()?
        .join("results")
        .join("recovery")
        .join("btc-stale-relevance-inventory.csv");
    path.is_file().then_some(path)
}

/// Resolve a chain's CSV path from the JSON manifest: join the chain's relative
/// `csv_path` onto the manifest base (`source_repo_default_path`, `$HOME`-expanded,
/// else the configured research root). `Ok(None)` when the chain has no
/// manifest entry or no base is configured for a relative manifest path.
fn manifest_csv_path(
    chain: &str,
    manifest_path: &Path,
    research_root: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let manifest: HistoricalSourceManifest = serde_json::from_slice(
        &std::fs::read(manifest_path)
            .with_context(|| format!("read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse {}", manifest_path.display()))?;
    let Some(source) = manifest.sources.iter().find(|source| source.chain == chain) else {
        return Ok(None);
    };
    let Some(base) = manifest
        .source_repo_default_path
        .as_deref()
        .map(expand_home_literal)
        .or_else(|| research_root.map(Path::to_path_buf))
    else {
        return Ok(None);
    };
    Ok(Some(base.join(&source.csv_path)))
}

/// Expand a literal `$HOME/` prefix against `home_dir`; other values pass through
/// verbatim. Only the literal prefix is handled, not general shell expansion.
fn expand_home_literal(value: &str) -> PathBuf {
    value
        .strip_prefix("$HOME/")
        .map(|suffix| home_dir().join(suffix))
        .unwrap_or_else(|| PathBuf::from(value))
}

/// Home directory from `$HOME`, falling back to `.` so path building never panics.
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Drop duplicate paths in place while preserving first-seen order, so the
/// candidate precedence in `default_csv_candidates` survives deduplication.
fn dedup_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = std::collections::BTreeSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_recovered_source_has_a_height_column() {
        assert_eq!(HISTORICAL_CHAINS.len(), 20);
        for spec in HISTORICAL_CHAINS.iter() {
            assert!(spec.source_code.starts_with("auxpow:"));
            assert!(!spec.height_column.is_empty());
        }
        assert_eq!(
            historical_chain_spec("i0coin").unwrap().height_column,
            "child_height"
        );
        assert_eq!(
            historical_chain_spec("xaya").unwrap().height_column,
            "child_height"
        );
        assert_eq!(
            historical_chain_spec("vcash").unwrap().source_code,
            "auxpow:vcash"
        );
        assert_eq!(
            historical_chain_spec("vcash").unwrap().height_column,
            "vcash_height"
        );
        assert_eq!(
            historical_chain_spec("lyncoin").unwrap().height_column,
            "child_height"
        );
        assert_eq!(
            historical_chain_spec("sixeleven").unwrap().height_column,
            "child_height"
        );
        for chain in ["vcash", "lyncoin", "sixeleven"] {
            assert!(
                historical_chain_spec(chain)
                    .unwrap()
                    .requires_exact_child_fields(),
                "{chain} must require exact child fields"
            );
        }
        assert!(
            !historical_chain_spec("devcoin")
                .unwrap()
                .requires_exact_child_fields()
        );
    }

    #[test]
    fn historical_dataset_roots_use_merge_mining_env_names() {
        assert_eq!(RESEARCH_ROOT_ENV, "MERGE_MINING_RESEARCH_DIR");
        assert_eq!(ARCHIVE_ROOT_ENV, "MERGE_MINING_ARCHIVE_DIR");
    }

    #[test]
    fn height_columns_side_table_has_no_orphans_or_duplicates() {
        // The forward direction (every Historical/Partial registry row has a
        // height column) fails loudly via `height_column_for`'s panic the
        // moment HISTORICAL_CHAINS is built, which every test above forces.
        // This covers the reverse: a HEIGHT_COLUMNS entry whose chain no
        // longer exists in the registry (or is no longer Historical/Partial)
        // is dead configuration and must be removed, and duplicate keys
        // would make the lookup order-dependent.
        let mut seen = std::collections::HashSet::new();
        for (chain, _) in HEIGHT_COLUMNS {
            assert!(
                seen.insert(*chain),
                "duplicate HEIGHT_COLUMNS entry: {chain:?}"
            );
            assert!(
                SOURCE_REGISTRY.iter().any(|def| def.chain == *chain
                    && matches!(
                        def.lifecycle,
                        SourceLifecycle::Historical | SourceLifecycle::Partial
                    )),
                "orphaned HEIGHT_COLUMNS entry: {chain:?} has no Historical/Partial registry row"
            );
        }
    }
}
