//! The only crate that links corepc: Bitcoin Core node access plus the
//! Core-backed parent classification policy built directly on it.

mod bitcoin_rpc;
mod parent_classifier;

pub use bitcoin_rpc::{BitcoinCoreBlockCoinbase, BitcoinCoreRpcClient};
pub use parent_classifier::{
    BitcoinCoreParentClassifier, BlockKind, ClassifiedHeader, ConfiguredParentClassifier,
    CoreHeader, HeightSource, KnownBlockContext, ParentClassification, ParentPreflight, SyncedTip,
    TIME_BELOW_MTP,
};
#[cfg(any(test, feature = "db-integration"))]
pub use parent_classifier::{FakeParentClassifier, FakeParentClassifierGate};
