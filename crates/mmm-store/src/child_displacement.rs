//! Child-side displacement: which block the child chain carries at a height.
//!
//! A child chain can replace the block it carries at a height. The event
//! captured for the replaced block is still valid Bitcoin-side evidence, so it
//! is never revoked for that reason; it is marked displaced instead, and every
//! Bitcoin-side aggregate keeps reading it. See `docs/data-model.md`, "Child
//! Displacement".

use anyhow::{Context, Result, ensure};
use tokio_postgres::{Client, GenericClient, Transaction};

/// Two-int advisory-lock class for serializing the per-height transition. The
/// two-int `pg_advisory_xact_lock(int4, int4)` space is disjoint from the
/// one-int space the per-block hash locks use, and the class keeps it apart
/// from the source-health class in the same space.
const CHILD_CHAIN_LOCK_CLASS: i32 = 0x4348; // 'CH' - child chain block at a height

/// What the caller of [`record_child_chain_block`] knows about the current
/// block's Bitcoin parent, which decides how hashless partial observations at
/// the height are treated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentBlockParent<'a> {
    /// The block's proof verified and names this parent (internal byte order).
    /// A hashless row with the same parent is the block; any other hashless
    /// row is a different block and is displaced.
    Known(&'a [u8]),
    /// The block is known to carry no AuxPoW. No hashless AuxPoW observation
    /// can be it, so every hashless row is displaced.
    NoAuxpow,
    /// The block's proof did not verify, so its parent is unknown. Hashless
    /// rows are left untouched: one of them may be this very block, and
    /// displacing it by itself would poison its later promotion.
    Unknown,
}

/// What the producer made of the block it recorded at a height. Stored on
/// the `child_chain_head` row so a later rescan can tell from one block-hash
/// lookup whether the height needs capturing again: only the final outcomes
/// let a rescan that finds the same hash skip the capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildChainHeadOutcome {
    /// An event was written for the block.
    Captured,
    /// The block's proof verified and its verdict is settled, but it yields
    /// no event (a parent that misses Bitcoin's target, for instance).
    Recorded,
    /// The block carries no AuxPoW and yields no event.
    NoAuxpow,
    /// The block's proof did not parse, or its verdict still depends on the
    /// Bitcoin Core cache and may change when the height is observed again.
    Unverified,
    /// The producer holds the cursor at the block; the next observation
    /// captures it again.
    Held,
}

impl ChildChainHeadOutcome {
    /// Whether a rescan that finds the same block hash may skip the capture.
    pub fn is_final(self) -> bool {
        matches!(self, Self::Captured | Self::Recorded | Self::NoAuxpow)
    }

    fn as_db_str(self) -> &'static str {
        match self {
            Self::Captured => "captured",
            Self::Recorded => "recorded",
            Self::NoAuxpow => "non_auxpow",
            Self::Unverified => "unverified",
            Self::Held => "held",
        }
    }

    fn from_db_str(value: &str) -> Result<Self> {
        Ok(match value {
            "captured" => Self::Captured,
            "recorded" => Self::Recorded,
            "non_auxpow" => Self::NoAuxpow,
            "unverified" => Self::Unverified,
            "held" => Self::Held,
            other => anyhow::bail!("unknown child_chain_head outcome {other:?}"),
        })
    }
}

/// The block a child chain last carried at a height, as its producer recorded
/// it: the `child_chain_head` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildChainHead {
    pub block_hash: Vec<u8>,
    /// The parent the block's proof named, `None` when the block yielded no
    /// verified proof. Lets displacement maintenance on a skipped rescan treat
    /// hashless observations the way the original capture did.
    pub btc_parent_header_hash: Option<Vec<u8>>,
    pub outcome: ChildChainHeadOutcome,
    pub observed_at: i64,
}

impl ChildChainHead {
    /// The [`CurrentBlockParent`] the original capture recorded with.
    pub fn current_parent(&self) -> CurrentBlockParent<'_> {
        match (&self.btc_parent_header_hash, self.outcome) {
            (Some(parent), _) => CurrentBlockParent::Known(parent.as_slice()),
            (None, ChildChainHeadOutcome::NoAuxpow) => CurrentBlockParent::NoAuxpow,
            (None, _) => CurrentBlockParent::Unknown,
        }
    }
}

/// Load the block the child chain last carried at `(source_id, child_height)`,
/// or `None` when no producer has processed the height since the row was
/// introduced.
pub async fn load_child_chain_head<C: GenericClient>(
    client: &C,
    source_id: i64,
    child_height: i32,
) -> Result<Option<ChildChainHead>> {
    let row = client
        .query_opt(
            "SELECT block_hash, btc_parent_header_hash, outcome, observed_at \
               FROM child_chain_head \
              WHERE source_id = $1 AND child_height = $2",
            &[&source_id, &child_height],
        )
        .await
        .context("load the child chain head")?;
    row.map(|row| {
        Ok(ChildChainHead {
            block_hash: row.get(0),
            btc_parent_header_hash: row.get(1),
            outcome: ChildChainHeadOutcome::from_db_str(row.get::<_, &str>(2))?,
            observed_at: row.get(3),
        })
    })
    .transpose()
}

