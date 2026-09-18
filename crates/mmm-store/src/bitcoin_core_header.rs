//! Sparse canonical Bitcoin header cache.
//!
//! Rows are observed from the required Bitcoin Core node after the caller has
//! applied its confirmation policy. A disagreement is an integrity error.

use anyhow::{Context, Error, Result, ensure};
use mmm_capture::nbits_table::{BitcoinEpochHeader, NbitsTable};
use tokio_postgres::{Client, GenericClient, Row, Transaction};

// Stable, monitor-specific advisory-lock key. The refresh reads a Core snapshot
// before replacing the shallow suffix, so one session must own both operations.
const BITCOIN_CORE_HEADER_CACHE_LOCK: i64 = 0x4d4d4d43_4f524543;

/// Persisted Core-header evidence is internally inconsistent and cannot be
/// repaired by retrying the same monitor operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitcoinCoreHeaderCacheIntegrityError;

impl std::fmt::Display for BitcoinCoreHeaderCacheIntegrityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bitcoin Core header cache integrity failed")
    }
}

impl std::error::Error for BitcoinCoreHeaderCacheIntegrityError {}

/// Whether `err` reports a non-retryable Core-header-cache integrity failure.
pub fn is_bitcoin_core_header_cache_integrity_error(err: &Error) -> bool {
    err.downcast_ref::<BitcoinCoreHeaderCacheIntegrityError>()
        .is_some()
}

/// Build a non-retryable Core-header-cache integrity error with `message` as
/// its operator-facing detail.
pub fn bitcoin_core_header_cache_integrity_error(message: impl Into<String>) -> Error {
    Error::new(BitcoinCoreHeaderCacheIntegrityError).context(message.into())
}

fn ensure_bitcoin_core_header_cache_integrity(
    condition: bool,
    message: impl Into<String>,
) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(bitcoin_core_header_cache_integrity_error(message))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCoreHeader {
    pub height: i32,
    pub block_hash: Vec<u8>,
    pub block_time: i64,
    pub bits: u32,
}

/// The meaningful effect of replacing the mutable cache suffix: whether the
/// replacement scheduled unknown-parent reclassification work, and what is
/// pending afterwards.
///
/// A changed shallow header can alter previously derived orphan placement,
/// and a horizon advance can settle rows that had no verdict. Neither is
/// done inside the refresh: the refresh increments the pending generation
/// and the scheduled recheck job (`reclassify-unknown-parents --scheduled`)
/// consumes it one page per cache-lock hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitcoinCoreHeaderCacheUpdate {
    /// This replacement scheduled a recheck (the pending generation moved).
    pub scheduled: bool,
    /// The pending scope revisits already classified orphans, not only rows
    /// with no orphan class yet.
    pub pending_orphans: bool,
    /// Work is pending: the pending generation exceeds the acknowledged one.
    pub pending: bool,
}

#[derive(Debug, Clone)]
struct BitcoinCoreHeaderCacheState {
    horizon_time: i64,
    pending_generation: i64,
    pending_orphans: bool,
    pending_sources: Option<Vec<String>>,
    acknowledged_generation: i64,
}

/// What a scheduled recheck must cover: whether already classified orphans
/// are revisited, and which witness sources' events are candidates (`None`
/// is every source; an empty list is none, the state a bind leaves behind).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecheckScope {
    pub orphans: bool,
    pub sources: Option<Vec<String>>,
}

impl RecheckScope {
    /// Every source, orphans included: what an invalidating cache change
    /// schedules.
    pub fn everything() -> Self {
        Self {
            orphans: true,
            sources: None,
        }
    }

    /// Every source, rows without a verdict only: what a horizon advance
    /// schedules.
    pub fn pending_rows() -> Self {
        Self {
            orphans: false,
            sources: None,
        }
    }

    /// Nothing: the pending scope right after a bind consumed it.
    fn nothing() -> Self {
        Self {
            orphans: false,
            sources: Some(Vec::new()),
        }
    }

