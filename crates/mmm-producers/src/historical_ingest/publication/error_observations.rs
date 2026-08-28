//! Preflight for the aggregate error-witness artifact.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, ensure};

use super::{
    NORMALIZED_COLUMNS, PublicationArtifact, RSK_SIDECAR_COLUMNS, open_artifact_file,
    required_column,
};
use crate::historical_ingest::config::is_historical_import_chain;

/// Verified error-observation aggregate retained for the write phase after all
/// normal artifacts have preflighted. Its rows carry their real source chain,
/// so layout selection happens per row rather than per file.
#[derive(Debug)]
pub(crate) struct ErrorObservationPreflight {
    pub(crate) row_count: u64,
    pub(crate) source_chain_counts: BTreeMap<String, u64>,
    pub(crate) identity: Option<PublicationArtifact>,
    file: File,
}

impl ErrorObservationPreflight {
    pub(crate) fn open_reader(&mut self) -> Result<csv::Reader<&mut File>> {
        self.file
            .seek(SeekFrom::Start(0))
            .context("rewind verified error-observation artifact")?;
        Ok(csv::Reader::from_reader(&mut self.file))
    }
}

pub(crate) fn inspect_error_observation_csv(
    path: &Path,
    expected: Option<&PublicationArtifact>,
) -> Result<ErrorObservationPreflight> {
    let mut file = open_artifact_file(path, expected)?;
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    let mut reader = csv::Reader::from_reader(&mut file);
    let mut expected_header = NORMALIZED_COLUMNS.to_vec();
    expected_header.extend_from_slice(RSK_SIDECAR_COLUMNS);
    ensure!(
        reader
            .headers()?
            .iter()
            .map(str::trim)
            .eq(expected_header.iter().copied()),
        "CSV header does not match the error-observation union schema"
    );
    let chain_index = required_column(reader.headers()?, "chain")?;
    let classification_index = required_column(reader.headers()?, "classification")?;
    let mut row_count = 0_u64;
    let mut source_chain_counts = BTreeMap::new();
    for (offset, record) in reader.records().enumerate() {
        let record =
            record.with_context(|| format!("parse {} row {}", path.display(), offset + 2))?;
        let chain = record
            .get(chain_index)
            .map(str::trim)
            .filter(|chain| !chain.is_empty())
            .context("error-observation row has no source chain")?;
        ensure!(
            is_historical_import_chain(chain),
            "{} row {} has an unknown or surveyed source chain {chain:?}",
            path.display(),
            offset + 2
        );
        ensure!(
            record.get(classification_index).map(str::trim) == Some("error_block"),
            "{} row {} is not labelled error_block",
            path.display(),
            offset + 2
        );
        *source_chain_counts.entry(chain.to_owned()).or_insert(0) += 1;
        row_count += 1;
    }
    if let Some(expected) = expected {
        ensure!(
            row_count == expected.row_count,
            "artifact row-count mismatch for {}: expected {}, got {}",
            path.display(),
            expected.row_count,
            row_count
        );
        ensure!(
            source_chain_counts == expected.source_chain_counts,
            "error-observation source-chain count mismatch for {}",
            path.display()
        );
    }
    drop(reader);
    file.seek(SeekFrom::Start(0))
        .with_context(|| format!("rewind {}", path.display()))?;
    Ok(ErrorObservationPreflight {
        row_count,
        source_chain_counts,
        identity: expected.cloned(),
        file,
    })
}
