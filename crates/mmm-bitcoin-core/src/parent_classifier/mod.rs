//! On-demand Bitcoin parent-header classification.
//!
//! This is a narrow, on-demand classification boundary, not the future
//! live-chaintip producer. The classifier is optional: with `BITCOIN_RPC_URL`
//! unset it returns `unknown` for BTC-PoW-valid parents and capture remains
//! fully local to the child-chain RPCs.

#[cfg(any(test, feature = "db-integration"))]
use std::collections::VecDeque;
use std::env;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(any(test, feature = "db-integration"))]
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail};
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget};
#[cfg(any(test, feature = "db-integration"))]
use tokio::sync::Notify;
use tracing::warn;

use crate::bitcoin_rpc::{
    self, BitcoinCoreBlockCoinbase, BitcoinCoreChainStatus,
    BitcoinCoreHeaderStatus as CoreHeaderStatus, BitcoinCoreRpcClient,
};
use mmm_capture::capture::{ClassificationProof, ParentKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightSource {
    BitcoinCore,
    PrevCanonical,
    PrevStale,
}

impl HeightSource {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::BitcoinCore => "bitcoin-core",
            Self::PrevCanonical => "prev-canonical",
            Self::PrevStale => "prev-stale",
        }
    }

    pub fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "bitcoin-core" => Ok(Self::BitcoinCore),
            "prev-canonical" => Ok(Self::PrevCanonical),
            "prev-stale" => Ok(Self::PrevStale),
            other => bail!("unknown btc_height_source {other:?}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Canonical,
    Stale,
    Unknown,
}

impl BlockKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "canonical" => Ok(Self::Canonical),
            "stale" => Ok(Self::Stale),
            "unknown" => Ok(Self::Unknown),
            other => bail!("unknown block.kind {other:?}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownBlockContext {
    pub kind: BlockKind,
    pub btc_height: Option<i32>,
    pub btc_height_source: Option<HeightSource>,
    pub canonical_competitor_hash: Option<Vec<u8>>,
    pub core_attested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentPreflight {
    pub known_prev: Option<KnownBlockContext>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifiedHeader {
    pub hash: Vec<u8>,
    pub prev_hash: Vec<u8>,
    pub header: Header,
    pub height: i32,
    pub coinbase: Option<BitcoinCoreBlockCoinbase>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParentClassification {
    pub kind: ParentKind,
    pub height: Option<i32>,
    pub height_source: Option<HeightSource>,
    pub prev_hash: Vec<u8>,
    pub canonical_predecessor_header: Option<ClassifiedHeader>,
    pub canonical_competitor_header: Option<ClassifiedHeader>,
    pub canonical_competitor_hash: Option<Vec<u8>>,
    pub coinbase: Option<BitcoinCoreBlockCoinbase>,
    pub difficulty_epoch_ok: Option<bool>,
    pub live_observed: bool,
    pub core_attested: bool,
    /// True only when Bitcoin Core was consulted and returned the candidate
    /// header as not-found (provably absent from Core's main chain and stale
    /// set), so the resulting `unknown` is a genuine BTC-orphan candidate rather
    /// than a never-checked (Disabled) or transient-RPC-error unknown. Set by
    /// the candidate-not-found post-process in [`BitcoinCoreParentClassifier::classify_parent`];
    /// the read-model reconciler gates strict/weak orphan classification on it.
    pub core_absence_attested: bool,
}

impl ParentClassification {
    pub fn unknown(header: &Header) -> Self {
        Self {
            kind: ParentKind::Unknown,
            height: None,
            height_source: None,
            prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
            canonical_predecessor_header: None,
            canonical_competitor_header: None,
            canonical_competitor_hash: None,
            coinbase: None,
            difficulty_epoch_ok: None,
            live_observed: false,
            core_attested: false,
            core_absence_attested: false,
        }
    }

    pub fn to_proof(&self) -> ClassificationProof {
        ClassificationProof {
            parent_kind: Some(self.kind),
            parent_height: self.height,
            difficulty_epoch_ok: self.difficulty_epoch_ok,
        }
    }
}

#[derive(Clone, Default)]
pub enum ConfiguredParentClassifier {
    #[default]
    Disabled,
    BitcoinCore(BitcoinCoreParentClassifier),
    #[cfg(any(test, feature = "db-integration"))]
    Fake(FakeParentClassifier),
}

impl std::fmt::Debug for ConfiguredParentClassifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => f.write_str("ConfiguredParentClassifier::Disabled"),
            Self::BitcoinCore(_) => f.write_str("ConfiguredParentClassifier::BitcoinCore(..)"),
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(_) => f.write_str("ConfiguredParentClassifier::Fake(..)"),
        }
    }
}

impl ConfiguredParentClassifier {
    pub fn from_env() -> Result<Self> {
        match env::var("BITCOIN_RPC_URL") {
            Ok(url) if !url.trim().is_empty() => Ok(Self::BitcoinCore(
                BitcoinCoreParentClassifier::from_env_url(&url)?,
            )),
            Ok(_) | Err(env::VarError::NotPresent) => Ok(Self::Disabled),
            Err(err) => Err(err).context("read BITCOIN_RPC_URL"),
        }
    }

    pub fn is_enabled(&self) -> bool {
        !matches!(self, Self::Disabled)
    }

    pub async fn classify_parent(
        &self,
        header: &Header,
        preflight: ParentPreflight,
    ) -> Result<ParentClassification> {
        match self {
            Self::Disabled => Ok(ParentClassification::unknown(header)),
            Self::BitcoinCore(classifier) => classifier.classify_parent(header, preflight).await,
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.classify_parent(header, preflight).await,
        }
    }

    pub async fn synced_tip_height(&self) -> Result<Option<i32>> {
        match self {
            Self::Disabled => Ok(None),
            Self::BitcoinCore(classifier) => classifier.synced_tip_height().await,
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.synced_tip_height().await,
        }
    }

    /// The synced Core tip with its freshness (see [`SyncedTip`]). `Disabled`
    /// reports no tip, so the far-future resolver holds rather than revoking.
    pub async fn synced_tip(&self) -> Result<Option<SyncedTip>> {
        match self {
            Self::Disabled => Ok(None),
            Self::BitcoinCore(classifier) => classifier.synced_tip().await,
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.synced_tip().await,
        }
    }

    /// Canonical Bitcoin nBits + header time at a DAA epoch-start height, resolved
    /// from Core (memoized per epoch). `Disabled` errors: the caller gates on
    /// [`Self::synced_tip_height`] first, which `Disabled` answers `None`, so the
    /// resolver holds before ever reaching here.
    pub async fn epoch_nbits(
        &self,
        epoch_start_height: i32,
        synced_tip: i32,
    ) -> Result<EpochNbits> {
        match self {
            Self::Disabled => {
                bail!("Bitcoin Core classifier disabled; cannot resolve epoch nBits")
            }
            Self::BitcoinCore(classifier) => {
                classifier.epoch_nbits(epoch_start_height, synced_tip).await
            }
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.epoch_nbits(epoch_start_height, synced_tip).await,
        }
    }
}

mod core;
#[cfg(any(test, feature = "db-integration"))]
mod fake;
#[cfg(test)]
mod tests;

pub use core::{BitcoinCoreParentClassifier, EpochNbits, SyncedTip};
#[cfg(any(test, feature = "db-integration"))]
pub use fake::{FakeParentClassifier, FakeParentClassifierGate};