/// What one `record_child_chain_block` call changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChildDisplacementOutcome {
    /// Events whose displacement was cleared because their block is the
    /// chain's block at the height again.
    pub restored: u64,
    /// Events newly marked displaced by the current block.
    pub displaced: u64,
}

/// The advisory-lock key for one `(source_id, child_height)`: the class in
/// key one, a hash of the pair in key two.
const CHILD_CHAIN_LOCK_KEY: &str = "$1, hashint8(($2::bigint << 32) | $3::bigint)";

/// Hold the per-height lock at session level across a producer's observation
/// of the child chain and the writes that follow, so that what a producer
/// observed is what it records: two producers overlapping on one height (a
/// live poller and a bounded backfill) observe and write one after the other,
/// and the later observation describes the chain. The transaction-level lock
/// the capture transaction takes on the same key is re-entrant for the
/// session that holds this one. Pair every call with
/// [`finish_child_chain_height_operation`].
pub async fn lock_child_chain_height_session(
    client: &Client,
    source_id: i64,
    child_height: i32,
) -> Result<()> {
    client
        .execute(
            &format!("SELECT pg_advisory_lock({CHILD_CHAIN_LOCK_KEY})"),
            &[
                &CHILD_CHAIN_LOCK_CLASS,
                &source_id,
                &i64::from(child_height),
            ],
        )
        .await
        .context("lock the child chain height for the session")?;
    Ok(())
}

