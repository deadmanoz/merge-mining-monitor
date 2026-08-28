//! Last-imported publication artifact receipts for smart `import-all`.

use anyhow::{Context, Result};
use tokio_postgres::GenericClient;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalImportArtifact {
    pub role: String,
    pub chain: String,
    pub sha256: String,
    pub size_bytes: i64,
    pub row_count: i64,
    pub source_repo_commit: String,
}

pub async fn count_historical_import_artifacts<C: GenericClient>(client: &C) -> Result<i64> {
    let row = client
        .query_one(
            "SELECT count(*)::bigint FROM historical_import_artifact",
            &[],
        )
        .await
        .context("count historical_import_artifact")?;
    Ok(row.get(0))
}

pub async fn load_historical_import_artifacts<C: GenericClient>(
    client: &C,
) -> Result<Vec<HistoricalImportArtifact>> {
    let rows = client
        .query(
            "SELECT role, chain, sha256, size_bytes, row_count, source_repo_commit \
             FROM historical_import_artifact",
            &[],
        )
        .await
        .context("load historical_import_artifact")?;
    Ok(rows
        .into_iter()
        .map(|row| HistoricalImportArtifact {
            role: row.get(0),
            chain: row.get(1),
            sha256: row.get(2),
            size_bytes: row.get(3),
            row_count: row.get(4),
            source_repo_commit: row.get(5),
        })
        .collect())
}

pub async fn upsert_historical_import_artifact<C: GenericClient>(
    client: &C,
    artifact: &HistoricalImportArtifact,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO historical_import_artifact ( \
                role, chain, sha256, size_bytes, row_count, source_repo_commit \
             ) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (role, chain) DO UPDATE SET \
                sha256 = EXCLUDED.sha256, \
                size_bytes = EXCLUDED.size_bytes, \
                row_count = EXCLUDED.row_count, \
                source_repo_commit = EXCLUDED.source_repo_commit, \
                imported_at = now()",
            &[
                &artifact.role,
                &artifact.chain,
                &artifact.sha256,
                &artifact.size_bytes,
                &artifact.row_count,
                &artifact.source_repo_commit,
            ],
        )
        .await
        .context("upsert historical_import_artifact")?;
    Ok(())
}

pub async fn seed_historical_import_artifacts<C: GenericClient>(
    client: &C,
    artifacts: &[HistoricalImportArtifact],
) -> Result<u64> {
    let mut inserted = 0_u64;
    for artifact in artifacts {
        let rows = client
            .execute(
                "INSERT INTO historical_import_artifact ( \
                    role, chain, sha256, size_bytes, row_count, source_repo_commit \
                 ) VALUES ($1, $2, $3, $4, $5, $6) \
                 ON CONFLICT (role, chain) DO NOTHING",
                &[
                    &artifact.role,
                    &artifact.chain,
                    &artifact.sha256,
                    &artifact.size_bytes,
                    &artifact.row_count,
                    &artifact.source_repo_commit,
                ],
            )
            .await
            .context("seed historical_import_artifact")?;
        inserted += rows;
    }
    Ok(inserted)
}
