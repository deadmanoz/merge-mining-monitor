//! The historical-import engine: stream a recovered-evidence CSV row by row and
//! persist each accepted row through the standard producer write path.
//!
//! This is the only I/O layer of the importer. It pairs the pure `csv_source`
//! parse with the live Core `ConfiguredParentClassifier` to decide each row,
//! then writes only `merge_mining_event` (plus, for RSK, the 1:1
//! `rsk_merge_mining_evidence` sidecar) via `mmm_store`, routing through
//! `read_model::mutation` so the derived tables
//! follow the same path as live producers. Per-row failures are tallied as
//! skips, never aborts; only setup failures and capture errors propagate.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{BlockKind, ConfiguredParentClassifier, ParentClassification};
use mmm_capture::btc_orphan::BtcOrphanVerdict;
use mmm_capture::capture::{
    ClassificationProof, ParentKind, ResolvedPoolAttributions, build_event_payload_from_evidence,
    now_epoch_seconds, resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{
    capture_historical_in_transaction, cascade_historical_import,
    rebuild_historical_source_health_in_transaction,
    reconcile_authoritative_historical_source_in_transaction,
};
use mmm_store::{
    EventWriteDisposition, upsert_merge_mining_event_with_attributions, upsert_pool_snapshot,
    write_elastos_capture_in_txn, write_rsk_capture_in_txn,
};
use tokio_postgres::{Client, GenericClient, Transaction};
use tracing::info;

use super::config::{HistoricalImportConfig, historical_chain_spec};
use super::csv_source::{
    CsvLayout, ImportCandidate, RelevanceSelection, SkipReason, SourceClassification,
    candidate_from_record,
};
use super::publication::{ArtifactPreflight, preflight_artifact};

/// Running tallies for one import, surfaced to the operator via `print`.
///
/// `rows_seen` counts every CSV record; `candidates` those that passed both the
/// parse gate and the decision gate; `ingested` those actually persisted. The
/// per-kind and per-attestation counters partition `ingested`, and `skipped`
/// maps each `SkipReason::as_str` to its drop count. Counters reconcile:
/// rows_seen = ingested + sum(skipped).
#[derive(Debug, Default, Clone)]
pub struct HistoricalImportSummary {
    pub expected_rows: u64,
    pub published_canonical: u64,
    pub published_stale: u64,
    pub published_stale_descendant: u64,
    pub published_strict_orphans: u64,
    pub published_weak_orphans: u64,
    pub rows_seen: u64,
    pub candidates: u64,
    pub ingested: u64,
    pub inserted: u64,
    pub updated: u64,
    pub promoted: u64,
    pub satisfied_by_existing_exact: u64,
    pub removed: u64,
    pub canonical: u64,
    pub stale: u64,
    pub strict_orphans: u64,
    pub weak_orphans: u64,
    /// Unknown rows whose PERSISTED `btc_orphan_class` is `excluded`, from
    /// either exclusion path: the known-stale membership gate or the
    /// wrong-difficulty-epoch check, both of which can override a local
    /// strict/weak verdict at write time.
    pub excluded: u64,
    /// Ingested unknown rows whose persisted class is still NULL (beyond the
    /// committed nBits table horizon, or a row reconciliation left without a
    /// block row).
    pub pending: u64,
    pub known_direct_branch_attestations: u64,
    pub known_descendant_branch_attestations: u64,
    pub skipped: BTreeMap<&'static str, u64>,
}

#[derive(Debug, Default, Clone)]
pub struct HistoricalImportAllSummary {
    pub chains: Vec<(String, HistoricalImportSummary)>,
    pub stale_branches_reconciled: u64,
}

/// The per-row verdict from `import_decision`, deciding which capture path (if
/// any) a candidate takes. `Skip` short-circuits before any DB write.
enum ImportDecision {
    /// Capture without a preset parent kind: the reconciler classifies later.
    CaptureUnclassified,
    /// Capture with a Core-attested classification already attached (boxed to
    /// keep the enum small).
    CapturePreclassified(Box<ParentClassification>),
    Skip(SkipReason),
}

/// Per-import shared state threaded into `import_candidate`, resolved once before
/// the row loop so each row reuses the same source id, classifier, pool resolver,
/// and slug-to-id map rather than recomputing them.
struct ImportContext<'a> {
    source_id: i64,
    chain: &'a str,
    classifier: &'a ConfiguredParentClassifier,
    resolver: &'a PoolResolver,
    pool_ids_by_slug: &'a HashMap<String, i64>,
}