    /// Widen this scope by another: orphans by OR, sources by union where
    /// `None` (every source) absorbs any list.
    fn union(&self, other: &RecheckScope) -> RecheckScope {
        let sources = match (&self.sources, &other.sources) {
            (Some(mine), Some(theirs)) => {
                let mut all = mine.clone();
                all.extend(theirs.iter().filter(|s| !mine.contains(s)).cloned());
                Some(all)
            }
            _ => None,
        };
        RecheckScope {
            orphans: self.orphans || other.orphans,
            sources,
        }
    }
}

/// The pass a scheduled recheck job has bound to a generation, with the
/// keyset cursor of the last completed page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecheckPass {
    pub generation: i64,
    pub scope: RecheckScope,
    pub cursor: Option<(i64, i64)>,
}

/// The scheduled-recheck state of the Core-header cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduledRecheck {
    pub pending_generation: i64,
    /// What has been scheduled since the last bind, including the scope of a
    /// pass an invalidating trigger folded back in.
    pub pending_scope: RecheckScope,
    pub acknowledged_generation: i64,
    /// The pass in flight, which continues from its cursor: an invalidating
    /// trigger clears it, an additive one (a horizon advance) leaves it.
    pub pass: Option<RecheckPass>,
}

impl ScheduledRecheck {
    /// Work is pending while the pending generation exceeds the acknowledged
    /// one.
    pub fn is_pending(&self) -> bool {
        self.pending_generation > self.acknowledged_generation
    }
}

/// Read the scheduled-recheck state. Callers that go on to bind or advance a
/// pass hold the exclusive cache lock, which serializes them against the
/// refresh that schedules work.
pub async fn load_scheduled_recheck<C: GenericClient>(client: &C) -> Result<ScheduledRecheck> {
    let row = client
        .query_one(
            "SELECT recheck_pending_generation, recheck_pending_orphans, \
                    recheck_pending_sources, recheck_acknowledged_generation, \
                    recheck_pass_generation, recheck_pass_orphans, recheck_pass_sources, \
                    recheck_cursor_height, recheck_cursor_id \
             FROM bitcoin_core_header_cache_state WHERE singleton",
            &[],
        )
        .await
        .context("load scheduled Core-cache recheck state")?;
    let pass_generation: Option<i64> = row.get(4);
    let cursor_height: Option<i64> = row.get(7);
    let cursor_id: Option<i64> = row.get(8);
    Ok(ScheduledRecheck {
        pending_generation: row.get(0),
        pending_scope: RecheckScope {
            orphans: row.get(1),
            sources: row.get(2),
        },
        acknowledged_generation: row.get(3),
        pass: pass_generation.map(|generation| RecheckPass {
            generation,
            scope: RecheckScope {
                orphans: row.get::<_, Option<bool>>(5).unwrap_or(false),
                sources: row.get(6),
            },
            cursor: cursor_height.zip(cursor_id),
        }),
    })
}

/// Bind a pass to the pending generation with a fresh cursor, consuming the
/// pending scope `scope` (what was scheduled since the last bind, as the
/// caller just loaded it under the exclusive cache lock); triggers that
/// arrive later accumulate for the follow-up pass.
pub async fn bind_recheck_pass<C: GenericClient>(
    client: &C,
    scope: RecheckScope,
) -> Result<RecheckPass> {
    let nothing = RecheckScope::nothing();
    let row = client
        .query_one(
            "UPDATE bitcoin_core_header_cache_state \
                SET recheck_pass_generation = recheck_pending_generation, \
                    recheck_pass_orphans = $1, recheck_pass_sources = $2, \
                    recheck_pending_orphans = $3, recheck_pending_sources = $4, \
                    recheck_cursor_height = NULL, recheck_cursor_id = NULL \
              WHERE singleton \
          RETURNING recheck_pass_generation",
            &[
                &scope.orphans,
                &scope.sources,
                &nothing.orphans,
                &nothing.sources,
            ],
        )
        .await
        .context("bind the scheduled Core-cache recheck pass")?;
    Ok(RecheckPass {
        generation: row.get(0),
        scope,
        cursor: None,
    })
}

