//! Sparse canonical Bitcoin header cache.
//!
//! Rows are observed from the required Bitcoin Core node after the caller has
//! applied its confirmation policy. A disagreement is an integrity error.

use anyhow::{Context, Result, ensure};
use mmm_capture::nbits_table::{BitcoinEpochHeader, NbitsTable};
use tokio_postgres::{Client, GenericClient, Row, Transaction};

// Stable, monitor-specific advisory-lock key. The refresh reads a Core snapshot
// before replacing the shallow suffix, so one session must own both operations.
const BITCOIN_CORE_HEADER_CACHE_LOCK: i64 = 0x4d4d4d43_4f524543;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCoreHeader {
    pub height: i32,
    pub block_hash: Vec<u8>,
    pub block_time: i64,
    pub bits: u32,
}

/// The meaningful effect of replacing the mutable cache suffix.
///
/// A changed shallow header can alter previously derived orphan placement, so
/// callers must reclassify existing orphan rows before releasing the cache
/// refresh lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitcoinCoreHeaderCacheUpdate {
    /// Pending rows must be reconsidered against the new persisted Core
    /// coverage before the cache lock is released.
    pub reclassification_needed: bool,
    /// Existing orphan verdicts may have changed because Core replaced the
    /// shallow suffix or added an epoch boundary inside existing timestamp
    /// coverage, so the retry must revisit them as well as pending rows.
    pub recheck_orphans: bool,
}

#[derive(Debug, Clone, Copy)]
struct BitcoinCoreHeaderCacheState {
    horizon_time: i64,
    reclassification_needed: bool,
    orphan_recheck_needed: bool,
}

/// Serialize a Core observation and its cache replacement on this connection.
///
/// A table lock inside the replacement transaction cannot prevent a slower
/// refresh from fetching an older Core snapshot before the faster one commits.
pub async fn lock_bitcoin_core_header_cache(client: &Client) -> Result<()> {
    client
        .query_one(
            "SELECT pg_advisory_lock($1)",
            &[&BITCOIN_CORE_HEADER_CACHE_LOCK],
        )
        .await
        .context("lock Core-header-cache refresh")?;
    Ok(())
}

/// Release the session lock acquired by [`lock_bitcoin_core_header_cache`].
async fn unlock_bitcoin_core_header_cache(client: &Client) -> Result<()> {
    let unlocked: bool = client
        .query_one(
            "SELECT pg_advisory_unlock($1)",
            &[&BITCOIN_CORE_HEADER_CACHE_LOCK],
        )
        .await
        .context("unlock Core-header-cache refresh")?
        .get(0);
    ensure!(unlocked, "Core-header-cache refresh lock was not held");
    Ok(())
}

/// Complete an operation that holds the Core-header-cache advisory lock.
///
/// The lock is always released. When both the operation and unlock fail, the
/// operation remains the primary error and carries the unlock failure as
/// context.
pub async fn finish_bitcoin_core_header_cache_operation<T>(
    client: &Client,
    result: Result<T>,
) -> Result<T> {
    let unlock_result = unlock_bitcoin_core_header_cache(client).await;
    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(unlock_error)) => Err(error.context(format!(
            "also failed to unlock Core header cache: {unlock_error}"
        ))),
    }
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

