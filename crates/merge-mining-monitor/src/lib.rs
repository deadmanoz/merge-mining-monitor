//! Wiring-only CLI package. The single library symbol is the package
//! version, recorded into the pool-snapshot manifest and logged at startup;
//! every domain module lives in the mmm-* workspace crates.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