struct ChainImportContext<'a> {
    source_id: i64,
    spec: &'a super::config::HistoricalChainSpec,
    config: &'a HistoricalImportConfig,
    classifier: &'a ConfiguredParentClassifier,
    resolver: &'a PoolResolver,
    pool_ids_by_slug: &'a HashMap<String, i64>,
    classifications: &'a mut HashMap<Vec<u8>, ParentClassification>,
}

impl HistoricalImportSummary {
    /// Increment the drop count for `reason`, keyed by its stable string.
    fn skip(&mut self, reason: SkipReason) {
        *self.skipped.entry(reason.as_str()).or_default() += 1;
    }

    /// Bump the per-kind and per-attestation counters after a successful persist.
    ///
    /// Buckets by the PERSISTED `block.kind` and `btc_orphan_class` read back
    /// after the write, never the incoming payload: capture reconciliation can
    /// retain an existing canonical/stale row when a later classification is
    /// unknown (`effective_classification`), and the read-model's known-stale
    /// membership gate can persist `excluded` for a row whose offline verdict
    /// was strict/weak. The summary reports what was stored. Call exactly once
    /// per persisted row, paired with `ingested += 1`.
    fn record_persisted(
        &mut self,
        persisted: Option<(BlockKind, Option<String>)>,
        candidate: &ImportCandidate,
    ) {
        match persisted {
            Some((BlockKind::Canonical, _)) => self.canonical += 1,
            Some((BlockKind::Stale, _)) => self.stale += 1,
            Some((BlockKind::Unknown, class)) => match class.as_deref() {
                Some("strict_btc_orphan") => self.strict_orphans += 1,
                Some("weak_btc_orphan") => self.weak_orphans += 1,
                Some("excluded") => self.excluded += 1,
                _ => self.pending += 1,
            },
            None => self.pending += 1,
        }
        match candidate.relevance_selection {
            Some(RelevanceSelection::KnownDirectStale) => {
                self.known_direct_branch_attestations += 1;
            }
            Some(RelevanceSelection::KnownStaleDescendant) => {
                self.known_descendant_branch_attestations += 1;
            }
            Some(RelevanceSelection::StrictBtcOrphan | RelevanceSelection::WeakBtcOrphan)
            | None => {}
        }
    }

    /// Print the one-line space-delimited summary to stdout (the operator-facing
    /// report; the `skipped` map renders as comma-joined `reason:count` pairs).
    pub fn print(&self) {
        println!(
            "historical import: expected_rows={} published_canonical={} published_stale={} published_stale_descendant={} published_strict_btc_orphan={} published_weak_btc_orphan={} rows_seen={} candidates={} ingested={} inserted={} updated={} promoted={} satisfied_by_existing_exact={} removed={} canonical={} stale={} strict_btc_orphan={} weak_btc_orphan={} excluded={} pending={} known_direct_branch_attestations={} known_descendant_branch_attestations={} skipped={}",
            self.expected_rows,
            self.published_canonical,
            self.published_stale,
            self.published_stale_descendant,
            self.published_strict_orphans,
            self.published_weak_orphans,
            self.rows_seen,
            self.candidates,
            self.ingested,
            self.inserted,
            self.updated,
            self.promoted,
            self.satisfied_by_existing_exact,
            self.removed,
            self.canonical,
            self.stale,
            self.strict_orphans,
            self.weak_orphans,
            self.excluded,
            self.pending,
            self.known_direct_branch_attestations,
            self.known_descendant_branch_attestations,
            self.skipped
                .iter()
                .map(|(reason, count)| format!("{reason}:{count}"))
                .collect::<Vec<_>>()
                .join(",")
        );
    }
}

impl HistoricalImportAllSummary {
    pub fn print(&self) {
        for (chain, summary) in &self.chains {
            print!("chain={chain} ");
            summary.print();
        }
        println!(
            "historical import-all: chains={} expected_rows={} ingested={} inserted={} updated={} promoted={} satisfied_by_existing_exact={} removed={} stale_branches_reconciled={}",
            self.chains.len(),
            self.chains
                .iter()
                .map(|(_, value)| value.expected_rows)
                .sum::<u64>(),
            self.chains
                .iter()
                .map(|(_, value)| value.ingested)
                .sum::<u64>(),
            self.chains
                .iter()
                .map(|(_, value)| value.inserted)
                .sum::<u64>(),
            self.chains
                .iter()
                .map(|(_, value)| value.updated)
                .sum::<u64>(),
            self.chains
                .iter()
                .map(|(_, value)| value.promoted)
                .sum::<u64>(),
            self.chains
                .iter()
                .map(|(_, value)| value.satisfied_by_existing_exact)
                .sum::<u64>(),
            self.chains
                .iter()
                .map(|(_, value)| value.removed)
                .sum::<u64>(),
            self.stale_branches_reconciled,
        );
    }
}