/// Replace Core observations that are still shallow while retaining final
/// reorg-safe epoch boundaries. `final_epochs` contains every boundary newly
/// verified deeply enough to become final; earlier final rows remain untouched.
pub async fn replace_bitcoin_core_header_cache(
    client: &mut Client,
    final_epoch_height: i32,
    final_epochs: &[BitcoinCoreHeader],
    shallow_epoch: Option<&BitcoinCoreHeader>,
    horizon: &BitcoinCoreHeader,
    prior_horizon_reorged: bool,
) -> Result<BitcoinCoreHeaderCacheUpdate> {
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
    let transaction = client
        .transaction()
        .await
        .context("start Core-header-cache replacement")?;
    transaction
        .batch_execute("LOCK TABLE bitcoin_core_header IN SHARE ROW EXCLUSIVE MODE")
        .await
        .context("lock Core-header-cache replacement")?;
    let previous_state = lock_bitcoin_core_header_cache_state(&transaction).await?;
    let highest_final: Option<i32> = transaction
        .query_one(
            "SELECT max(height) FROM bitcoin_core_header WHERE is_final",
            &[],
        )
        .await
        .context("load highest final Core-header-cache epoch during replacement")?
        .get(0);
    ensure!(
        horizon.height >= final_epoch_height,
        "Bitcoin Core cache horizon must not precede the final cutoff"
    );
    ensure!(
        highest_final.is_none_or(|height| horizon.height >= height),
        "Bitcoin Core cache horizon must not precede the highest finalized epoch"
    );
    let previous_horizon_height: Option<i32> = transaction
        .query_one("SELECT max(height) FROM bitcoin_core_header", &[])
        .await
        .context("load previous Core-header-cache horizon")?
        .get(0);

    let previous_shallow = load_previous_shallow_bitcoin_core_headers(&transaction).await?;
    let incoming = final_epochs
        .iter()
        .chain(shallow_epoch)
        .chain(std::iter::once(horizon))
        .collect::<Vec<_>>();
    let epoch_coverage_overlaps_prior_horizon = cache_epoch_coverage_overlaps_prior_horizon(
        highest_final,
        &previous_shallow,
        &incoming,
        previous_state.horizon_time,
    );
    let shallow_reorged = shallow_cache_reorged(
        &previous_shallow,
        &incoming,
        horizon.height,
        prior_horizon_reorged,
    );

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
        horizon.height == final_epoch_height
            || highest_final.is_some_and(|height| horizon.height == height),
    )
    .await?;
    let update = update_bitcoin_core_header_cache_state(
        &transaction,
        previous_state,
        previous_horizon_height,
        horizon,
        shallow_reorged,
        epoch_coverage_overlaps_prior_horizon,
    )
    .await?;
    transaction
        .commit()
        .await
        .context("commit Core-header-cache replacement")?;
    Ok(update)
}

fn cache_epoch_coverage_overlaps_prior_horizon(
    highest_final: Option<i32>,
    previous_shallow: &[BitcoinCoreHeader],
    incoming: &[&BitcoinCoreHeader],
    previous_horizon_time: i64,
) -> bool {
    let previous_epoch_height = highest_final
        .into_iter()
        .chain(
            previous_shallow
                .iter()
                .filter(|header| header.height % 2016 == 0)
                .map(|header| header.height),
        )
        .max();
    incoming.iter().any(|header| {
        header.height % 2016 == 0
            && previous_epoch_height.is_none_or(|height| header.height > height)
            && header.block_time <= previous_horizon_time
    })
}

fn shallow_cache_reorged(
    previous_shallow: &[BitcoinCoreHeader],
    incoming: &[&BitcoinCoreHeader],
    horizon_height: i32,
    prior_horizon_reorged: bool,
) -> bool {
    prior_horizon_reorged
        || previous_shallow.iter().any(|previous| {
            incoming
                .iter()
                .find(|incoming| incoming.height == previous.height)
                .is_some_and(|incoming| *incoming != previous)
        })
        || previous_shallow
            .iter()
            .map(|header| header.height)
            .max()
            .is_some_and(|height| height > horizon_height)
}

/// Load the highest cached Core observation so a later snapshot can verify
/// that an advancing tip still descends from it.
pub async fn load_bitcoin_core_header_cache_horizon<C: GenericClient>(
    client: &C,
) -> Result<Option<BitcoinCoreHeader>> {
    client
        .query_opt(
            "SELECT height, block_hash, block_time, bits \
             FROM bitcoin_core_header ORDER BY height DESC LIMIT 1",
            &[],
        )
        .await
        .context("load prior Core-header-cache horizon")?
        .map(|row| bitcoin_core_header_from_row(&row))
        .transpose()
}

async fn load_previous_shallow_bitcoin_core_headers(
    transaction: &Transaction<'_>,
) -> Result<Vec<BitcoinCoreHeader>> {
    transaction
        .query(
            "SELECT height, block_hash, block_time, bits \
             FROM bitcoin_core_header WHERE NOT is_final",
            &[],
        )
        .await
        .context("load previous shallow Core-header-cache rows")?
        .into_iter()
        .map(|row| bitcoin_core_header_from_row(&row))
        .collect()
}

