//! Child-side displacement: which block the child chain carries at a height.
//!
//! A child chain can replace the block it carries at a height. The event
//! captured for the replaced block is still valid Bitcoin-side evidence, so it
//! is never revoked for that reason; it is marked displaced instead, and every
//! Bitcoin-side aggregate keeps reading it. See `docs/data-model.md`, "Child
//! Displacement".

use anyhow::{Context, Result, ensure};
use tokio_postgres::GenericClient;

/// What one `record_child_chain_block` call changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChildDisplacementOutcome {
    /// Events whose displacement was cleared because their block is the
    /// chain's block at the height again.
    pub restored: u64,
    /// Events newly marked displaced by the current block.
    pub displaced: u64,
}

/// Record that the child chain now carries `current_block_hash` at
/// `(source_id, child_height)`.
///
/// Clears displacement on the event for that block, which is how a chain that
/// flips back to an earlier block is recorded, and marks every other event at
/// the height that is not yet displaced as displaced by it, with `observed_at`
/// as the displacement time. An event already displaced keeps its first
/// displacement record, so the two columns say when a block first left the
/// chain and which block took its place at that moment; the chain's current
/// block at a height is the event with no displacement.
///
/// Hashless partial observations at the height are displaced too. A partial
/// observation of the current block would have been promoted to its exact
/// identity by the capture upsert that precedes this call, so a hashless row
/// that remains belongs to a different block. Revoked events are treated the
/// same as active ones: displacement tracks the child chain and revocation
/// tracks evidence validity, and neither reads the other.
///
/// The transition is one UPDATE, so it is atomic under autocommit as well as
/// inside a transaction: a failure can never leave the height half-moved, and
/// two callers recording different blocks at the same height serialize on the
/// row locks, with the later commit describing the chain. Idempotent: repeating
/// the call for the same block changes nothing. Only the two displacement
/// columns change, so no parent read-model reconciliation and no parent
/// advisory lock are needed. Producers call it inside the capture transaction
/// after the current block's event has been upserted, or on its own when the
/// current block carries no AuxPoW and has no event.
pub async fn record_child_chain_block<C: GenericClient>(
    client: &C,
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
    let rows = client
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