/// Stream the configured CSV and persist accepted rows, returning the tallies.
///
/// Refuses to run without a live classifier unless `--allow-unclassified` is set
/// (the orphan-import safety guard). Resolves the `source_id`, upserts the
/// embedded pool snapshot, validates and classifies the complete input before
/// opening the chain transaction, then iterates rows to capture them. Honors
/// `--limit` (caps `ingested`) and logs progress every `batch_size` ingests.
/// Setup, validation, and capture errors propagate.
pub async fn run_historical_import(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
) -> Result<HistoricalImportSummary> {
    let mut classifications = HashMap::new();
    run_historical_import_with_cache(client, classifier, config, &mut classifications, None).await
}

async fn run_historical_import_with_cache(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
    preflighted_artifact: Option<ArtifactPreflight>,
) -> Result<HistoricalImportSummary> {
    let spec = historical_chain_spec(&config.chain)
        .ok_or_else(|| anyhow::anyhow!("unsupported published chain {:?}", config.chain))?;
    let candidates_prepared = preflighted_artifact.is_some();
    let artifact = match preflighted_artifact {
        Some(artifact) => artifact,
        None => preflight_artifact(config, spec)?,
    };
    if !candidates_prepared {
        let import_configs = (!matches!(
            spec.lifecycle,
            mmm_capture::source_registry::SourceLifecycle::Surveyed
        ))
        .then_some(config);
        ensure_import_environment(client, classifier, import_configs.as_slice()).await?;
        preflight_and_classify_candidates(
            client,
            classifier,
            config,
            spec,
            artifact.row_count,
            classifications,
        )
        .await?;
    }
    if spec.lifecycle == mmm_capture::source_registry::SourceLifecycle::Surveyed {
        return Ok(HistoricalImportSummary {
            expected_rows: artifact.row_count,
            rows_seen: artifact.row_count,
            ..HistoricalImportSummary::default()
        });
    }
    let source_id = mmm_store::get_source_id(client, spec.source_code).await?;
    let resolver = PoolResolver::from_default_snapshot().context("load embedded pool snapshot")?;

    let txn = client
        .transaction()
        .await
        .with_context(|| format!("begin {} historical chain transaction", spec.chain))?;
    let pool_ids_by_slug = upsert_pool_snapshot(&txn, resolver.snapshot()).await?;
    let (mut summary, mut changed_hashes) = import_rows_in_transaction(
        &txn,
        &mut ChainImportContext {
            source_id,
            spec,
            config,
            classifier,
            resolver: &resolver,
            pool_ids_by_slug: &pool_ids_by_slug,
            classifications,
        },
        artifact,
    )
    .await?;

    if config.limit.is_none() && spec.is_authoritative() {
        let (removed, hashes) = reconcile_authoritative_historical_source_in_transaction(
            &txn,
            source_id,
            super::config::PINNED_RESEARCH_COMMIT,
            spec.chain,
            classifier,
        )
        .await?;
        summary.removed = removed;
        changed_hashes.extend(hashes);
    }
    rebuild_historical_source_health_in_transaction(&txn).await?;
    txn.commit()
        .await
        .with_context(|| format!("commit {} historical chain transaction", spec.chain))?;
    changed_hashes.sort();
    changed_hashes.dedup();
    cascade_historical_import(client, classifier, changed_hashes).await?;
    Ok(summary)
}

