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

use anyhow::{Context, Result, bail, ensure};
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{BlockKind, ConfiguredParentClassifier, ParentClassification};
use mmm_capture::capture::{
    ClassificationProof, ResolvedPoolAttributions, build_event_payload_from_evidence,
    now_epoch_seconds, resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::nbits_table::NbitsTable;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{
    clear_authoritative_historical_provenance_in_transaction,
    drain_historical_reconcile_queue_with_nbits_table, enqueue_historical_parent_reconcile,
    invalidate_source_health_in_transaction, rebuild_historical_source_health,
    reconcile_authoritative_historical_source_in_transaction, write_historical_base_in_transaction,
};
use mmm_store::{
    EventWriteDisposition, upsert_merge_mining_event_with_attributions, upsert_pool_snapshot,
    write_elastos_capture_in_txn, write_rsk_capture_in_txn,
};
use tokio_postgres::{Client, Transaction};
use tracing::info;

use super::config::{HistoricalImportConfig, historical_chain_spec};
use super::csv_source::{ImportCandidate, RelevanceSelection, SkipReason, candidate_from_record};
use super::preclassify::{ImportDecision, import_decision, preflight_and_classify_candidates};
#[cfg(feature = "db-integration")]
use super::publication::inspect_error_observation_csv;
use super::publication::{
    ArtifactPreflight, ErrorObservationPreflight, PreparedPublication, preflight_artifact,
    preflight_publication,
};

mod error_observations;
#[cfg(feature = "db-integration")]
use error_observations::import_error_observations;
use error_observations::preflight_error_observations;
mod publication_state;
use publication_state::{ImportPlan, plan_publication_import};
mod write_plan;
use write_plan::{PlannedWrite, write_planned_imports};

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
    pub error_blocks: u64,
    pub error_parents: u64,
    /// Unknown rows whose PERSISTED `btc_orphan_class` is `excluded`, from
    /// either exclusion path: the known-stale membership gate or the
    /// wrong-difficulty-epoch check, both of which can override a local
    /// strict/weak verdict at write time.
    pub excluded: u64,
    /// Ingested unknown rows whose persisted class is still NULL (beyond the
    /// persisted Core-cache horizon, or a row reconciliation left without a
    /// block row).
    pub pending: u64,
    pub known_direct_branch_attestations: u64,
    pub known_descendant_branch_attestations: u64,
    pub skipped: BTreeMap<&'static str, u64>,
}

#[derive(Debug, Default, Clone)]
pub struct HistoricalImportAllSummary {
    pub chains: Vec<(String, HistoricalImportSummary)>,
    pub error_observations: Option<HistoricalImportSummary>,
    pub stale_branches_reconciled: u64,
    pub skipped_matching_state: u64,
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
    nbits_table: &'a NbitsTable,
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
    fn record_persisted(&mut self, persisted: Option<(BlockKind, Option<String>)>, count: u64) {
        match persisted {
            Some((BlockKind::Canonical, _)) => self.canonical += count,
            Some((BlockKind::Stale, _)) => self.stale += count,
            Some((BlockKind::ErrorBlock, _)) => {
                self.error_blocks += count;
            }
            Some((BlockKind::Unknown, class)) => match class.as_deref() {
                Some("strict_btc_orphan") => self.strict_orphans += count,
                Some("weak_btc_orphan") => self.weak_orphans += count,
                Some("excluded") => self.excluded += count,
                _ => self.pending += count,
            },
            None => self.pending += count,
        }
    }

    fn record_attestation(&mut self, candidate: &ImportCandidate) {
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
            "historical import: expected_rows={} published_canonical={} published_stale={} published_stale_descendant={} published_strict_btc_orphan={} published_weak_btc_orphan={} rows_seen={} candidates={} ingested={} inserted={} updated={} promoted={} satisfied_by_existing_exact={} removed={} canonical={} stale={} error_block={} error_parents={} strict_btc_orphan={} weak_btc_orphan={} excluded={} pending={} known_direct_branch_attestations={} known_descendant_branch_attestations={} skipped={}",
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
            self.error_blocks,
            self.error_parents,
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
        if let Some(summary) = &self.error_observations {
            print!("chain=error-block-observations ");
            summary.print();
        }
        println!(
            "historical import-all: chains={} error_observations={} error_parents={} expected_rows={} ingested={} inserted={} updated={} promoted={} satisfied_by_existing_exact={} removed={} stale_branches_reconciled={} skipped_matching_state={}",
            self.chains.len(),
            self.error_observations
                .as_ref()
                .map_or(0, |value| value.ingested),
            self.error_observations
                .as_ref()
                .map_or(0, |value| value.error_parents),
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
            self.skipped_matching_state,
        );
    }
}

