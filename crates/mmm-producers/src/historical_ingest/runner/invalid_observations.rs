//! Import and preserve catalogue-backed historical invalid witnesses.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use anyhow::{Context, Result, bail, ensure};
use bitcoin::hashes::Hash as _;
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
    CsvLayout, ImportCandidate, invalid_observation_candidate_from_record,
};
use super::super::preclassify::{ImportDecision, import_invalid_observation_decision};
use super::super::publication::InvalidObservationPreflight;
use super::{
    HistoricalImportSummary, ImportContext, import_candidate, record_persisted_parent_counts,
};

/// Parse and classify every aggregate row before any database mutation. The
/// file is already byte-verified by publication preflight; this adds the
/// source-chain decoder and mandatory Core-plus-catalogue gate.
pub(super) async fn preflight_invalid_observations(
    client: &Client,
    classifier: &ConfiguredParentClassifier,
    artifact: &mut InvalidObservationPreflight,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
    nbits_table: &NbitsTable,
    expected_parent_hashes: &BTreeSet<[u8; 32]>,
) -> Result<()> {
    let mut reader = artifact.open_reader()?;
    let headers = reader
        .headers()
        .context("read invalid-observation CSV header")?
        .clone();
    let mut rows = 0_u64;
    let mut source_chain_counts = BTreeMap::new();
    let mut parent_hashes = BTreeSet::new();
    for (offset, record) in reader.records().enumerate() {
        let record =
            record.with_context(|| format!("parse invalid-observation row {}", offset + 2))?;
        let (spec, candidate) =
            parse_invalid_observation_candidate(&headers, &record, offset + 2, nbits_table)?;
        match import_invalid_observation_decision(client, classifier, &candidate, classifications)
            .await?
        {
            ImportDecision::CapturePreclassified(_) => {}
            ImportDecision::Skip(reason) => {
                bail!(
                    "invalid-observation row {} would be skipped as {}",
                    offset + 2,
                    reason.as_str()
                );
            }
        }
        *source_chain_counts
            .entry(spec.chain.to_owned())
            .or_insert(0) += 1;
        parent_hashes.insert(
            candidate
                .evidence
                .btc_parent_header
                .block_hash()
                .to_byte_array(),
        );
        rows += 1;
    }
    ensure!(
        rows == artifact.row_count,
        "invalid-observation artifact changed during preflight: expected {} rows, parsed {rows}",
        artifact.row_count
    );
    ensure!(
        source_chain_counts == artifact.source_chain_counts,
        "invalid-observation source-chain counts changed during preflight"
    );
    ensure_catalogue_coverage(&parent_hashes, expected_parent_hashes)?;
    Ok(())
}

