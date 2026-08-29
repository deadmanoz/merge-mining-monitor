use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::nbits_table::NbitsTable;
use mmm_read_model::rebuild_historical_source_health;
use tokio_postgres::Client;
use tracing::info;

use super::super::config::HistoricalImportConfig;
use super::super::publication::{ArtifactPreflight, ErrorObservationPreflight};
use super::error_observations::import_error_observations;
use super::publication_state::ImportPlan;
use super::{
    HistoricalImportAllSummary, reconcile_published_stale_branches,
    run_historical_import_with_cache,
};

pub(super) struct PlannedWrite<'a> {
    pub configs: &'a [HistoricalImportConfig],
    pub preflighted_artifacts: Vec<ArtifactPreflight>,
    pub error_observations: Option<ErrorObservationPreflight>,
    pub plan: Option<ImportPlan>,
    pub work_error_observations: bool,
}

pub(super) async fn write_planned_imports(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    write: PlannedWrite<'_>,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
    nbits_table: &NbitsTable,
    expected_error_parents: &BTreeSet<[u8; 32]>,
) -> Result<HistoricalImportAllSummary> {
    let PlannedWrite {
        configs,
        preflighted_artifacts,
        error_observations,
        plan,
        work_error_observations,
    } = write;
    let work_chain = |index: usize| plan.as_ref().is_none_or(|plan| plan.work_chain[index]);
    let mut summary = HistoricalImportAllSummary::default();
    if let Some(plan) = &plan {
        summary.skipped_matching_state = plan.skipped_matching_state;
    }
    let work_total = configs
        .iter()
        .enumerate()
        .filter(|(index, _)| work_chain(*index))
        .count();
    let mut work_index = 0_usize;
    for (index, (chain_config, artifact)) in configs.iter().zip(preflighted_artifacts).enumerate() {
        if !work_chain(index) {
            continue;
        }
        work_index += 1;
        info!(
            chain = %chain_config.chain,
            current = work_index,
            total = work_total,
            "importing historical publication chain"
        );
        let chain_summary = run_historical_import_with_cache(
            client,
            classifier,
            chain_config,
            classifications,
            Some(artifact),
            false,
            Some(nbits_table),
        )
        .await?;
        summary
            .chains
            .push((chain_config.chain.clone(), chain_summary));
    }
    if work_error_observations && let Some(artifact) = error_observations {
        summary.error_observations = Some(
            import_error_observations(
                client,
                classifier,
                artifact,
                classifications,
                nbits_table,
                expected_error_parents,
            )
            .await?,
        );
    }
    if work_total > 0
        || work_error_observations
        || plan.as_ref().is_some_and(|plan| plan.needs_finalization)
    {
        summary.stale_branches_reconciled =
            reconcile_published_stale_branches(client, classifier, nbits_table).await?;
        rebuild_historical_source_health(client).await?;
    }
    Ok(summary)
}
