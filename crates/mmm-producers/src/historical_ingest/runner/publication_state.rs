//! Plan `import-all` by comparing publication-owned state with the database.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail, ensure};
use futures::{TryStreamExt, pin_mut};
use mmm_capture::source_registry::SourceLifecycle;
use tokio_postgres::Client;

use super::super::config::{HistoricalImportConfig, historical_chain_spec};
use super::super::csv_source::{
    ComparablePublicationState, ExpectedPublicationState, PublicationRowKey,
};
use super::super::publication::{ArtifactPreflight, ErrorObservationPreflight};

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ImportPlan {
    pub(super) work_chain: Vec<bool>,
    pub(super) work_error_observations: bool,
    pub(super) needs_finalization: bool,
    pub(super) skipped_matching_state: u64,
}

struct ExpectedRow<'a> {
    artifact_index: usize,
    state: &'a ExpectedPublicationState,
    matched: bool,
}

struct ExpectedErrorRow<'a> {
    state: &'a ExpectedPublicationState,
    matched: bool,
}

type ExpectedRows<'a> = BTreeMap<PublicationRowKey, ExpectedRow<'a>>;
type ExpectedErrorRows<'a> = BTreeMap<PublicationRowKey, ExpectedErrorRow<'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum BaseIdentity {
    Partial {
        child_height: i32,
        parent_hash: [u8; 32],
    },
}

/// Compare the verified publication with stored provenance before acquiring
/// the Bitcoin Core cache lock. Research pins are intentionally absent from
/// the logical row key: any non-operator pin with matching content satisfies
/// the current publication coordinate.
pub(super) async fn plan_publication_import(
    client: &Client,
    configs: &[HistoricalImportConfig],
    artifacts: &[ArtifactPreflight],
    error_observations: Option<&ErrorObservationPreflight>,
) -> Result<ImportPlan> {
    ensure!(
        configs.len() == artifacts.len(),
        "historical import config and preflight counts differ"
    );
    let mut plan = ImportPlan {
        work_chain: vec![false; configs.len()],
        work_error_observations: false,
        needs_finalization: false,
        skipped_matching_state: 0,
    };
    let chain_indices = configs
        .iter()
        .enumerate()
        .map(|(index, config)| (config.chain.as_str(), index))
        .collect::<BTreeMap<_, _>>();
    let (mut expected, mut expected_errors) = build_expected_rows(artifacts, error_observations)?;
    match_normal_state(client, configs, &chain_indices, &mut expected, &mut plan).await?;
    let error_event_ids = match_error_state(client, &mut expected_errors).await?;

    for row in expected.values() {
        if !row.matched {
            plan.work_chain[row.artifact_index] = true;
        }
    }
    plan.work_error_observations = expected_errors.values().any(|row| !row.matched);

    verify_authoritative_base_state(client, configs, artifacts, &error_event_ids, &mut plan)
        .await?;

    if plan.work_chain.iter().all(|work| !work) && !plan.work_error_observations {
        plan.needs_finalization = mmm_store::load_historical_finalization_state(client)
            .await?
            .required();
    }
    plan.skipped_matching_state = u64::try_from(
        plan.work_chain.iter().filter(|work| !**work).count()
            + usize::from(error_observations.is_some() && !plan.work_error_observations),
    )
    .context("matching historical artifact count exceeds u64")?;
    tracing::info!(
        work_chains = plan.work_chain.iter().filter(|work| **work).count(),
        work_error_observations = plan.work_error_observations,
        needs_finalization = plan.needs_finalization,
        skipped_matching_state = plan.skipped_matching_state,
        "planned historical publication import from database state"
    );
    Ok(plan)
}

