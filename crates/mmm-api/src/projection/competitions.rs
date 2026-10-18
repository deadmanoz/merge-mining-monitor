//! Stale-vs-canonical competition projection for `/api/v1/competitions`.

use anyhow::{Context, Result};
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use serde::Serialize;
use tokio_postgres::Client;

use super::ProjectionError;
use super::shared::{
    COMPETITION_DELTA_SQL, COMPETITION_FROM_SQL, PoolObject, admit_observed_bitcoin_source,
    display_hash, load_sources, pool_from_columns,
};

#[derive(Debug, Clone, Serialize)]
pub struct CompetitionsPayload {
    pub competitions: Vec<CompetitionRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompetitionRecord {
    pub btc_height: i32,
    pub stale_hash: String,
    /// `canonical.btc_header_time - stale.btc_header_time` (`null` when outside `i32`).
    pub header_time_delta_s: Option<i32>,
    pub stale_header_time: i64,
    pub stale_bitcoin_miner_pool: PoolObject,
    pub canonical_bitcoin_miner_pool: PoolObject,
    pub sources: Vec<String>,
}

pub async fn competitions(client: &Client) -> Result<CompetitionsPayload, ProjectionError> {
    let registry = load_sources(client).await?;
    let bitcoin_source = registry
        .get(BITCOIN_SOURCE_CODE)
        .map(|source| source.code.clone());
    Ok(CompetitionsPayload {
        competitions: load_competition_records(client, bitcoin_source.as_deref()).await?,
    })
}

async fn load_competition_records(
    client: &Client,
    bitcoin_source: Option<&str>,
) -> Result<Vec<CompetitionRecord>> {
    // Sort in Rust by `(btc_height, stale_hash)` so tie-breaking uses reversed display hex
    // rather than stored `bytea` order.
    let sql = format!(
        "SELECT stale.btc_height, stale.btc_header_hash, \
                {COMPETITION_DELTA_SQL} AS header_time_delta_s, \
                stale.btc_header_time, \
                sp.id, sp.slug, sp.canonical_name, \
                cp.id, cp.slug, cp.canonical_name, \
                stale.core_attested, stale.live_observed, \
                (SELECT array_agg(DISTINCT src.code ORDER BY src.code) \
                   FROM attestation_proof ap \
                   JOIN source src ON src.id = ap.source_id \
                  WHERE ap.btc_header_hash = stale.btc_header_hash \
                    AND ap.revoked_at IS NULL) AS proof_sources \
         {COMPETITION_FROM_SQL}"
    );
    let rows = client
        .query(&sql, &[])
        .await
        .context("load stale competitions")?;
    let mut records = rows
        .iter()
        .map(|row| {
            let core_attested: bool = row.get(10);
            let live_observed: bool = row.get(11);
            let proof_sources: Option<Vec<String>> = row.get(12);
            let mut sources = proof_sources.unwrap_or_default();
            let observed = core_attested || live_observed;
            let bitcoin_added = admit_observed_bitcoin_source(
                &mut sources,
                bitcoin_source.map(str::to_owned),
                observed,
                |existing, bitcoin| existing == bitcoin,
            );
            if bitcoin_added {
                sources.sort();
            }
            Ok(CompetitionRecord {
                btc_height: row.get(0),
                stale_hash: display_hash(&row.get::<_, Vec<u8>>(1))?,
                header_time_delta_s: row.get(2),
                stale_header_time: row.get(3),
                stale_bitcoin_miner_pool: pool_from_columns(row.get(4), row.get(5), row.get(6)),
                canonical_bitcoin_miner_pool: pool_from_columns(row.get(7), row.get(8), row.get(9)),
                sources,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    records.sort_by(|a, b| {
        a.btc_height
            .cmp(&b.btc_height)
            .then_with(|| a.stale_hash.cmp(&b.stale_hash))
    });
    Ok(records)
}
