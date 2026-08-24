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
    record_bitcoin_core_header_with_finality(client, header, header.height % 2016 == 0).await
}

async fn record_bitcoin_core_header_with_finality<C: GenericClient>(
    client: &C,
    header: &BitcoinCoreHeader,
    is_final: bool,
) -> Result<()> {
    ensure!(
        header.block_hash.len() == 32,
        "Bitcoin Core header at height {} has an invalid hash length",
        header.height
    );
    ensure!(
        !is_final || header.height % 2016 == 0,
        "only a Bitcoin difficulty boundary can be final"
    );
    let inserted = client
        .execute(
            "INSERT INTO bitcoin_core_header (height, block_hash, block_time, bits, is_final) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (height) DO NOTHING",
            &[
                &header.height,
                &header.block_hash,
                &header.block_time,
                &i64::from(header.bits),
                &is_final,
            ],
        )
        .await
        .with_context(|| format!("record Bitcoin Core header at height {}", header.height))?;
    if inserted == 1 {
        return Ok(());
    }

    let existing = client
        .query_one(
            "SELECT block_hash, block_time, bits, is_final \
             FROM bitcoin_core_header WHERE height = $1",
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
            && existing.get::<_, i64>(2) == i64::from(header.bits)
            && existing.get::<_, bool>(3) == is_final,
        "Bitcoin Core header at height {} disagrees with the persisted canonical header",
        header.height
    );
    Ok(())
}

/// Replace Core observations that are still shallow while retaining immutable,
/// reorg-safe epoch boundaries. `final_epochs` contains every boundary newly
/// verified deeply enough to become final; earlier final rows remain untouched.
pub async fn replace_bitcoin_core_header_cache(
    client: &mut Client,
    final_epoch_height: i32,
    final_epochs: &[BitcoinCoreHeader],
    shallow_epoch: Option<&BitcoinCoreHeader>,
    horizon: &BitcoinCoreHeader,
) -> Result<()> {
    ensure!(
        final_epochs
            .iter()
            .all(|header| header.height % 2016 == 0 && header.height <= final_epoch_height),
        "final Bitcoin Core headers must be difficulty boundaries at or below the cutoff"
    );
    if let Some(header) = shallow_epoch {
        ensure!(
            header.height % 2016 == 0 && header.height > final_epoch_height,
            "shallow Bitcoin Core header must be a boundary above the cutoff"
        );
    }
    ensure!(
        horizon.height >= final_epoch_height,
        "Bitcoin Core cache horizon must not precede the final cutoff"
    );
    let transaction = client
        .transaction()
        .await
        .context("start Core-header-cache replacement")?;
    transaction
        .batch_execute("LOCK TABLE bitcoin_core_header IN SHARE ROW EXCLUSIVE MODE")
        .await
        .context("lock Core-header-cache replacement")?;
    transaction
        .execute("DELETE FROM bitcoin_core_header WHERE NOT is_final", &[])
        .await
        .context("remove shallow Core-header-cache rows")?;
    for header in final_epochs {
        record_bitcoin_core_header_with_finality(&transaction, header, true).await?;
    }
    if let Some(header) = shallow_epoch {
        record_bitcoin_core_header_with_finality(&transaction, header, false).await?;
    }
    record_bitcoin_core_header_with_finality(
        &transaction,
        horizon,
        horizon.height == final_epoch_height,
    )
    .await?;
    transaction
        .commit()
        .await
        .context("commit Core-header-cache replacement")
}

/// Load the cache when it has been initialized by a Core-backed command.
///
/// Read-only API handlers use `None` to degrade optional placement rather than
/// failing during the migration-to-first-refresh interval.
pub async fn load_bitcoin_core_nbits_table_if_present<C: GenericClient>(
    client: &C,
) -> Result<Option<NbitsTable>> {
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
    if headers.is_empty() {
        return Ok(None);
    }
    NbitsTable::from_bitcoin_core_headers(&headers).map(Some)
}

/// Load the initialized cache for a command that requires nBits classification.
pub async fn load_bitcoin_core_nbits_table<C: GenericClient>(client: &C) -> Result<NbitsTable> {
    load_bitcoin_core_nbits_table_if_present(client)
        .await?
        .context("Bitcoin Core header cache is empty")
}

/// Highest epoch boundary that has already been verified at the reorg-safe
/// depth and made immutable.
pub async fn highest_final_bitcoin_core_epoch<C: GenericClient>(client: &C) -> Result<Option<i32>> {
    client
        .query_one(
            "SELECT max(height) FROM bitcoin_core_header \
             WHERE is_final",
            &[],
        )
        .await
        .context("load highest final Bitcoin Core epoch")
        .map(|row| row.get(0))
}