async fn import_rows_in_transaction(
    txn: &Transaction<'_>,
    context: &mut ChainImportContext<'_>,
    artifact: ArtifactPreflight,
) -> Result<(HistoricalImportSummary, Vec<Vec<u8>>)> {
    let (mut reader, layout) = open_candidate_reader(context.config, context.spec)?;
    let mut summary = HistoricalImportSummary {
        expected_rows: artifact.row_count,
        published_canonical: artifact.counts.canonical,
        published_stale: artifact.counts.stale,
        published_stale_descendant: artifact.counts.stale_descendant,
        published_strict_orphans: artifact.counts.strict_btc_orphan,
        published_weak_orphans: artifact.counts.weak_btc_orphan,
        ..HistoricalImportSummary::default()
    };
    let mut changed_hashes = Vec::new();

    for record in reader.records() {
        summary.rows_seen += 1;
        let record = match record {
            Ok(record) => record,
            Err(_) => {
                summary.skip(SkipReason::Malformed);
                continue;
            }
        };
        let candidate = match candidate_from_record(context.spec, &layout, &record) {
            Ok(candidate) => candidate,
            Err(reason) => {
                summary.skip(reason);
                continue;
            }
        };
        let decision = import_decision(
            txn,
            context.classifier,
            context.config,
            &candidate,
            context.classifications,
        )
        .await?;
        if let ImportDecision::Skip(reason) = decision {
            summary.skip(reason);
            continue;
        }
        summary.candidates += 1;
        import_candidate(
            txn,
            &ImportContext {
                source_id: context.source_id,
                chain: context.spec.chain,
                classifier: context.classifier,
                resolver: context.resolver,
                pool_ids_by_slug: context.pool_ids_by_slug,
            },
            &mut summary,
            candidate,
            decision,
        )
        .await
        .map(|hashes| changed_hashes.extend(hashes))?;
        if let Some(limit) = context.config.limit
            && summary.ingested as usize >= limit
        {
            break;
        }
        if summary.ingested > 0
            && summary
                .ingested
                .is_multiple_of(context.config.batch_size as u64)
        {
            info!(
                chain = context.spec.chain,
                ingested = summary.ingested,
                rows_seen = summary.rows_seen,
                "historical import progress"
            );
        }
    }
    Ok((summary, changed_hashes))
}

/// Preflight the complete pinned publication, then import every event artifact
/// in deterministic chain order with one shared parent-classification cache.
pub async fn run_historical_import_all(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &super::config::HistoricalImportAllConfig,
) -> Result<HistoricalImportAllSummary> {
    let configs = config.chain_configs()?;
    run_historical_import_configs(client, classifier, configs).await
}

async fn run_historical_import_configs(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    mut configs: Vec<HistoricalImportConfig>,
) -> Result<HistoricalImportAllSummary> {
    configs.sort_by(|left, right| left.chain.cmp(&right.chain));
    let mut preflighted_artifacts = Vec::with_capacity(configs.len());
    for chain_config in &configs {
        let spec = historical_chain_spec(&chain_config.chain)
            .expect("chain configs are built from the source registry");
        let artifact = preflight_artifact(chain_config, spec)?;
        preflighted_artifacts.push(artifact);
    }

    let mut classifications = HashMap::new();
    let import_configs = configs
        .iter()
        .filter(|config| {
            historical_chain_spec(&config.chain).is_some_and(|spec| {
                spec.lifecycle != mmm_capture::source_registry::SourceLifecycle::Surveyed
            })
        })
        .collect::<Vec<_>>();
    ensure_import_environment(client, classifier, &import_configs).await?;
    for (chain_config, artifact) in configs.iter().zip(&preflighted_artifacts) {
        let spec = historical_chain_spec(&chain_config.chain)
            .expect("chain configs are built from the source registry");
        preflight_and_classify_candidates(
            client,
            classifier,
            chain_config,
            spec,
            artifact.row_count,
            &mut classifications,
        )
        .await?;
    }

    let mut summary = HistoricalImportAllSummary::default();
    for (index, (chain_config, artifact)) in configs.iter().zip(preflighted_artifacts).enumerate() {
        info!(
            chain = %chain_config.chain,
            current = index + 1,
            total = configs.len(),
            "importing historical publication chain"
        );
        let chain_summary = run_historical_import_with_cache(
            client,
            classifier,
            chain_config,
            &mut classifications,
            Some(artifact),
        )
        .await?;
        summary
            .chains
            .push((chain_config.chain.clone(), chain_summary));
    }
    summary.stale_branches_reconciled =
        reconcile_published_stale_branches(client, classifier).await?;
    Ok(summary)
}

/// Exercise the production multi-chain orchestration with explicit normalized
/// fixture configs. Only exposed to the database integration test feature.
#[cfg(feature = "db-integration")]
#[doc(hidden)]
pub async fn run_historical_import_configs_for_test(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    configs: Vec<HistoricalImportConfig>,
) -> Result<HistoricalImportAllSummary> {
    run_historical_import_configs(client, classifier, configs).await
}

