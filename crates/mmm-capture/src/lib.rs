//! Pure offline evidence: parse, normalize, classify.
//!
//! Compiles with no tokio-postgres, no reqwest, and no corepc in its normal
//! dependency graph (mechanically checked with `cargo tree -e normal`).
//! Everything that touches a database, a node, or the network lives in the
//! crates built on top of this one.

pub mod attribution_policy;
pub mod auxpow;
pub mod btc_orphan;
pub mod capture;
pub mod child_payout;
pub mod core_coinbase;
pub mod identity_registry;
pub mod nbits_table;
pub mod pool_resolver;
#[cfg(any(test, feature = "artifact-generation"))]
pub mod pool_snapshot_gen;
pub mod source_registry;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
