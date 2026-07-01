//! Elastos: the dual-endpoint AuxPoW chain. The endpoint may be untrusted, so
//! capture self-verifies before any write (reconstruct + hash guard, full
//! CAuxPow commitment, BTC parent-target and child-target gates, nBits
//! verdict). Relocated under `chains/`; behavior unchanged.

pub(crate) mod backfill;
pub(crate) mod capture;
pub(crate) mod identity;
pub(crate) mod identity_reresolve;
pub(crate) mod rpc;

#[cfg(any(test, feature = "db-integration"))]
pub use capture::{
    ElastosCaptureContext, ElastosHeightOutcome, process_elastos_height,
    process_elastos_table_horizon_for_test,
};
#[cfg(any(test, feature = "db-integration"))]
pub use identity::{
    ELASTOS_MINERINFO_NAMESPACE, ELASTOS_REWARD_ADDRESS_NAMESPACE, ELASTOS_RPC_MINERINFO_SOURCE,
    ELASTOS_RPC_REWARD_ADDRESS_SOURCE,
};
#[cfg(any(test, feature = "db-integration"))]
pub use rpc::{ElastosBlock, ElastosRpc};
