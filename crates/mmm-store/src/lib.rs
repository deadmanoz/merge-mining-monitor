//! Producer-side base-table SQL (`merge_mining_event`, child block observations, 1:1 chain
//! sidecars, attributions, cursors, capture errors, child-chain heads, Core header cache, and
//! the relationship refresh queue).
//! Never writes derived tables.

mod bitcoin_core_header;
mod capture_error;
mod capture_write;
mod chains;
mod child_displacement;
mod event;
mod historical_import;
mod known_stale;
mod observation;
mod pending_reconcile;
mod poll_cursor;
mod pool;
mod relationship_queue;

pub use bitcoin_core_header::{
    BitcoinCoreHeader, BitcoinCoreHeaderCacheIntegrityError, BitcoinCoreHeaderCacheUpdate,
    RecheckPass, RecheckScope, ScheduledRecheck, acknowledge_recheck_pass, bind_recheck_pass,
    bitcoin_core_header_cache_integrity_error, cached_bitcoin_core_header_height,
    finish_bitcoin_core_header_cache_operation, finish_bitcoin_core_header_cache_shared_operation,
    highest_final_bitcoin_core_epoch, is_bitcoin_core_header_cache_integrity_error,
    load_bitcoin_core_header_cache_horizon, load_bitcoin_core_nbits_table,
    load_bitcoin_core_nbits_table_if_present, load_core_cache_generation,
    load_core_cache_publication_revision, load_scheduled_recheck, lock_bitcoin_core_header_cache,
    lock_bitcoin_core_header_cache_shared, lock_bitcoin_core_header_cache_shared_in_transaction,
    persist_recheck_cursor, record_bitcoin_core_header, replace_bitcoin_core_header_cache,
    schedule_core_recheck,
};
pub use capture_error::{
    CAPTURE_ERROR_HEIGHT_CAPTURE_FAILED, CAPTURE_ERROR_MALFORMED_AUXPOW_PROOF, clear_capture_error,
    has_capture_error, record_capture_error,
};
pub use capture_write::{CaptureWrite, write_capture};
pub use chains::elastos::{ElastosIdentityReresolveRow, load_elastos_identity_reresolve_batch};
pub use chains::hathor::{
    hathor_evidence_event_at_height, hathor_sidecar_graph_heads_at_height,
    load_hathor_reward_replay_batch, update_hathor_reward_audit, write_hathor_capture_in_txn,
};
#[cfg(any(test, feature = "db-integration"))]
pub use chains::rsk::write_rsk_capture;
pub use chains::rsk::{
    RskBlockFacts, record_rsk_child_only_block, record_rsk_observation, upsert_rsk_only_pools,
    upsert_rsk_pool_identities, upsert_rsk_pool_identities_with_policy, write_rsk_capture_in_txn,
};
pub use child_displacement::{
    ChildChainHead, ChildChainHeadOutcome, ChildChainHeadRecord, ChildDisplacementOutcome,
    CurrentBlockParent, EvidenceMarker, finish_child_chain_height_operation, load_child_chain_head,
    lock_child_chain_height, lock_child_chain_height_session, prune_child_chain_heads_below,
    record_child_chain_block, record_child_chain_block_in_own_transaction,
    reobserve_child_chain_block_in_own_transaction,
};
// The bare event write, without attributions or a required transaction, is for tests.
#[cfg(any(test, feature = "db-integration"))]
pub use event::upsert_merge_mining_event;
pub use event::{
    EventWriteDisposition, EventWriteOutcome, child_block_is_counted, delete_child_block_events,
    delete_event_pool_attributions_for_source, upsert_event_pool_attributions,
    upsert_event_pool_attributions_without_stale_cleanup,
    upsert_merge_mining_event_with_attributions,
};
pub use historical_import::{
    HistoricalBaseEventRow, HistoricalFinalizationState, HistoricalPublicationStateRow,
    load_historical_finalization_state, stream_authoritative_historical_base_events,
    stream_historical_invalid_observation_state, stream_historical_publication_state,
};
pub use known_stale::{count_known_stale_blocks, is_known_stale_hash, upsert_known_stale_block};
pub use observation::{
    ObservationPoolRow, load_child_identity_pools, load_observation_pool_batch,
    sync_event_observation_attribution, update_observation_pools, upsert_child_only_observation,
};
pub use pending_reconcile::{
    PendingReconcileRow, bump_pending_attempts, delete_pending_reconcile, list_pending_reconcile,
    upsert_pending_reconcile,
};
pub use poll_cursor::{get_source_id, load_poll_cursor, upsert_poll_cursor_with_target};
pub use pool::{
    PoolIdentitySeed, load_pool_identities_by_namespace, reconcile_identity_registry,
    upsert_identity_registry, upsert_pool_snapshot, upsert_registry_only_pools,
};

mod producer_runtime;
pub use producer_runtime::{
    ProducerRun, RuntimeTransition, attempt_producer_run, heartbeat_producer_run,
    start_producer_run, transition_producer_run,
};
