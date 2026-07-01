//! Mining-pool resolution from the embedded registries.
//!
//! The resolver is pure and offline: callers supply coinbase bytes or
//! chain-native identity strings and get pool verdicts back, with no database
//! or network I/O.
//!
//! The module is split by concern:
//!
//! - `btc` handles BTC-coinbase pool resolution from the embedded
//!   `data/pools/current.json` snapshot.
//! - `identity` handles RSK miner-address attribution from the embedded
//!   `data/pools/child-identities/rsk_miner_registry.json` registry.
//! - `error` holds the shared [`PoolResolverError`] enum.
//!
//! The submodules are private; the public API is the set of items
//! re-exported below, which matches the pre-split file's API exactly.

mod btc;
mod error;
mod identity;

pub use btc::{
    DEFAULT_POOL_SNAPSHOT_JSON, MatchKind, PoolMatch, PoolRecord, PoolResolver, PoolSnapshot,
    PoolSnapshotSource,
};
pub use error::PoolResolverError;
pub use identity::{
    DEFAULT_RSK_MINER_REGISTRY_JSON, PoolIdentityMatch, PoolIdentityRegistry,
    RSK_MINER_ADDRESS_NAMESPACE, RskMinerEntry, RskMinerRegistry,
};

pub use identity::normalize_rsk_address;