fn bitcoin_core_header_from_row(row: &Row) -> Result<BitcoinCoreHeader> {
    Ok(BitcoinCoreHeader {
        height: row.get(0),
        block_hash: row.get(1),
        block_time: row.get(2),
        bits: u32::try_from(row.get::<_, i64>(3))
            .context("cached Bitcoin Core header bits exceed u32")?,
    })
}

async fn lock_bitcoin_core_header_cache_state(
    transaction: &Transaction<'_>,
) -> Result<BitcoinCoreHeaderCacheState> {
    transaction
        .execute(
            "INSERT INTO bitcoin_core_header_cache_state (singleton) VALUES (TRUE) \
             ON CONFLICT (singleton) DO NOTHING",
            &[],
        )
        .await
        .context("initialize Core-header-cache state")?;
    let row = transaction
        .query_one(
            "SELECT horizon_time, reclassification_needed, orphan_recheck_needed \
             FROM bitcoin_core_header_cache_state WHERE singleton FOR UPDATE",
            &[],
        )
        .await
        .context("lock Core-header-cache state")?;
    Ok(BitcoinCoreHeaderCacheState {
        horizon_time: row.get(0),
        reclassification_needed: row.get(1),
        orphan_recheck_needed: row.get(2),
    })
}

async fn update_bitcoin_core_header_cache_state(
    transaction: &Transaction<'_>,
    previous: BitcoinCoreHeaderCacheState,
    previous_horizon_height: Option<i32>,
    horizon: &BitcoinCoreHeader,
    shallow_reorged: bool,
    epoch_coverage_overlaps_prior_horizon: bool,
) -> Result<BitcoinCoreHeaderCacheUpdate> {
    let current_observed_time: i64 = transaction
        .query_one("SELECT max(block_time) FROM bitcoin_core_header", &[])
        .await
        .context("load current Core-header-cache timestamp coverage")?
        .get(0);
    let horizon_advanced = previous_horizon_height.is_none_or(|height| horizon.height > height);
    let horizon_time = if shallow_reorged {
        current_observed_time
    } else {
        previous.horizon_time.max(current_observed_time)
    };
    let recheck_orphans =
        previous.orphan_recheck_needed || shallow_reorged || epoch_coverage_overlaps_prior_horizon;
    let reclassification_needed = previous.reclassification_needed
        || recheck_orphans
        || horizon_advanced
        || horizon_time > previous.horizon_time;
    transaction
        .execute(
            "UPDATE bitcoin_core_header_cache_state \
             SET horizon_time = $1, reclassification_needed = $2, orphan_recheck_needed = $3 \
             WHERE singleton",
            &[&horizon_time, &reclassification_needed, &recheck_orphans],
        )
        .await
        .context("update Core-header-cache state")?;
    Ok(BitcoinCoreHeaderCacheUpdate {
        reclassification_needed,
        recheck_orphans,
    })
}

/// Acknowledge a successful cache reclassification while holding the cache
/// advisory lock. A failed pass leaves both durable retry markers set.
pub async fn complete_bitcoin_core_header_cache_reclassification<C: GenericClient>(
    client: &C,
) -> Result<()> {
    let updated = client
        .execute(
            "UPDATE bitcoin_core_header_cache_state \
             SET reclassification_needed = FALSE, orphan_recheck_needed = FALSE \
             WHERE singleton",
            &[],
        )
        .await
        .context("mark Core-header-cache reclassification complete")?;
    ensure!(updated == 1, "Core-header-cache state is missing");
    Ok(())
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
            "SELECT h.height, h.block_time, h.bits, s.horizon_time \
             FROM bitcoin_core_header h \
             JOIN bitcoin_core_header_cache_state s ON s.singleton \
             ORDER BY h.height",
            &[],
        )
        .await
        .context("load cached Bitcoin Core headers and timestamp coverage")?;
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
    let cached_horizon_time: i64 = rows[0].get(3);
    NbitsTable::from_bitcoin_core_headers_with_horizon_time(&headers, cached_horizon_time).map(Some)
}

/// Load the initialized cache for a command that requires nBits classification.
pub async fn load_bitcoin_core_nbits_table<C: GenericClient>(client: &C) -> Result<NbitsTable> {
    load_bitcoin_core_nbits_table_if_present(client)
        .await?
        .context("Bitcoin Core header cache is empty")
}

/// Highest epoch boundary that has already been verified at the reorg-safe
/// depth and made final.
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