fn build_expected_rows<'a>(
    artifacts: &'a [ArtifactPreflight],
    error_observations: Option<&'a ErrorObservationPreflight>,
) -> Result<(ExpectedRows<'a>, ExpectedErrorRows<'a>)> {
    let mut expected = BTreeMap::new();
    for (artifact_index, artifact) in artifacts.iter().enumerate() {
        for state in &artifact.state_rows {
            ensure!(
                expected
                    .insert(
                        state.key.clone(),
                        ExpectedRow {
                            artifact_index,
                            state,
                            matched: false,
                        },
                    )
                    .is_none(),
                "publication coordinate appears in more than one event artifact: {}:{}:{}",
                state.key.chain,
                state.key.source_path,
                state.key.source_row_number
            );
        }
    }
    let mut expected_errors = BTreeMap::new();
    if let Some(artifact) = error_observations {
        for state in &artifact.state_rows {
            ensure!(
                expected_errors
                    .insert(
                        state.key.clone(),
                        ExpectedErrorRow {
                            state,
                            matched: false,
                        },
                    )
                    .is_none(),
                "publication coordinate appears twice in error observations: {}:{}:{}",
                state.key.chain,
                state.key.source_path,
                state.key.source_row_number
            );
        }
    }
    Ok((expected, expected_errors))
}

async fn match_normal_state(
    client: &Client,
    configs: &[HistoricalImportConfig],
    chain_indices: &BTreeMap<&str, usize>,
    expected: &mut ExpectedRows<'_>,
    plan: &mut ImportPlan,
) -> Result<()> {
    let rows = mmm_store::stream_historical_publication_state(client).await?;
    pin_mut!(rows);
    while let Some(row) = rows
        .try_next()
        .await
        .context("read stored historical publication state")?
    {
        let row = mmm_store::HistoricalPublicationStateRow::from_row(&row);
        let key = PublicationRowKey {
            chain: row.chain.clone(),
            source_path: row.source_path.clone(),
            source_row_number: row.source_row_number,
        };
        let stored = ComparablePublicationState::from_stored(row)?;
        let Some(&artifact_index) = chain_indices.get(key.chain.as_str()) else {
            continue;
        };
        if let Some(expected) = expected.get_mut(&key) {
            if expected.state.matches(&stored) {
                expected.matched = true;
            }
        } else if exact_lifecycle(configs[artifact_index].chain.as_str())? {
            plan.work_chain[artifact_index] = true;
        }
    }
    Ok(())
}

async fn match_error_state(
    client: &Client,
    expected: &mut ExpectedErrorRows<'_>,
) -> Result<BTreeSet<i64>> {
    let rows = mmm_store::stream_historical_error_observation_state(client).await?;
    pin_mut!(rows);
    let mut event_ids = BTreeSet::new();
    while let Some(row) = rows
        .try_next()
        .await
        .context("read stored historical error-observation state")?
    {
        let row = mmm_store::HistoricalPublicationStateRow::from_row(&row);
        event_ids.insert(row.event_id);
        let key = PublicationRowKey {
            chain: row.chain.clone(),
            source_path: row.source_path.clone(),
            source_row_number: row.source_row_number,
        };
        let stored = ComparablePublicationState::from_stored(row)?;
        if let Some(expected) = expected.get_mut(&key)
            && expected.state.matches(&stored)
        {
            expected.matched = true;
        }
    }
    Ok(event_ids)
}