/// Persist the keyset cursor of the last completed page of the bound pass.
pub async fn persist_recheck_cursor<C: GenericClient>(
    client: &C,
    generation: i64,
    cursor: (i64, i64),
) -> Result<()> {
    let updated = client
        .execute(
            "UPDATE bitcoin_core_header_cache_state \
                SET recheck_cursor_height = $2, recheck_cursor_id = $3 \
              WHERE singleton AND recheck_pass_generation = $1",
            &[&generation, &cursor.0, &cursor.1],
        )
        .await
        .context("persist the scheduled Core-cache recheck cursor")?;
    ensure_bitcoin_core_header_cache_integrity(
        updated == 1,
        "the scheduled recheck pass is no longer bound to its generation",
    )
}

/// Acknowledge the bound pass as complete: its generation becomes the
/// acknowledged one and the pass is cleared. Additive triggers that arrived
/// during the pass keep the pending generation ahead, and the caller binds a
/// follow-up pass over the scope accumulated since. Fails closed when no
/// pass is bound to that generation.
pub async fn acknowledge_recheck_pass<C: GenericClient>(client: &C, generation: i64) -> Result<()> {
    let updated = client
        .execute(
            "UPDATE bitcoin_core_header_cache_state \
                SET recheck_acknowledged_generation = $1, \
                    recheck_pass_generation = NULL, recheck_pass_orphans = NULL, \
                    recheck_pass_sources = NULL, \
                    recheck_cursor_height = NULL, recheck_cursor_id = NULL \
              WHERE singleton AND recheck_pass_generation = $1",
            &[&generation],
        )
        .await
        .context("acknowledge the scheduled Core-cache recheck pass")?;
    ensure_bitcoin_core_header_cache_integrity(
        updated == 1,
        "the scheduled recheck pass is no longer bound to its generation",
    )
}