/// Write the preflighted aggregate before ordinary chain imports. It deliberately
/// has no authoritative-delete phase: historical invalid-witness provenance is
/// retained outside normal per-chain snapshot reconciliation.
pub(super) async fn import_invalid_observations(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    mut artifact: InvalidObservationPreflight,
    classifications: &mut HashMap<Vec<u8>, ParentClassification>,
    nbits_table: &NbitsTable,
    expected_parent_hashes: &BTreeSet<[u8; 32]>,
) -> Result<HistoricalImportSummary> {
    let resolver = PoolResolver::from_default_snapshot().context("load embedded pool snapshot")?;
    let source_ids = load_historical_source_ids(client).await?;
    let txn = client
        .transaction()
        .await
        .context("begin invalid-observation historical transaction")?;
    let pool_ids_by_slug = upsert_pool_snapshot(&txn, resolver.snapshot()).await?;
    let mut summary = HistoricalImportSummary {
        expected_rows: artifact.row_count,
        ..HistoricalImportSummary::default()
    };
    let mut parent_counts = HashMap::new();
    let mut source_chain_counts = BTreeMap::new();
    let mut parent_hashes = BTreeSet::new();
    let mut reader = artifact.open_reader()?;
    let headers = reader
        .headers()
        .context("read invalid-observation CSV header")?
        .clone();
    for (offset, record) in reader.records().enumerate() {
        summary.rows_seen += 1;
        let record =
            record.with_context(|| format!("parse invalid-observation row {}", offset + 2))?;
        let (spec, candidate) =
            parse_invalid_observation_candidate(&headers, &record, offset + 2, nbits_table)?;
        *source_chain_counts
            .entry(spec.chain.to_owned())
            .or_insert(0) += 1;
        let decision =
            import_invalid_observation_decision(&txn, classifier, &candidate, classifications)
                .await?;
        let ImportDecision::CapturePreclassified(_) = decision else {
            let ImportDecision::Skip(reason) = decision else {
                unreachable!("invalid-observation decision is exhaustive");
            };
            bail!(
                "invalid-observation row {} would be skipped as {}",
                offset + 2,
                reason.as_str()
            );
        };
        summary.candidates += 1;
        parent_hashes.insert(
            candidate
                .evidence
                .btc_parent_header
                .block_hash()
                .to_byte_array(),
        );
        let source_id = *source_ids
            .get(spec.chain)
            .expect("source id loaded for every importable chain");
        import_candidate(
            &txn,
            &ImportContext {
                source_id,
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
        "invalid-observation artifact changed during import: expected {} rows, parsed {}",
        artifact.row_count,
        summary.rows_seen
    );
    ensure!(
        source_chain_counts == artifact.source_chain_counts,
        "invalid-observation source-chain counts changed during import"
    );
    ensure_catalogue_coverage(&parent_hashes, expected_parent_hashes)?;
    invalidate_source_health_in_transaction(&txn).await?;
    txn.commit()
        .await
        .context("commit invalid-observation historical transaction")?;
    drain_historical_reconcile_queue_with_nbits_table(
        client,
        classifier,
        classifications,
        Some(nbits_table),
    )
    .await?;
    summary.invalid_parents = u64::try_from(parent_counts.len())
        .context("invalid-observation parent count exceeds u64")?;
    record_persisted_parent_counts(client, &mut summary, parent_counts).await?;
    Ok(summary)
}

fn parse_invalid_observation_candidate(
    headers: &csv::StringRecord,
    record: &csv::StringRecord,
    row_number: usize,
    nbits_table: &NbitsTable,
) -> Result<(&'static HistoricalChainSpec, ImportCandidate)> {
    let chain = record.get(0).map(str::trim).ok_or_else(|| {
        anyhow::anyhow!("invalid-observation row {row_number} has no source chain")
    })?;
    let spec = historical_chain_spec(chain).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid-observation row {row_number} has unsupported source chain {chain:?}",
        )
    })?;
    ensure!(
        is_historical_import_chain(chain),
        "invalid-observation row {row_number} has unknown or surveyed source chain {chain:?}"
    );
    let layout = CsvLayout::new(headers, spec)?;
    let candidate = invalid_observation_candidate_from_record(
        spec,
        &layout,
        record,
        PINNED_RESEARCH_COMMIT.as_str(),
        nbits_table,
    )
    .map_err(|reason| {
        anyhow::anyhow!(
            "invalid-observation row {row_number} failed {}",
            reason.as_str()
        )
    })?;
    Ok((spec, candidate))
}

pub(super) fn pinned_invalid_parent_hashes() -> BTreeSet<[u8; 32]> {
    mmm_capture::invalid_blocks::hashes().collect()
}

fn ensure_catalogue_coverage(
    parent_hashes: &BTreeSet<[u8; 32]>,
    expected_parent_hashes: &BTreeSet<[u8; 32]>,
) -> Result<()> {
    ensure!(
        parent_hashes == expected_parent_hashes,
        "invalid-observation artifact does not cover the pinned invalid-block catalogue"
    );
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_coverage_rejects_a_missing_parent() {
        let expected_parent_hashes = pinned_invalid_parent_hashes();
        let mut parent_hashes = expected_parent_hashes.clone();
        let removed = *parent_hashes.iter().next().expect("non-empty catalogue");
        assert!(parent_hashes.remove(&removed));
        let error = ensure_catalogue_coverage(&parent_hashes, &expected_parent_hashes)
            .expect_err("a missing catalogue parent must fail preflight");
        assert!(error.to_string().contains("does not cover"));
    }
}