async fn verify_authoritative_base_state(
    client: &Client,
    configs: &[HistoricalImportConfig],
    artifacts: &[ArtifactPreflight],
    error_event_ids: &BTreeSet<i64>,
    plan: &mut ImportPlan,
) -> Result<()> {
    let mut source_ids = Vec::new();
    let mut chain_indices = BTreeMap::new();
    let mut exact = Vec::with_capacity(configs.len());
    let mut partial = Vec::with_capacity(configs.len());
    let mut partial_seen = Vec::with_capacity(configs.len());
    for (index, (config, artifact)) in configs.iter().zip(artifacts).enumerate() {
        exact.push(BTreeSet::new());
        partial.push(BTreeSet::new());
        partial_seen.push(BTreeSet::new());
        let spec = historical_chain_spec(&config.chain)
            .context("publication config chain is absent from source registry")?;
        if spec.lifecycle == SourceLifecycle::Live {
            continue;
        }
        if plan.work_chain[index] && spec.lifecycle != SourceLifecycle::Surveyed {
            continue;
        }
        source_ids.push(spec.source_id);
        chain_indices.insert(spec.chain, index);
        if spec.lifecycle == SourceLifecycle::Surveyed {
            continue;
        }
        for state in &artifact.state_rows {
            match state.child_block_hash {
                Some(hash) => {
                    exact[index].insert(hash);
                }
                None => {
                    partial[index].insert(BaseIdentity::Partial {
                        child_height: state
                            .child_height
                            .context("partial publication row has no child height")?,
                        parent_hash: state.btc_parent_header_hash,
                    });
                }
            }
        }
    }
    if source_ids.is_empty() {
        return Ok(());
    }

    let rows = mmm_store::stream_authoritative_historical_base_events(client, &source_ids).await?;
    pin_mut!(rows);
    while let Some(row) = rows
        .try_next()
        .await
        .context("read authoritative historical base state")?
    {
        let row = mmm_store::HistoricalBaseEventRow::from_row(&row);
        if error_event_ids.contains(&row.event_id) {
            continue;
        }
        let Some(&index) = chain_indices.get(row.chain.as_str()) else {
            continue;
        };
        let spec = historical_chain_spec(&row.chain)
            .context("stored historical source chain is absent from source registry")?;
        if spec.lifecycle == SourceLifecycle::Surveyed {
            bail!(
                "surveyed source {} has stored base events but its publication is explicitly empty",
                row.chain
            );
        }
        let parent_hash = array32(row.btc_parent_header_hash, "btc_parent_header_hash")?;
        let covered = match row.child_block_hash {
            Some(hash) => {
                let hash = array32(hash, "child_block_hash")?;
                if exact[index].contains(&hash) {
                    true
                } else {
                    cover_partial(
                        index,
                        row.child_height,
                        parent_hash,
                        &partial,
                        &mut partial_seen,
                    )
                }
            }
            None => cover_partial(
                index,
                row.child_height,
                parent_hash,
                &partial,
                &mut partial_seen,
            ),
        };
        if !covered {
            plan.work_chain[index] = true;
        }
    }
    Ok(())
}

fn cover_partial(
    index: usize,
    child_height: Option<i32>,
    parent_hash: [u8; 32],
    expected: &[BTreeSet<BaseIdentity>],
    seen: &mut [BTreeSet<BaseIdentity>],
) -> bool {
    let Some(child_height) = child_height else {
        return false;
    };
    let identity = BaseIdentity::Partial {
        child_height,
        parent_hash,
    };
    expected[index].contains(&identity) && seen[index].insert(identity)
}

fn exact_lifecycle(chain: &str) -> Result<bool> {
    Ok(matches!(
        historical_chain_spec(chain)
            .with_context(|| format!("publication chain {chain:?} is absent from source registry"))?
            .lifecycle,
        SourceLifecycle::Historical | SourceLifecycle::Partial | SourceLifecycle::Surveyed
    ))
}

fn array32(value: Vec<u8>, field: &str) -> Result<[u8; 32]> {
    value
        .try_into()
        .map_err(|_| anyhow::anyhow!("stored {field} is not 32 bytes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_database_enrichment_matches_only_once() {
        let identity = BaseIdentity::Partial {
            child_height: 42,
            parent_hash: [7; 32],
        };
        let expected = [BTreeSet::from([identity])];
        let mut seen = [BTreeSet::new()];
        assert!(cover_partial(0, Some(42), [7; 32], &expected, &mut seen));
        assert!(!cover_partial(0, Some(42), [7; 32], &expected, &mut seen));
    }

    #[test]
    fn partial_identity_includes_parent_hash() {
        let expected = [BTreeSet::from([BaseIdentity::Partial {
            child_height: 42,
            parent_hash: [7; 32],
        }])];
        let mut seen = [BTreeSet::new()];
        assert!(!cover_partial(0, Some(42), [8; 32], &expected, &mut seen));
    }
}
