//! Historical AuxPoW CSV importer for recovered dead-chain evidence.
//!
//! This module is producer-side orchestration only: it parses local CSV input,
//! builds standard `MergeMiningEventPayload`s, then writes through
//! `mmm_store` and the `mmm_read_model` mutation facades so the derived tables follow the
//! same path as live producers.

mod config;
mod csv_source;
mod publication;
mod rsk_sidecar;
mod runner;

pub use config::{HistoricalImportAllConfig, HistoricalImportConfig};
pub use runner::{
    HistoricalImportAllSummary, HistoricalImportSummary, run_historical_import,
    run_historical_import_all,
};
#[cfg(feature = "db-integration")]
pub use runner::{
    enqueue_published_stale_branches_for_test, run_historical_import_configs_for_test,
    run_manifest_historical_import_for_test,
};
