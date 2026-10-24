//! Pure offline evidence parsing, normalization, and classification (`cargo tree -e normal`
//! has no database, HTTP, or Core RPC dependencies).

pub mod attribution_policy;
pub mod auxpow;
pub mod btc_orphan;
pub mod capture;
pub mod child_payout;
pub mod core_coinbase;
#[cfg(any(test, feature = "artifact-generation"))]
pub mod findings_registry;
pub mod identity_registry;
pub mod invalid_blocks;
pub mod lineage;
pub mod nbits_table;
pub mod pool_resolver;
#[cfg(any(test, feature = "artifact-generation"))]
pub mod pool_snapshot_gen;
pub mod progress;
pub mod protocol_commitment;
pub mod source_registry;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
