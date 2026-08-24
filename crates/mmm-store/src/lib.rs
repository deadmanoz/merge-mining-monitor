//! Producer-side base-table SQL: event upserts, chain sidecars, poll
//! cursor, source/pool seeding. Owns every producer-facing SQL statement
//! against the base tables and never writes a derived table (the
//! reconciler-authorized lockstep mutations of merge_mining_event live in
//! mmm-read-model).
//!
//! `lib.rs` re-exports the stable public API
//! (`mmm_store::fn`). Shared, table-generic SQL lives in root modules
//! (`event`, `pool`, `poll_cursor`, `pending_reconcile`); chain-specific SQL
//! (capture writers, 1:1 evidence sidecars, per-chain event reads, the RSK
//! pool-identity adapters) lives under `chains/<chain>.rs`. A new merge-mined
//! chain is a new `chains/<chain>.rs`, not an append to one god file.

mod bitcoin_core_header;
mod chains;
mod event;
mod known_stale;
mod pending_reconcile;
mod poll_cursor;
mod pool;

pub use bitcoin_core_header::{
    BitcoinCoreHeader, BitcoinCoreHeaderCacheUpdate,
    complete_bitcoin_core_header_cache_reclassification,
    finish_bitcoin_core_header_cache_operation, highest_final_bitcoin_core_epoch,
    load_bitcoin_core_nbits_table, load_bitcoin_core_nbits_table_if_present,
    lock_bitcoin_core_header_cache, record_bitcoin_core_header, replace_bitcoin_core_header_cache,
};
pub use chains::elastos::{
    ElastosIdentityReresolveRow, active_event_ids_at_height, load_elastos_identity_reresolve_batch,
    write_elastos_capture_in_txn,
};
pub use chains::hathor::{
    HathorEventRow, hathor_events_at_height, load_hathor_reward_replay_batch,
    update_hathor_reward_audit, write_hathor_capture_in_txn,
};
#[cfg(any(test, feature = "db-integration"))]
pub use chains::rsk::write_rsk_capture;
pub use chains::rsk::{
    late_fill_rsk_pool_identity_id, upsert_rsk_only_pools, upsert_rsk_pool_identities,
    upsert_rsk_pool_identities_with_policy, write_rsk_capture_in_txn,
};
pub use event::{
    EventWriteDisposition, EventWriteOutcome, delete_event_pool_attributions_for_source,
    fill_event_child_coinbase, upsert_event_pool_attributions,
    upsert_event_pool_attributions_without_stale_cleanup, upsert_merge_mining_event,
    upsert_merge_mining_event_with_attributions,
};
pub use known_stale::{count_known_stale_blocks, is_known_stale_hash, upsert_known_stale_block};
pub use pending_reconcile::{
    PendingReconcileRow, bump_pending_attempts, delete_pending_reconcile,
    delete_pending_reconcile_at, list_pending_reconcile, retag_revocation_reason,
    upsert_pending_reconcile,
};
pub use poll_cursor::{get_source_id, load_poll_cursor, upsert_poll_cursor_with_target};
pub use pool::{
    PoolIdentitySeed, load_pool_identities_by_namespace, upsert_identity_registry,
    upsert_pool_identities_for_namespace, upsert_pool_snapshot, upsert_registry_only_pools,
};