/// Exercise manifest-to-chain config resolution plus the production
/// manifest-backed import path without requiring a full 27-chain fixture.
#[cfg(feature = "db-integration")]
#[doc(hidden)]
pub async fn run_manifest_historical_import_for_test(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &super::config::HistoricalImportAllConfig,
    chain: &str,
) -> Result<HistoricalImportSummary> {
    let chain_config = config
        .chain_configs()?
        .into_iter()
        .find(|candidate| candidate.chain == chain)
        .ok_or_else(|| anyhow::anyhow!("manifest has no event artifact for {chain:?}"))?;
    run_historical_import(client, classifier, &chain_config).await
}

async fn reconcile_published_stale_branches(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
) -> Result<u64> {
    let rows = client
        .query(
            "SELECT DISTINCT ON (e.btc_parent_header_hash) e.id \
             FROM merge_mining_event e \
             JOIN historical_event_provenance p ON p.event_id = e.id \
             LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
             WHERE e.revoked_at IS NULL \
               AND p.relevance_reason IN ('valid_direct_stale', 'valid_stale_descendant') \
               AND (b.kind IS NULL OR b.kind = 'unknown') \
             ORDER BY e.btc_parent_header_hash, e.child_height NULLS LAST, e.id",
            &[],
        )
        .await
        .context("load published stale branches for targeted reconciliation")?;
    let mut reconciled = 0_u64;
    for row in rows {
        let event_id: i64 = row.get(0);
        mmm_read_model::reconcile_from_merge_mining_event(client, event_id, classifier, None)
            .await
            .with_context(|| format!("reconcile published stale-branch event {event_id}"))?;
        reconciled += 1;
    }
    Ok(reconciled)
}

/// Validate every normalized candidate and resolve parent classifications before
/// the chain transaction begins.
///
/// Positive Core classifications and Core-absence results are cached by parent
/// hash so repeated observations across a chain do not repeat RPC work. The
/// mutation pass reuses this cache and therefore performs no Bitcoin Core calls
/// while holding the chain transaction open. Parsing and classification share
/// one stream so the complete artifact is not reopened for two identical parse
/// passes.
async fn preflight_and_classify_candidates<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    spec: &super::config::HistoricalChainSpec,
    expected_rows: u64,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    let (mut reader, layout) = open_candidate_reader(config, spec)?;
    let mut rows = 0_u64;
    for (offset, record) in reader.records().enumerate() {
        let record = record.with_context(|| {
            format!(
                "parse normalized artifact {} row {}",
                config.csv_path.display(),
                offset + 2
            )
        })?;
        let candidate = candidate_from_record(spec, &layout, &record).map_err(|reason| {
            anyhow::anyhow!(
                "normalized artifact {} row {} failed {}",
                config.csv_path.display(),
                offset + 2,
                reason.as_str()
            )
        })?;
        if classifier.is_enabled() {
            let decision =
                import_decision(client, classifier, config, &candidate, classifications).await?;
            if config.manifest_path.is_some()
                && let ImportDecision::Skip(reason) = decision
            {
                bail!(
                    "published artifact {} row {} would be skipped as {}",
                    config.csv_path.display(),
                    offset + 2,
                    reason.as_str()
                );
            }
        }
        rows += 1;
    }
    if rows != expected_rows {
        bail!(
            "normalized artifact {} changed during preflight: expected {expected_rows} rows, parsed {rows}",
            config.csv_path.display()
        );
    }
    Ok(())
}

/// Validate the shared classifier and known-stale prerequisites once for the
/// set of non-surveyed chains that will be imported.
async fn ensure_import_environment(
    client: &Client,
    classifier: &ConfiguredParentClassifier,
    configs: &[&HistoricalImportConfig],
) -> Result<()> {
    if configs.is_empty() {
        return Ok(());
    }
    if !classifier.is_enabled() && configs.iter().any(|config| !config.allow_unclassified) {
        bail!(
            "BITCOIN_RPC_URL is required for historical import unless --allow-unclassified is passed"
        );
    }
    // An empty membership means the orphan-class gate cannot exclude a known
    // stale. Refuse by default; the flag is only for disposable diagnostics.
    let known_stale_count = mmm_store::count_known_stale_blocks(client).await?;
    if known_stale_count == 0
        && configs
            .iter()
            .any(|config| !config.allow_empty_known_stales)
    {
        bail!(
            "known_stale_block is empty: import the upstream stale-blocks dataset with \
             import-known-stales before historical import, or pass --allow-empty-known-stales to \
             run without known-stale exclusion (known stales may be mislabelled strict/weak)"
        );
    }
    info!(
        chains = configs.len(),
        known_stale_count, "starting historical import with known-stale membership"
    );
    Ok(())
}