/// Schedule a recheck by hand (a repair or a test): increments the pending
/// generation and widens the pending scope by `scope`. `invalidating` says
/// verdicts already given may change: a pass in flight is cleared and its
/// scope folded back into the pending scope, so the next bind starts over
/// with the merged scope. Migrations do the same in SQL.
pub async fn schedule_core_recheck<C: GenericClient>(
    client: &C,
    scope: &RecheckScope,
    invalidating: bool,
) -> Result<i64> {
    let row = client
        .query_one(
            "UPDATE bitcoin_core_header_cache_state \
                SET recheck_pending_generation = recheck_pending_generation + 1, \
                    recheck_pending_orphans = recheck_pending_orphans OR $1 \
                        OR ($3 AND COALESCE(recheck_pass_orphans, FALSE)), \
                    recheck_pending_sources = CASE \
                        WHEN recheck_pending_sources IS NULL OR $2::text[] IS NULL \
                             OR ($3 AND recheck_pass_generation IS NOT NULL \
                                 AND recheck_pass_sources IS NULL) THEN NULL \
                        ELSE (SELECT COALESCE(array_agg(DISTINCT s), '{}') \
                                FROM unnest(recheck_pending_sources || $2::text[] \
                                            || CASE WHEN $3 THEN COALESCE(recheck_pass_sources, '{}') \
                                                    ELSE '{}' END) AS s) END, \
                    recheck_pass_generation = CASE WHEN $3 THEN NULL ELSE recheck_pass_generation END, \
                    recheck_pass_orphans = CASE WHEN $3 THEN NULL ELSE recheck_pass_orphans END, \
                    recheck_pass_sources = CASE WHEN $3 THEN NULL ELSE recheck_pass_sources END, \
                    recheck_cursor_height = CASE WHEN $3 THEN NULL ELSE recheck_cursor_height END, \
                    recheck_cursor_id = CASE WHEN $3 THEN NULL ELSE recheck_cursor_id END \
              WHERE singleton \
          RETURNING recheck_pending_generation",
            &[&scope.orphans, &scope.sources, &invalidating],
        )
        .await
        .context("schedule a Core-cache recheck")?;
    Ok(row.get(0))
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

/// Hold a shared cache lock across a classification and its dependent write.
///
/// A refresh holds the exclusive counterpart through its replacement and
/// reclassification sweep. Readers therefore cannot commit a verdict from an
/// old cache after that sweep has acknowledged the new one.
pub async fn lock_bitcoin_core_header_cache_shared(client: &Client) -> Result<()> {
    client
        .query_one(
            "SELECT pg_advisory_lock_shared($1)",
            &[&BITCOIN_CORE_HEADER_CACHE_LOCK],
        )
        .await
        .context("lock Core-header-cache classification")?;
    Ok(())
}

/// Hold the shared cache lock until the caller's transaction completes.
///
/// This is deliberately generic so read-model transactions can acquire it
/// without exposing their concrete transaction type to the store crate.
pub async fn lock_bitcoin_core_header_cache_shared_in_transaction<C: GenericClient>(
    client: &C,
) -> Result<()> {
    client
        .query_one(
            "SELECT pg_advisory_xact_lock_shared($1)",
            &[&BITCOIN_CORE_HEADER_CACHE_LOCK],
        )
        .await
        .context("lock Core-header-cache classification transaction")?;
    Ok(())
}

/// Release the session lock acquired by [`lock_bitcoin_core_header_cache`].
async fn unlock_bitcoin_core_header_cache(client: &Client, shared: bool) -> Result<()> {
    let function = if shared {
        "pg_advisory_unlock_shared"
    } else {
        "pg_advisory_unlock"
    };
    let unlocked: bool = client
        .query_one(
            &format!("SELECT {function}($1)"),
            &[&BITCOIN_CORE_HEADER_CACHE_LOCK],
        )
        .await
        .context("unlock Core-header-cache operation")?
        .get(0);
    ensure!(unlocked, "Core-header-cache operation lock was not held");
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
    finish_bitcoin_core_header_cache_lock_operation(client, result, false).await
}

/// Complete an operation that holds the shared Core-header-cache lock.
pub async fn finish_bitcoin_core_header_cache_shared_operation<T>(
    client: &Client,
    result: Result<T>,
) -> Result<T> {
    finish_bitcoin_core_header_cache_lock_operation(client, result, true).await
}

async fn finish_bitcoin_core_header_cache_lock_operation<T>(
    client: &Client,
    result: Result<T>,
    shared: bool,
) -> Result<T> {
    let unlock_result = unlock_bitcoin_core_header_cache(client, shared).await;
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
    ensure_bitcoin_core_header_cache_integrity(
        header.block_hash.len() == 32,
        format!(
            "Bitcoin Core header at height {} has an invalid hash length",
            header.height
        ),
    )?;
    ensure_bitcoin_core_header_cache_integrity(
        !is_final || header.height % 2016 == 0,
        "only a Bitcoin difficulty boundary can be final",
    )?;
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
    ensure_bitcoin_core_header_cache_integrity(
        existing.get::<_, Vec<u8>>(0) == header.block_hash
            && existing.get::<_, i64>(1) == header.block_time
            && existing.get::<_, i64>(2) == i64::from(header.bits)
            && existing.get::<_, bool>(3) == is_final,
        format!(
            "Bitcoin Core header at height {} disagrees with the persisted canonical header",
            header.height
        ),
    )?;
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
    ensure_bitcoin_core_header_cache_integrity(
        final_epochs
            .iter()
            .all(|header| header.height % 2016 == 0 && header.height <= final_epoch_height),
        "final Bitcoin Core headers must be difficulty boundaries at or below the cutoff",
    )?;
    if let Some(header) = shallow_epoch {
        ensure_bitcoin_core_header_cache_integrity(
            header.height % 2016 == 0 && header.height > final_epoch_height,
            "shallow Bitcoin Core header must be a boundary above the cutoff",
        )?;
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
    ensure_bitcoin_core_header_cache_integrity(
        horizon.height >= final_epoch_height,
        "Bitcoin Core cache horizon must not precede the final cutoff",
    )?;
    ensure_bitcoin_core_header_cache_integrity(
        highest_final.is_none_or(|height| horizon.height >= height),
        "Bitcoin Core cache horizon must not precede the highest finalized epoch",
    )?;
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
        bits: u32::try_from(row.get::<_, i64>(3)).map_err(|_| {
            bitcoin_core_header_cache_integrity_error("cached Bitcoin Core header bits exceed u32")
        })?,
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
            "SELECT horizon_time, recheck_pending_generation, recheck_pending_orphans, \
                    recheck_pending_sources, recheck_acknowledged_generation \
             FROM bitcoin_core_header_cache_state WHERE singleton FOR UPDATE",
            &[],
        )
        .await
        .context("lock Core-header-cache state")?;
    Ok(BitcoinCoreHeaderCacheState {
        horizon_time: row.get(0),
        pending_generation: row.get(1),
        pending_orphans: row.get(2),
        pending_sources: row.get(3),
        acknowledged_generation: row.get(4),
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
    let cache_was_empty = previous_horizon_height.is_none();
    let horizon_time = if shallow_reorged {
        current_observed_time
    } else {
        previous.horizon_time.max(current_observed_time)
    };
    // A shallow reorg, a boundary inside existing coverage, or an empty cache
    // can change verdicts already given: that clears a pass in flight (its
    // scope is covered by the everything the trigger schedules) and revisits
    // classified orphans. A plain horizon advance only lets rows without a
    // verdict be decided: additive, so a pass in flight continues.
    let invalidating = shallow_reorged || epoch_coverage_overlaps_prior_horizon || cache_was_empty;
    let schedule = invalidating || horizon_advanced || horizon_time > previous.horizon_time;
    // A replaced shallow boundary or a boundary inside existing coverage can
    // change verdicts already given, so every child-chain head recorded under
    // the previous cache generation stops being final for a rescan.
    let verdicts_may_change = shallow_reorged || epoch_coverage_overlaps_prior_horizon;
    let accumulated = RecheckScope {
        orphans: previous.pending_orphans,
        sources: previous.pending_sources.clone(),
    };
    let pending_scope = if schedule && invalidating {
        accumulated.union(&RecheckScope::everything())
    } else if schedule {
        accumulated.union(&RecheckScope::pending_rows())
    } else {
        accumulated
    };
    let pending_generation = if schedule {
        previous.pending_generation + 1
    } else {
        previous.pending_generation
    };
    transaction
        .execute(
            "UPDATE bitcoin_core_header_cache_state \
             SET horizon_time = $1, recheck_pending_generation = $2, \
                 recheck_pending_orphans = $3, recheck_pending_sources = $4, \
                 recheck_pass_generation = CASE WHEN $5 THEN NULL ELSE recheck_pass_generation END, \
                 recheck_pass_orphans = CASE WHEN $5 THEN NULL ELSE recheck_pass_orphans END, \
                 recheck_pass_sources = CASE WHEN $5 THEN NULL ELSE recheck_pass_sources END, \
                 recheck_cursor_height = CASE WHEN $5 THEN NULL ELSE recheck_cursor_height END, \
                 recheck_cursor_id = CASE WHEN $5 THEN NULL ELSE recheck_cursor_id END, \
                 core_cache_generation = core_cache_generation + CASE WHEN $6 THEN 1 ELSE 0 END \
             WHERE singleton",
            &[
                &horizon_time,
                &pending_generation,
                &pending_scope.orphans,
                &pending_scope.sources,
                &invalidating,
                &verdicts_may_change,
            ],
        )
        .await
        .context("update Core-header-cache state")?;
    Ok(BitcoinCoreHeaderCacheUpdate {
        scheduled: schedule,
        pending_orphans: pending_scope.orphans,
        pending: pending_generation > previous.acknowledged_generation,
    })
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
                bits: u32::try_from(row.get::<_, i64>(2)).map_err(|_| {
                    bitcoin_core_header_cache_integrity_error(
                        "cached Bitcoin Core header bits exceed u32",
                    )
                })?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if headers.is_empty() {
        return Ok(None);
    }
    let cached_horizon_time: i64 = rows[0].get(3);
    NbitsTable::from_bitcoin_core_headers_with_horizon_time(&headers, cached_horizon_time)
        .map(Some)
        .map_err(|err| bitcoin_core_header_cache_integrity_error(err.to_string()))
}

/// Load the initialized cache for a command that requires nBits classification.
pub async fn load_bitcoin_core_nbits_table<C: GenericClient>(client: &C) -> Result<NbitsTable> {
    load_bitcoin_core_nbits_table_if_present(client)
        .await?
        .ok_or_else(|| {
            bitcoin_core_header_cache_integrity_error("Bitcoin Core header cache is empty")
        })
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
