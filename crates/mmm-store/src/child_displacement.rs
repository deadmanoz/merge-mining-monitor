//! Child-side displacement: which block the child chain carries at a height.
//!
//! A child chain can replace the block it carries at a height. The event
//! captured for the replaced block is still valid Bitcoin-side evidence, so it
//! is never revoked for that reason; it is marked displaced instead, and every
//! Bitcoin-side aggregate keeps reading it. See `docs/data-model.md`, "Child
//! Displacement".

use anyhow::{Context, Result, ensure};
use tokio_postgres::Transaction;

/// Two-int advisory-lock class for serializing the per-height transition. The
/// two-int `pg_advisory_xact_lock(int4, int4)` space is disjoint from the
/// one-int space the per-block hash locks use, and the class keeps it apart
/// from the source-health class in the same space.
const CHILD_CHAIN_LOCK_CLASS: i32 = 0x4348; // 'CH' - child chain block at a height

/// What one `record_child_chain_block` call changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChildDisplacementOutcome {
    /// Events whose displacement was cleared because their block is the
    /// chain's block at the height again.
    pub restored: u64,
    /// Events newly marked displaced by the current block.
    pub displaced: u64,
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
        "SELECT pg_advisory_xact_lock($1, hashint8(($2::bigint << 32) | $3::bigint))",
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
/// Hashless partial observations at the height are displaced too. A partial
/// observation of the current block would have been promoted to its exact
/// identity by the capture upsert that precedes this call, so a hashless row
/// that remains belongs to a different block. Revoked events are treated the
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
    observed_at: i64,
) -> Result<ChildDisplacementOutcome> {
    ensure!(
        current_block_hash.len() == 32,
        "child block hash must be 32 bytes, got {}",
        current_block_hash.len()
    );
    lock_child_chain_height(txn, source_id, child_height).await?;
    let rows = txn
        .query(
            "UPDATE merge_mining_event \
             SET child_displaced_at = CASE WHEN child_block_hash = $3 THEN NULL ELSE $4::bigint END, \
                 child_displaced_by = CASE WHEN child_block_hash = $3 THEN NULL ELSE $3::bytea END \
             WHERE source_id = $1 AND child_height = $2 \
               AND ( \
                 (child_block_hash = $3 AND child_displaced_at IS NOT NULL) \
                 OR ( \
                   (child_block_hash IS NULL OR child_block_hash <> $3) \
                   AND child_displaced_at IS NULL \
                 ) \
               ) \
             RETURNING child_displaced_at IS NULL AS restored",
            &[&source_id, &child_height, &current_block_hash, &observed_at],
        )
        .await
        .context("record the child chain's current block")?;
    let restored = rows.iter().filter(|row| row.get::<_, bool>(0)).count() as u64;
    Ok(ChildDisplacementOutcome {
        restored,
        displaced: rows.len() as u64 - restored,
    })
}