/// Stream the configured CSV and persist accepted rows, returning the tallies.
///
/// Requires a live Core classifier (the orphan-import safety guard). Resolves
/// the `source_id`, upserts the embedded pool snapshot, validates and
/// classifies the complete input before opening the chain transaction, then
/// iterates rows to capture them. Honors `--limit` (caps `ingested`) and logs
/// progress every `batch_size` ingests.
/// Setup, validation, and capture errors propagate.
pub async fn run_historical_import(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
) -> Result<HistoricalImportSummary> {
    let mut classifications = HashMap::new();
    mmm_store::lock_bitcoin_core_header_cache(client).await?;
    let result = run_historical_import_with_cache(
        client,
        classifier,
        config,
        &mut classifications,
        None,
        true,
        None,
    )
    .await;
    mmm_store::finish_bitcoin_core_header_cache_operation(client, result).await
}

pub(super) async fn run_historical_import_with_cache(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &HistoricalImportConfig,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
    preflighted_artifact: Option<ArtifactPreflight>,
    rebuild_source_health_after_import: bool,
    shared_nbits_table: Option<&NbitsTable>,
) -> Result<HistoricalImportSummary> {
    if config.limit == Some(0) {
        bail!("--limit must be greater than zero");
    }
    let spec = historical_chain_spec(&config.chain)
        .ok_or_else(|| anyhow::anyhow!("unsupported published chain {:?}", config.chain))?;
    let candidates_prepared = preflighted_artifact.is_some();
    let mut artifact = match preflighted_artifact {
        Some(artifact) => artifact,
        None => preflight_artifact(config, spec)?,
    };
    let loaded_nbits_table = match shared_nbits_table {
        Some(_) => None,
        None => Some(mmm_store::load_bitcoin_core_nbits_table(client).await?),
    };
    let nbits_table = shared_nbits_table
        .or(loaded_nbits_table.as_ref())
        .expect("a historical import always has a Core nBits table");
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
            &mut artifact,
            classifications,
            nbits_table,
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
    if config.is_authoritative_snapshot(spec) {
        clear_authoritative_historical_provenance_in_transaction(&txn, spec.chain).await?;
    }
    let (mut summary, parent_counts) = import_rows_in_transaction(
        &txn,
        &mut ChainImportContext {
            source_id,
            spec,
            config,
            classifier,
            resolver: &resolver,
            pool_ids_by_slug: &pool_ids_by_slug,
            classifications,
            nbits_table,
        },
        artifact,
    )
    .await?;

    commit_chain_import_transaction(txn, spec, config, source_id, &mut summary).await?;
    drain_historical_reconcile_queue_with_nbits_table(
        client,
        classifier,
        classifications,
        Some(nbits_table),
    )
    .await?;
    record_persisted_parent_counts(client, &mut summary, parent_counts).await?;
    if rebuild_source_health_after_import {
        rebuild_historical_source_health(client).await?;
    }
    Ok(summary)
}

async fn commit_chain_import_transaction(
    txn: Transaction<'_>,
    spec: &super::config::HistoricalChainSpec,
    config: &HistoricalImportConfig,
    source_id: i64,
    summary: &mut HistoricalImportSummary,
) -> Result<()> {
    if config.is_authoritative_snapshot(spec) {
        summary.removed = reconcile_authoritative_historical_source_in_transaction(
            &txn,
            source_id,
            super::config::PINNED_RESEARCH_COMMIT.as_str(),
            spec.chain,
        )
        .await?;
    }
    invalidate_source_health_in_transaction(&txn).await?;
    txn.commit()
        .await
        .with_context(|| format!("commit {} historical chain transaction", spec.chain))?;
    Ok(())
}

