//! The only crate that links corepc: Bitcoin Core node access plus the
//! Core-backed parent classification policy built directly on it.

mod bitcoin_rpc;
mod parent_classifier;

pub use bitcoin_rpc::{BitcoinCoreBlockCoinbase, BitcoinCoreRpcClient};
pub use parent_classifier::{
    BitcoinCoreParentClassifier, BlockKind, ClassifiedHeader, ConfiguredParentClassifier,
    EpochNbits, HeightSource, KnownBlockContext, ParentClassification, ParentPreflight, SyncedTip,
};
#[cfg(any(test, feature = "db-integration"))]
pub use parent_classifier::{FakeParentClassifier, FakeParentClassifierGate};