fn open_candidate_reader(
    config: &HistoricalImportConfig,
    spec: &super::config::HistoricalChainSpec,
) -> Result<(csv::Reader<std::fs::File>, CsvLayout)> {
    let file = std::fs::File::open(&config.csv_path)
        .with_context(|| format!("open historical CSV {}", config.csv_path.display()))?;
    let mut reader = csv::Reader::from_reader(file);
    let layout = CsvLayout::new(
        reader.headers().context("read historical CSV header")?,
        spec,
    )?;
    Ok((reader, layout))
}

/// Decide a candidate's fate, the layer where live Core classification meets the
/// dataset's own labels.
///
/// With no classifier (`--allow-unclassified`): non-unknown rows capture
/// unclassified, unknown rows are skipped. With a classifier: Canonical/Stale
/// capture preclassified; `Near` is skipped; `Unknown` first holds any
/// known-branch selection as an excluded unknown pending targeted
/// reconciliation (an externally attested stale-branch member is never
/// persisted as a BTC orphan), then captures preclassified only when
/// Core-absence is attested, the dataset
/// classified it as unknown, and the local orphan verdict is Strict/Weak;
/// everything else,
/// `Unclassified`. Reads parent preflight by prev_blockhash in
/// `to_byte_array` (wire) order.
async fn import_decision<C: GenericClient>(
    client: &C,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    candidate: &ImportCandidate,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<ImportDecision> {
    if !classifier.is_enabled() {
        if config.allow_unclassified
            && candidate.source_classification != SourceClassification::Unknown
        {
            return Ok(ImportDecision::CaptureUnclassified);
        }
        return Ok(ImportDecision::Skip(SkipReason::Unclassified));
    }
    let parent_hash = candidate
        .evidence
        .btc_parent_header
        .block_hash()
        .to_byte_array()
        .to_vec();
    let prev_hash = candidate
        .evidence
        .btc_parent_header
        .prev_blockhash
        .to_byte_array()
        .to_vec();
    let classification = if let Some(classification) = classifications.get(&parent_hash) {
        classification.clone()
    } else {
        let preflight = mmm_read_model::load_parent_preflight(client, &prev_hash).await?;
        let classification = classifier
            .classify_parent(&candidate.evidence.btc_parent_header, preflight)
            .await
            .with_context(|| {
                format!(
                    "preclassify historical parent {}",
                    candidate.btc_parent_display_hash
                )
            })?;
        classifications.insert(parent_hash, classification.clone());
        classification
    };
    match classification.kind {
        ParentKind::Canonical
            if candidate.source_classification == SourceClassification::Canonical =>
        {
            Ok(ImportDecision::CapturePreclassified(Box::new(
                classification,
            )))
        }
        ParentKind::Stale
            if matches!(
                candidate.source_classification,
                SourceClassification::Stale | SourceClassification::StaleDescendant
            ) || matches!(
                candidate.relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) =>
        {
            Ok(ImportDecision::CapturePreclassified(Box::new(
                classification,
            )))
        }
        ParentKind::Canonical | ParentKind::Stale => {
            Ok(ImportDecision::Skip(SkipReason::ClassificationMismatch))
        }
        ParentKind::Near => Ok(ImportDecision::Skip(SkipReason::Unclassified)),
        ParentKind::Unknown => {
            if matches!(
                candidate.relevance_selection,
                Some(
                    RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant
                )
            ) {
                let mut known_branch = classification;
                known_branch.core_absence_attested = false;
                Ok(ImportDecision::CapturePreclassified(Box::new(known_branch)))
            } else if !classification.core_absence_attested
                || candidate.source_classification != SourceClassification::Unknown
            {
                Ok(ImportDecision::Skip(SkipReason::Unclassified))
            } else if matches!(
                candidate.orphan_verdict,
                Some(BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak)
            ) {
                Ok(ImportDecision::CapturePreclassified(Box::new(
                    classification,
                )))
            } else {
                Ok(ImportDecision::Skip(SkipReason::Unclassified))
            }
        }
    }
}

/// Persist one decided candidate through the shared producer write path.
///
/// Resolves pool attribution from the parent coinbase, builds the standard event
/// payload, then writes and reconciles it inside the caller-owned chain
/// transaction. `Skip` is unreachable here by construction.
async fn import_candidate(
    txn: &Transaction<'_>,
    context: &ImportContext<'_>,
    summary: &mut HistoricalImportSummary,
    candidate: ImportCandidate,
    decision: ImportDecision,
) -> Result<Vec<Vec<u8>>> {
    let attributions = candidate
        .evidence
        .btc_parent_coinbase_script
        .as_deref()
        .and_then(|script| {
            resolve_parent_pool_attribution_from_coinbase(
                script,
                &[],
                context.resolver,
                context.pool_ids_by_slug,
            )
        })
        .into_iter()
        .collect();
    let pool_attributions = ResolvedPoolAttributions { attributions };
    let mut payload = build_event_payload_from_evidence(
        candidate.evidence.clone(),
        pool_attributions,
        ClassificationProof::default(),
        now_epoch_seconds()?,
    )?;
    payload.historical_provenance = Some(candidate.historical_provenance.clone());
    // RSK rows carry a 1:1 `rsk_merge_mining_evidence` payload that must land
    // in the same transaction as the event (mmm-api hard-errors on any
    // `auxpow:rsk` event without its sidecar row). Elastos routes through its
    // reactivating writer so a conflict with a live row auto-revoked
    // `ELASTOS_REVOKE_NON_BTC` clears that reversible, evidence-based
    // revocation, exactly as a live re-Valid capture would -- but ONLY on the
    // Core-attested (preclassified) path: an `--allow-unclassified` run gates
    // rows on nothing stronger than the header's own encoded target, which is
    // weaker than the Elastos producer's validity gate, so it must never
    // resurrect a revoked row. Sticky and manual revocations stay untouched
    // either way. Hathor deliberately stays on the generic upsert: its
    // reversible revocations (voided/superseded) track CURRENT child-DAG
    // state that a historical observation must not resurrect, and its writer
    // requires the RFC 0006 sidecar the exports cannot supply. Every other
    // chain writes the event alone. `pool_identity_id` stays NULL here --
    // the `reclassify-pools` late-fill path resolves it from the registry.
    let rsk_evidence = candidate.rsk_evidence.as_ref();
    let use_elastos_writer =
        context.chain == "elastos" && matches!(&decision, ImportDecision::CapturePreclassified(_));
    let upsert = async |txn: &tokio_postgres::Transaction<'_>,
                        source_id: i64,
                        payload: &mmm_capture::capture::MergeMiningEventPayload| {
        match rsk_evidence {
            Some(evidence) => write_rsk_capture_in_txn(txn, source_id, payload, evidence).await,
            None if use_elastos_writer => {
                write_elastos_capture_in_txn(txn, source_id, payload).await
            }
            None => upsert_merge_mining_event_with_attributions(txn, source_id, payload).await,
        }
    };
    let parent_classification = match decision {
        ImportDecision::CaptureUnclassified => None,
        ImportDecision::CapturePreclassified(parent_classification) => Some(*parent_classification),
        ImportDecision::Skip(_) => unreachable!("skip decisions do not reach import_candidate"),
    };
    let (outcome, changed_hashes) = capture_historical_in_transaction(
        txn,
        context.source_id,
        context.classifier,
        &mut payload,
        parent_classification,
        upsert,
    )
    .await
    .with_context(|| {
        format!(
            "capture historical parent {}",
            candidate.btc_parent_display_hash
        )
    })?;
    summary.ingested += 1;
    match outcome.disposition {
        EventWriteDisposition::Inserted => summary.inserted += 1,
        EventWriteDisposition::Updated => summary.updated += 1,
        EventWriteDisposition::Promoted => summary.promoted += 1,
        EventWriteDisposition::SatisfiedByExistingExact => {
            summary.satisfied_by_existing_exact += 1;
        }
    }
    let parent_hash = candidate
        .evidence
        .btc_parent_header
        .block_hash()
        .to_byte_array()
        .to_vec();
    let persisted = mmm_read_model::load_persisted_kind_and_orphan_class(txn, &parent_hash)
        .await
        .context("read back persisted block kind and orphan class for the import summary")?;
    summary.record_persisted(persisted, &candidate);
    Ok(changed_hashes)
}