async fn import_rows_in_transaction(
    txn: &Transaction<'_>,
    context: &mut ChainImportContext<'_>,
    mut artifact: ArtifactPreflight,
) -> Result<(HistoricalImportSummary, HashMap<Vec<u8>, u64>)> {
    let mut summary = HistoricalImportSummary {
        expected_rows: artifact.row_count,
        published_canonical: artifact.counts.canonical,
        published_stale: artifact.counts.stale,
        published_stale_descendant: artifact.counts.stale_descendant,
        published_strict_orphans: artifact.counts.strict_btc_orphan,
        published_weak_orphans: artifact.counts.weak_btc_orphan,
        ..HistoricalImportSummary::default()
    };
    let mut parent_counts = HashMap::new();
    let expected_parent_only_rows = artifact.parent_only_rows;
    let (mut reader, layout) = artifact.open_reader(context.spec)?;

    for record in reader.records() {
        summary.rows_seen += 1;
        let record = match record {
            Ok(record) => record,
            Err(_) => {
                summary.skip(SkipReason::Malformed);
                continue;
            }
        };
        let candidate = match candidate_from_record(
            context.spec,
            &layout,
            &record,
            context.config.publication_ref(),
            Some(context.nbits_table),
        ) {
            Ok(candidate) => candidate,
            Err(reason) => {
                summary.skip(reason);
                continue;
            }
        };
        let decision =
            import_decision(txn, context.classifier, &candidate, context.classifications).await?;
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
        .map(|parent_hash| *parent_counts.entry(parent_hash).or_default() += 1)?;
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
    if context.config.limit.is_none()
        && let Some(expected) = expected_parent_only_rows
    {
        let actual = summary
            .skipped
            .get(SkipReason::MissingChildIdentity.as_str())
            .copied()
            .unwrap_or_default();
        ensure!(
            actual == expected,
            "{} import skipped {} parent-only rows, expected {}",
            context.spec.chain,
            actual,
            expected
        );
    }
    Ok((summary, parent_counts))
}

/// Preflight the complete pinned publication, then import every event artifact
/// in deterministic chain order with one shared parent-classification cache.
pub async fn run_historical_import_all(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    config: &super::config::HistoricalImportAllConfig,
) -> Result<HistoricalImportAllSummary> {
    let PreparedPublication {
        configs,
        event_artifacts,
        error_observations,
    } = preflight_publication(config)?;
    run_preflighted_historical_import_configs(
        client,
        classifier,
        configs,
        event_artifacts,
        Some(error_observations),
    )
    .await
}

#[cfg(feature = "db-integration")]
async fn run_historical_import_configs(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    mut configs: Vec<HistoricalImportConfig>,
    error_observations: Option<ErrorObservationPreflight>,
) -> Result<HistoricalImportAllSummary> {
    configs.sort_by(|left, right| left.chain.cmp(&right.chain));
    let mut preflighted_artifacts = Vec::with_capacity(configs.len());
    for chain_config in &configs {
        let spec = historical_chain_spec(&chain_config.chain)
            .expect("chain configs are built from the source registry");
        let artifact = preflight_artifact(chain_config, spec)?;
        preflighted_artifacts.push(artifact);
    }
    run_preflighted_historical_import_configs(
        client,
        classifier,
        configs,
        preflighted_artifacts,
        error_observations,
    )
    .await
}

async fn run_preflighted_historical_import_configs(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    configs: Vec<HistoricalImportConfig>,
    preflighted_artifacts: Vec<ArtifactPreflight>,
    error_observations: Option<ErrorObservationPreflight>,
) -> Result<HistoricalImportAllSummary> {
    ensure!(
        configs.len() == preflighted_artifacts.len()
            && configs
                .iter()
                .zip(&preflighted_artifacts)
                .all(|(config, artifact)| config.chain == artifact.chain),
        "preflighted artifact order does not match import configs"
    );
    let publication_backed = configs.iter().all(|config| config.manifest_path.is_some());
    let plan = if publication_backed {
        Some(
            plan_publication_import(
                client,
                &configs,
                &preflighted_artifacts,
                error_observations.as_ref(),
            )
            .await?,
        )
    } else {
        None
    };
    if let Some(plan) = &plan
        && plan.work_chain.iter().all(|work| !work)
        && !plan.work_error_observations
        && !plan.needs_finalization
    {
        return Ok(HistoricalImportAllSummary {
            skipped_matching_state: plan.skipped_matching_state,
            ..HistoricalImportAllSummary::default()
        });
    }

    mmm_store::lock_bitcoin_core_header_cache(client).await?;
    let result = run_historical_import_configs_locked(
        client,
        classifier,
        configs,
        preflighted_artifacts,
        error_observations,
        plan,
    )
    .await;
    mmm_store::finish_bitcoin_core_header_cache_operation(client, result).await
}

async fn run_historical_import_configs_locked(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    configs: Vec<HistoricalImportConfig>,
    mut preflighted_artifacts: Vec<ArtifactPreflight>,
    mut error_observations: Option<ErrorObservationPreflight>,
    plan: Option<ImportPlan>,
) -> Result<HistoricalImportAllSummary> {
    let mut classifications = HashMap::new();
    let work_chain = |index: usize| plan.as_ref().is_none_or(|plan| plan.work_chain[index]);
    let work_error_observations = plan
        .as_ref()
        .is_none_or(|plan| plan.work_error_observations);
    let import_configs = configs
        .iter()
        .enumerate()
        .filter(|(index, config)| {
            work_chain(*index)
                && historical_chain_spec(&config.chain).is_some_and(|spec| {
                    spec.lifecycle != mmm_capture::source_registry::SourceLifecycle::Surveyed
                })
        })
        .map(|(_, config)| config)
        .collect::<Vec<_>>();
    let needs_finalization = plan.as_ref().is_some_and(|plan| plan.needs_finalization);
    if import_configs.is_empty() && !work_error_observations && !needs_finalization {
        return Ok(HistoricalImportAllSummary {
            skipped_matching_state: plan.as_ref().map_or(0, |plan| plan.skipped_matching_state),
            ..HistoricalImportAllSummary::default()
        });
    }
    let finalization_requires_import_environment = plan
        .as_ref()
        .is_some_and(|plan| plan.finalization_requires_import_environment);
    let environment_configs =
        if finalization_requires_import_environment && import_configs.is_empty() {
            configs
                .iter()
                .filter(|config| {
                    historical_chain_spec(&config.chain).is_some_and(|spec| {
                        spec.lifecycle != mmm_capture::source_registry::SourceLifecycle::Surveyed
                    })
                })
                .collect::<Vec<_>>()
        } else {
            import_configs
        };
    ensure_import_environment(client, classifier, &environment_configs).await?;
    let nbits_table = mmm_store::load_bitcoin_core_nbits_table(client).await?;
    let expected_error_parents = error_observations::pinned_error_parent_hashes();
    for (index, (chain_config, artifact)) in
        configs.iter().zip(&mut preflighted_artifacts).enumerate()
    {
        if !work_chain(index) {
            continue;
        }
        let spec = historical_chain_spec(&chain_config.chain)
            .expect("chain configs are built from the source registry");
        preflight_and_classify_candidates(
            client,
            classifier,
            chain_config,
            spec,
            artifact,
            &mut classifications,
            &nbits_table,
        )
        .await?;
    }
    if work_error_observations && let Some(artifact) = &mut error_observations {
        preflight_error_observations(
            client,
            classifier,
            artifact,
            &mut classifications,
            &nbits_table,
            &expected_error_parents,
        )
        .await?;
    }

    write_planned_imports(
        client,
        classifier,
        PlannedWrite {
            configs: &configs,
            preflighted_artifacts,
            error_observations,
            plan,
            work_error_observations,
        },
        &mut classifications,
        &nbits_table,
        &expected_error_parents,
    )
    .await
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
    run_historical_import_configs(client, classifier, configs, None).await
}

/// Exercise the production error-observation preflight and write path with a
/// small union-schema fixture. Only exposed to the database integration test
/// feature.
#[cfg(feature = "db-integration")]
#[doc(hidden)]
pub async fn run_error_observation_import_for_test(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    path: &std::path::Path,
    expected_parent_hashes: &[[u8; 32]],
) -> Result<HistoricalImportSummary> {
    if !classifier.is_enabled() {
        bail!("BITCOIN_RPC_URL is required for historical import");
    }
    let mut artifact = inspect_error_observation_csv(path, None)?;
    mmm_store::lock_bitcoin_core_header_cache(client).await?;
    let result = async {
        let nbits_table = mmm_store::load_bitcoin_core_nbits_table(client).await?;
        let expected_error_parents = expected_parent_hashes.iter().copied().collect();
        let mut classifications = HashMap::new();
        preflight_error_observations(
            client,
            classifier,
            &mut artifact,
            &mut classifications,
            &nbits_table,
            &expected_error_parents,
        )
        .await?;
        let summary = import_error_observations(
            client,
            classifier,
            artifact,
            &mut classifications,
            &nbits_table,
            &expected_error_parents,
        )
        .await?;
        rebuild_historical_source_health(client).await?;
        Ok(summary)
    }
    .await;
    mmm_store::finish_bitcoin_core_header_cache_operation(client, result).await
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

pub(super) async fn reconcile_published_stale_branches(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    nbits_table: &NbitsTable,
) -> Result<u64> {
    let queued = enqueue_published_stale_branches(client).await?;
    let fresh_classifications = HashMap::new();
    drain_historical_reconcile_queue_with_nbits_table(
        client,
        classifier,
        &fresh_classifications,
        Some(nbits_table),
    )
    .await?;
    Ok(queued)
}

async fn enqueue_published_stale_branches(client: &mut Client) -> Result<u64> {
    let rows = client
        .query(
            "SELECT DISTINCT e.btc_parent_header_hash \
             FROM merge_mining_event e \
             JOIN historical_event_provenance p ON p.event_id = e.id \
             LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
             WHERE e.revoked_at IS NULL \
               AND p.relevance_reason IN ('valid_direct_stale', 'valid_stale_descendant') \
               AND (b.kind IS NULL OR b.kind = 'unknown') \
             ORDER BY e.btc_parent_header_hash",
            &[],
        )
        .await
        .context("load published stale branches for targeted reconciliation")?;
    let queued = u64::try_from(rows.len()).context("targeted stale branch count exceeds u64")?;
    if rows.is_empty() {
        return Ok(0);
    }
    let txn = client
        .transaction()
        .await
        .context("begin targeted stale-branch queue transaction")?;
    for row in rows {
        let parent_hash: Vec<u8> = row.get(0);
        enqueue_historical_parent_reconcile(&txn, &parent_hash).await?;
    }
    invalidate_source_health_in_transaction(&txn).await?;
    txn.commit()
        .await
        .context("commit targeted stale-branch queue transaction")?;
    Ok(queued)
}

#[cfg(feature = "db-integration")]
#[doc(hidden)]
pub async fn enqueue_published_stale_branches_for_test(client: &mut Client) -> Result<u64> {
    enqueue_published_stale_branches(client).await
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
    if !classifier.is_enabled() {
        bail!("BITCOIN_RPC_URL is required for historical import");
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

/// Persist one decided candidate through the shared producer write path.
///
/// Resolves pool attribution from the parent coinbase, builds the standard event
/// payload, then writes it and durably enqueues the affected parent inside the
/// caller-owned chain transaction. The bounded read-model reconcile drains
/// after commit. `Skip` is unreachable here by construction.
async fn import_candidate(
    txn: &Transaction<'_>,
    context: &ImportContext<'_>,
    summary: &mut HistoricalImportSummary,
    candidate: ImportCandidate,
    decision: ImportDecision,
) -> Result<Vec<u8>> {
    let attributions = resolve_parent_pool_attribution_from_coinbase(
        candidate
            .evidence
            .btc_parent_coinbase_script
            .as_deref()
            .unwrap_or_default(),
        &candidate.parent_output_addresses,
        context.resolver,
        context.pool_ids_by_slug,
    )
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
    // Core-attested path. Sticky and manual revocations stay untouched. Hathor
    // deliberately stays on the generic upsert: its
    // reversible revocations (voided/superseded) track CURRENT child-DAG
    // state that a historical observation must not resurrect, and its writer
    // requires the RFC 0006 sidecar the exports cannot supply. Every other
    // chain writes the event alone. `pool_identity_id` stays NULL here --
    // the `reclassify-pools` late-fill path resolves it from the registry.
    let rsk_evidence = candidate.rsk_evidence.as_ref();
    let use_elastos_writer = context.chain == "elastos";
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
        ImportDecision::CapturePreclassified(parent_classification) => Some(*parent_classification),
        ImportDecision::Skip(_) => unreachable!("skip decisions do not reach import_candidate"),
    };
    let outcome = write_historical_base_in_transaction(
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
    summary.record_attestation(&candidate);
    Ok(parent_hash)
}

async fn record_persisted_parent_counts(
    client: &Client,
    summary: &mut HistoricalImportSummary,
    parent_counts: HashMap<Vec<u8>, u64>,
) -> Result<()> {
    let parent_hashes = parent_counts.keys().cloned().collect::<Vec<_>>();
    let mut persisted = HashMap::with_capacity(parent_hashes.len());
    for chunk in parent_hashes.chunks(1_000) {
        let hashes = chunk.to_vec();
        for row in client
            .query(
                "SELECT btc_header_hash, kind, btc_orphan_class \
                 FROM block \
                 WHERE btc_header_hash = ANY($1::bytea[])",
                &[&hashes],
            )
            .await
            .context("load persisted historical parent classifications")?
        {
            let hash: Vec<u8> = row.get(0);
            let kind: String = row.get(1);
            let orphan_class: Option<String> = row.get(2);
            persisted.insert(hash, (BlockKind::from_db_str(&kind)?, orphan_class));
        }
    }
    for (hash, count) in parent_counts {
        summary.record_persisted(persisted.remove(&hash), count);
    }
    Ok(())
}
