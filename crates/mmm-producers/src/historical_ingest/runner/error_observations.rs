//! Import and preserve catalogue-backed historical error witnesses.

use std::collections::{BTreeMap, HashMap};

use anyhow::{Context, Result, bail, ensure};
use mmm_bitcoin_core::{ConfiguredParentClassifier, ParentClassification};
use mmm_capture::nbits_table::NbitsTable;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{
    drain_historical_reconcile_queue_with_nbits_table, invalidate_source_health_in_transaction,
};
use mmm_store::upsert_pool_snapshot;
use tokio_postgres::Client;

use super::super::config::{
    HistoricalChainSpec, PINNED_RESEARCH_COMMIT, historical_chain_spec, importable_chains,
    is_historical_import_chain,
};
use super::super::csv_source::{
    CsvLayout, ImportCandidate, error_observation_candidate_from_record,
};
use super::super::preclassify::{ImportDecision, import_error_observation_decision};
use super::super::publication::ErrorObservationPreflight;
use super::{
    HistoricalImportSummary, ImportContext, import_candidate, record_persisted_parent_counts,
};

/// Parse and classify every aggregate row before any database mutation. The
/// file is already byte-verified by publication preflight; this adds the
/// source-chain decoder and mandatory Core-plus-catalogue gate.
pub(super) async fn preflight_error_observations(
    client: &Client,
    classifier: &ConfiguredParentClassifier,
    artifact: &mut ErrorObservationPreflight,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
) -> Result<()> {
    let mut reader = artifact.open_reader()?;
    let headers = reader
        .headers()
        .context("read error-observation CSV header")?
        .clone();
    let mut rows = 0_u64;
    let mut source_chain_counts = BTreeMap::new();
    for (offset, record) in reader.records().enumerate() {
        let record =
            record.with_context(|| format!("parse error-observation row {}", offset + 2))?;
        let (spec, candidate) = parse_error_observation_candidate(&headers, &record, offset + 2)?;
        match import_error_observation_decision(client, classifier, &candidate, classifications)
            .await?
        {
            ImportDecision::CapturePreclassified(_) => {}
            ImportDecision::Skip(reason) => {
                bail!(
                    "error-observation row {} would be skipped as {}",
                    offset + 2,
                    reason.as_str()
                );
            }
        }
        *source_chain_counts
            .entry(spec.chain.to_owned())
            .or_insert(0) += 1;
        rows += 1;
    }
    ensure!(
        rows == artifact.row_count,
        "error-observation artifact changed during preflight: expected {} rows, parsed {rows}",
        artifact.row_count
    );
    ensure!(
        source_chain_counts == artifact.source_chain_counts,
        "error-observation source-chain counts changed during preflight"
    );
    Ok(())
}

/// Write the preflighted aggregate after ordinary chain imports. It deliberately
/// has no authoritative-delete phase: historical error-witness provenance is
/// retained outside normal per-chain snapshot reconciliation.
pub(super) async fn import_error_observations(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    mut artifact: ErrorObservationPreflight,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
    nbits_table: &NbitsTable,
) -> Result<HistoricalImportSummary> {
    let resolver = PoolResolver::from_default_snapshot().context("load embedded pool snapshot")?;
    let source_ids = load_historical_source_ids(client).await?;
    let txn = client
        .transaction()
        .await
        .context("begin error-observation historical transaction")?;
    let pool_ids_by_slug = upsert_pool_snapshot(&txn, resolver.snapshot()).await?;
    let mut summary = HistoricalImportSummary {
        expected_rows: artifact.row_count,
        ..HistoricalImportSummary::default()
    };
    let mut parent_counts = HashMap::new();
    let mut source_chain_counts = BTreeMap::new();
    let mut reader = artifact.open_reader()?;
    let headers = reader
        .headers()
        .context("read error-observation CSV header")?
        .clone();
    for (offset, record) in reader.records().enumerate() {
        summary.rows_seen += 1;
        let record =
            record.with_context(|| format!("parse error-observation row {}", offset + 2))?;
        let (spec, candidate) = parse_error_observation_candidate(&headers, &record, offset + 2)?;
        *source_chain_counts
            .entry(spec.chain.to_owned())
            .or_insert(0) += 1;
        let decision =
            import_error_observation_decision(&txn, classifier, &candidate, classifications)
                .await?;
        let ImportDecision::CapturePreclassified(_) = decision else {
            let ImportDecision::Skip(reason) = decision else {
                unreachable!("error-observation decision is exhaustive");
            };
            bail!(
                "error-observation row {} would be skipped as {}",
                offset + 2,
                reason.as_str()
            );
        };
        summary.candidates += 1;
        let source_id = *source_ids
            .get(spec.chain)
            .expect("source id loaded for every importable chain");
        import_candidate(
            &txn,
            &ImportContext {
                source_id,
                chain: spec.chain,
                classifier,
                resolver: &resolver,
                pool_ids_by_slug: &pool_ids_by_slug,
            },
            &mut summary,
            candidate,
            decision,
        )
        .await
        .map(|parent_hash| *parent_counts.entry(parent_hash).or_default() += 1)?;
    }
    ensure!(
        summary.rows_seen == artifact.row_count,
        "error-observation artifact changed during import: expected {} rows, parsed {}",
        artifact.row_count,
        summary.rows_seen
    );
    ensure!(
        source_chain_counts == artifact.source_chain_counts,
        "error-observation source-chain counts changed during import"
    );
    invalidate_source_health_in_transaction(&txn).await?;
    txn.commit()
        .await
        .context("commit error-observation historical transaction")?;
    drain_historical_reconcile_queue_with_nbits_table(
        client,
        classifier,
        classifications,
        Some(nbits_table),
    )
    .await?;
    summary.error_parents =
        u64::try_from(parent_counts.len()).context("error-observation parent count exceeds u64")?;
    record_persisted_parent_counts(client, &mut summary, parent_counts).await?;
    Ok(summary)
}

fn parse_error_observation_candidate(
    headers: &csv::StringRecord,
    record: &csv::StringRecord,
    row_number: usize,
) -> Result<(&'static HistoricalChainSpec, ImportCandidate)> {
    let chain = record
        .get(0)
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("error-observation row {row_number} has no source chain"))?;
    let spec =
        historical_chain_spec(chain).ok_or_else(|| {
            anyhow::anyhow!(
                "error-observation row {row_number} has unsupported source chain {chain:?}",
            )
        })?;
    ensure!(
        is_historical_import_chain(chain),
        "error-observation row {row_number} has unknown or surveyed source chain {chain:?}"
    );
    let layout = CsvLayout::new(headers, spec)?;
    let candidate = error_observation_candidate_from_record(
        spec,
        &layout,
        record,
        PINNED_RESEARCH_COMMIT.as_str(),
    )
    .map_err(|reason| {
        anyhow::anyhow!(
            "error-observation row {row_number} failed {}",
            reason.as_str()
        )
    })?;
    Ok((spec, candidate))
}

async fn load_historical_source_ids(client: &Client) -> Result<HashMap<&'static str, i64>> {
    let mut source_ids = HashMap::new();
    for spec in importable_chains() {
        source_ids.insert(
            spec.chain,
            mmm_store::get_source_id(client, spec.source_code).await?,
        );
    }
    Ok(source_ids)
}