/// Release the session-level height lock and fold the unlock into the
/// operation's result: the operation's error wins when both fail.
pub async fn finish_child_chain_height_operation<T>(
    client: &Client,
    source_id: i64,
    child_height: i32,
    result: Result<T>,
) -> Result<T> {
    let unlock = client
        .execute(
            &format!("SELECT pg_advisory_unlock({CHILD_CHAIN_LOCK_KEY})"),
            &[
                &CHILD_CHAIN_LOCK_CLASS,
                &source_id,
                &i64::from(child_height),
            ],
        )
        .await
        .context("unlock the child chain height for the session");
    match (result, unlock) {
        (Ok(value), Ok(_)) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
}

/// Take the transaction-scoped advisory lock that serializes every change to
/// which block the child chain carries at `(source_id, child_height)`.
///
/// Producers take it before upserting a captured block's event, so that two
/// captures of different blocks at one height contend here first rather than
/// each holding its own event row while waiting for the other; the lock is
/// re-entrant within the transaction, so the later [`record_child_chain_block`]
/// call takes it again without blocking. Released at commit or rollback.
pub async fn lock_child_chain_height(
    txn: &Transaction<'_>,
    source_id: i64,
    child_height: i32,
) -> Result<()> {
    txn.execute(
        &format!("SELECT pg_advisory_xact_lock({CHILD_CHAIN_LOCK_KEY})"),
        &[
            &CHILD_CHAIN_LOCK_CLASS,
            &source_id,
            &i64::from(child_height),
        ],
    )
    .await
    .context("lock the child chain height")?;
    Ok(())
}

/// Record that the child chain now carries `current_block_hash` at
/// `(source_id, child_height)`.
///
/// Clears displacement on the event for that block, which is how a chain that
/// flips back to an earlier block is recorded, and marks every other event at
/// the height that is not yet displaced as displaced by it, with `observed_at`
/// as the displacement time. An event already displaced keeps its first
/// displacement record: the two columns say when a block first left the chain
/// and which block took its place at that moment, and are not a pointer to the
/// chain's current block. The current block is the event with no
/// displacement; when the chain carries a block with no AuxPoW there is no
/// current event, and a later such block changes nothing the columns record.
///
/// A hashless partial observation at the height is treated by what the caller
/// knows about the current block's parent, see [`CurrentBlockParent`]: with a
/// known parent the row with that parent is the block (the identity partial
/// promotion uses) and any other hashless row is displaced; a block that
/// carries no AuxPoW displaces every hashless row, since none can be it; a
/// block whose proof did not verify leaves hashless rows untouched, since the
/// caller cannot tell which block such a row observed. Revoked events are treated the
/// same as active ones: displacement tracks the child chain and revocation
/// tracks evidence validity, and neither reads the other.
///
/// The call runs inside the caller's transaction under the per-height lock
/// from [`lock_child_chain_height`], which it takes itself (re-entrant within
/// the transaction), so two callers recording different blocks at one height
/// serialize and the later commit describes the chain; the transition itself
/// is one UPDATE, so a failure never leaves the height half-moved. It is
/// idempotent, and it changes only the two displacement columns, so no parent
/// read-model reconciliation and no parent advisory lock are needed.
///
/// The call also upserts the `child_chain_head` row for the height (block
/// hash, the parent the proof named, `outcome`, `observed_at`), the durable
/// record a trailing rescan compares one block-hash lookup against; see
/// [`load_child_chain_head`].
///
/// The producer sequence for a captured block is: take the height lock, upsert
/// the block's event, then call this. Taking the lock first matters: an
/// upsert locks its own event row, and two captures of different blocks at
/// one height that each held a row before contending for the height lock
/// would deadlock. A current block that carries no AuxPoW has no event, so
/// its producer calls this alone in a transaction of its own.
///
/// Only an observation of the child chain can say which block is current. A
/// write that inserts a new event at a height without one, such as a
/// historical publication import for a live chain, leaves that event
/// undisplaced beside the recorded current block until the next observation
/// of the height (a poller rescan or a backfill) records the chain again.
pub async fn record_child_chain_block(
    txn: &Transaction<'_>,
    source_id: i64,
    child_height: i32,
    current_block_hash: &[u8],
    current_parent: CurrentBlockParent<'_>,
    outcome: ChildChainHeadOutcome,
    observed_at: i64,
) -> Result<ChildDisplacementOutcome> {
    ensure!(
        current_block_hash.len() == 32,
        "child block hash must be 32 bytes, got {}",
        current_block_hash.len()
    );
    lock_child_chain_height(txn, source_id, child_height).await?;
    let (parent_hash, hashless_decidable) = match current_parent {
        CurrentBlockParent::Known(parent) => (Some(parent), true),
        CurrentBlockParent::NoAuxpow => (None, true),
        CurrentBlockParent::Unknown => (None, false),
    };
    txn.execute(
        "INSERT INTO child_chain_head \
             (source_id, child_height, block_hash, btc_parent_header_hash, outcome, observed_at) \
         VALUES ($1, $2, $3, $4, $5, $6) \
         ON CONFLICT (source_id, child_height) DO UPDATE SET \
             block_hash = EXCLUDED.block_hash, \
             btc_parent_header_hash = EXCLUDED.btc_parent_header_hash, \
             outcome = EXCLUDED.outcome, \
             observed_at = EXCLUDED.observed_at",
        &[
            &source_id,
            &child_height,
            &current_block_hash,
            &parent_hash,
            &outcome.as_db_str(),
            &observed_at,
        ],
    )
    .await
    .context("record the child chain head")?;
    let rows = txn
        .query(
            "WITH candidate AS ( \
                 SELECT id, \
                        (COALESCE(child_block_hash = $3, FALSE) \
                         OR (child_block_hash IS NULL \
                             AND $4::bytea IS NOT NULL \
                             AND btc_parent_header_hash = $4::bytea)) AS is_current \
                 FROM merge_mining_event \
                 WHERE source_id = $1 AND child_height = $2 \
                   AND NOT (child_block_hash IS NULL AND NOT $5::boolean) \
             ) \
             UPDATE merge_mining_event e \
             SET child_displaced_at = CASE WHEN c.is_current THEN NULL ELSE $6::bigint END, \
                 child_displaced_by = CASE WHEN c.is_current THEN NULL ELSE $3::bytea END \
             FROM candidate c \
             WHERE e.id = c.id \
               AND ( \
                 (c.is_current AND e.child_displaced_at IS NOT NULL) \
                 OR (NOT c.is_current AND e.child_displaced_at IS NULL) \
               ) \
             RETURNING e.child_displaced_at IS NULL AS restored",
            &[
                &source_id,
                &child_height,
                &current_block_hash,
                &parent_hash,
                &hashless_decidable,
                &observed_at,
            ],
        )
        .await
        .context("record the child chain's current block")?;
    let restored = rows.iter().filter(|row| row.get::<_, bool>(0)).count() as u64;
    Ok(ChildDisplacementOutcome {
        restored,
        displaced: rows.len() as u64 - restored,
    })
}

/// [`record_child_chain_block`] in a transaction of its own, for a block that
/// yields no event (no AuxPoW, or a proof that failed a gate): there is no
/// event upsert to order it against, and the record takes the per-height lock
/// itself.
pub async fn record_child_chain_block_in_own_transaction(
    client: &mut Client,
    source_id: i64,
    child_height: i32,
    current_block_hash: &[u8],
    current_parent: CurrentBlockParent<'_>,
    head_outcome: ChildChainHeadOutcome,
    observed_at: i64,
) -> Result<ChildDisplacementOutcome> {
    let txn = client
        .transaction()
        .await
        .context("begin child block record")?;
    let outcome = record_child_chain_block(
        &txn,
        source_id,
        child_height,
        current_block_hash,
        current_parent,
        head_outcome,
        observed_at,
    )
    .await?;
    txn.commit().await.context("commit child block record")?;
    Ok(outcome)
}
