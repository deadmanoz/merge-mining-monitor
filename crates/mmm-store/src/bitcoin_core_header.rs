//! Sparse canonical Bitcoin header cache.
//!
//! Rows are observed from the required Bitcoin Core node after the caller has
//! applied its confirmation policy. A disagreement is an integrity error.

use anyhow::{Context, Result, ensure};
use mmm_capture::nbits_table::{BitcoinEpochHeader, NbitsTable};
use tokio_postgres::{Client, GenericClient};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCoreHeader {
    pub height: i32,
    pub block_hash: Vec<u8>,
    pub block_time: i64,
    pub bits: u32,
}

pub async fn record_bitcoin_core_header<C: GenericClient>(
    client: &C,
    header: &BitcoinCoreHeader,
) -> Result<()> {
    ensure!(
        header.block_hash.len() == 32,
        "Bitcoin Core header at height {} has an invalid hash length",
        header.height
    );
    let inserted = client
        .execute(
            "INSERT INTO bitcoin_core_header (height, block_hash, block_time, bits) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (height) DO NOTHING",
            &[
                &header.height,
                &header.block_hash,
                &header.block_time,
                &i64::from(header.bits),
            ],
        )
        .await
        .with_context(|| format!("record Bitcoin Core header at height {}", header.height))?;
    if inserted == 1 {
        return Ok(());
    }

    let existing = client
        .query_one(
            "SELECT block_hash, block_time, bits FROM bitcoin_core_header WHERE height = $1",
            &[&header.height],
        )
        .await
        .with_context(|| {
            format!(
                "load cached Bitcoin Core header at height {}",
                header.height
            )
        })?;
    ensure!(
        existing.get::<_, Vec<u8>>(0) == header.block_hash
            && existing.get::<_, i64>(1) == header.block_time
            && existing.get::<_, i64>(2) == i64::from(header.bits),
        "Bitcoin Core header at height {} disagrees with the persisted canonical header",
        header.height
    );
    Ok(())
}

/// Replace the moving non-epoch horizon while retaining all epoch boundaries.
pub async fn replace_bitcoin_core_header_horizon(
    client: &mut Client,
    header: &BitcoinCoreHeader,
) -> Result<()> {
    let transaction = client
        .transaction()
        .await
        .context("start Core-header-cache horizon replacement")?;
    transaction
        .batch_execute("LOCK TABLE bitcoin_core_header IN SHARE ROW EXCLUSIVE MODE")
        .await
        .context("lock Core-header-cache horizon replacement")?;
    transaction
        .execute(
            "DELETE FROM bitcoin_core_header WHERE height % 2016 <> 0",
            &[],
        )
        .await
        .context("remove superseded Core-header-cache horizon")?;
    record_bitcoin_core_header(&transaction, header).await?;
    transaction
        .commit()
        .await
        .context("commit Core-header-cache horizon replacement")
}

pub async fn load_bitcoin_core_nbits_table<C: GenericClient>(client: &C) -> Result<NbitsTable> {
    let rows = client
        .query(
            "SELECT height, block_time, bits FROM bitcoin_core_header ORDER BY height",
            &[],
        )
        .await
        .context("load cached Bitcoin Core headers")?;
    let headers = rows
        .iter()
        .map(|row| {
            Ok(BitcoinEpochHeader {
                height: row.get(0),
                block_time: row.get(1),
                bits: u32::try_from(row.get::<_, i64>(2))
                    .context("cached Bitcoin Core header bits exceed u32")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    NbitsTable::from_bitcoin_core_headers(&headers)
}

pub async fn highest_bitcoin_core_epoch<C: GenericClient>(client: &C) -> Result<Option<i32>> {
    client
        .query_one(
            "SELECT max(height) FROM bitcoin_core_header WHERE height % 2016 = 0",
            &[],
        )
        .await
        .context("load highest cached Bitcoin Core epoch")
        .map(|row| row.get(0))
}
