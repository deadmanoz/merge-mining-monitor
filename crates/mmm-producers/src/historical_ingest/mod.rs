//! Historical AuxPoW CSV importer for recovered dead-chain evidence.
//!
//! This module is producer-side orchestration only: it parses local CSV input,
//! builds standard `MergeMiningEventPayload`s, then writes through
//! `mmm_store` and the `mmm_read_model` mutation facades so the derived tables follow the
//! same path as live producers.

mod config;
mod csv_source;
mod runner;

pub use config::HistoricalImportConfig;
pub use runner::{HistoricalImportSummary, run_historical_import};
