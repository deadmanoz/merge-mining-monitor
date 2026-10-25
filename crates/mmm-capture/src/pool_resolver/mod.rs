//! Offline mining-pool resolution from the embedded BTC snapshot and RSK miner registry.

mod btc;
mod error;
mod identity;

pub use btc::{
    DEFAULT_POOL_SNAPSHOT_JSON, MatchKind, PoolMatch, PoolRecord, PoolResolver, PoolSnapshot,
    PoolSnapshotSource,
};
pub use error::PoolResolverError;
pub use identity::{
    DEFAULT_RSK_MINER_REGISTRY_JSON, ELASTOS_MINERINFO_NAMESPACE, PoolIdentityMatch,
    PoolIdentityRegistry, RSK_MINER_ADDRESS_NAMESPACE, RskMinerEntry, RskMinerRegistry,
};

pub use identity::normalize_rsk_address;
