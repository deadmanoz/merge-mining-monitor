//! Every ingest/maintenance engine a CLI subcommand dispatches: the chain
//! registry and its producers, the live loop, the Bitcoin Core backbone,
//! and the repair/reattribution commands. The only crate that combines chain
//! RPC with database writes - and it owns NO base-table or derived-table SQL
//! of its own (store and read-model helpers are called with data).

mod bitcoin_core_backbone;
#[cfg(any(test, feature = "db-integration"))]
pub mod chains;
#[cfg(not(any(test, feature = "db-integration")))]
mod chains;
mod historical_ingest;
mod live_loop;
mod poller;
mod producer_runtime;
mod reclassify_pools;

#[cfg(any(test, feature = "db-integration"))]
pub use bitcoin_core_backbone::{
    BitcoinCoreBackboneSource, BitcoinCoreBackboneTip, initialize_follow_state,
};
pub use bitcoin_core_backbone::{
    BitcoinCoreSyncConfig, BitcoinCoreSyncStats, run_sync_bitcoin_core,
    run_sync_bitcoin_core_follow,
};
pub use chains::{
    is_producer_command, no_command_help, run_hathor_cache_command, run_producer_command,
    unknown_command_message,
};
pub use historical_ingest::{
    HistoricalImportConfig, HistoricalImportSummary, run_historical_import,
};
#[cfg(any(test, feature = "db-integration"))]
pub use poller::{ChainPoller, ChainPollerState, HeightProgress, Poller, PollerConfig};
pub use producer_runtime::connect_from_env;
pub use reclassify_pools::{ReclassifyPoolsConfig, ReclassifyPoolsStats, run_reclassify_pools};
