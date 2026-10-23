//! The only crate that links corepc: Bitcoin Core node access plus the
//! Core-backed parent classification policy built directly on it.

mod availability;
mod bitcoin_rpc;
mod parent_classifier;

pub use availability::{CoreFault, CoreUnavailable};
pub use bitcoin_rpc::{BitcoinCoreBlockCoinbase, BitcoinCoreRpcClient, is_retryable_capture_error};
pub use parent_classifier::{
    BitcoinCoreParentClassifier, BlockKind, ClassifiedHeader, ConfiguredParentClassifier,
    CoreHeader, HeightSource, KnownBlockContext, ParentClassification, ParentPreflight, SyncedTip,
    TIME_BELOW_MTP,
};
#[cfg(any(test, feature = "db-integration"))]
pub use parent_classifier::{FakeParentClassifier, FakeParentClassifierGate};
