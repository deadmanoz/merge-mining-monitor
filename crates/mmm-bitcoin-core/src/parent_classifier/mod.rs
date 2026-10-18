//! On-demand Bitcoin parent-header classification.
//!
//! This is a narrow, on-demand classification boundary, not the future
//! live-chaintip producer. Production monitor commands require the Core-backed
//! classifier; `Disabled` remains for local library use and tests.

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
pub use mmm_capture::error_blocks::TIME_BELOW_MTP;

/// Number of predecessor headers considered by Bitcoin's median-time-past
/// rule.
pub const MTP_WINDOW: usize = 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightSource {
    BitcoinCore,
    PrevCanonical,
    PrevStale,
    ErrorBlockCatalog,
}

impl HeightSource {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::BitcoinCore => "bitcoin-core",
            Self::PrevCanonical => "prev-canonical",
            Self::PrevStale => "prev-stale",
            Self::ErrorBlockCatalog => "error-block-catalog",
        }
    }

    pub fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "bitcoin-core" => Ok(Self::BitcoinCore),
            "prev-canonical" => Ok(Self::PrevCanonical),
            "prev-stale" => Ok(Self::PrevStale),
            "error-block-catalog" => Ok(Self::ErrorBlockCatalog),
            other => bail!("unknown btc_height_source {other:?}"),
        }
    }
}

/// Persisted state on `block.kind` (BTC-PoW-valid headers only; excludes `ParentKind::Near`,
/// which `mmm_read_model::classify` folds into `BlockKind::Unknown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Canonical,
    Stale,
    ErrorBlock,
    Unknown,
}

impl BlockKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Stale => "stale",
            Self::ErrorBlock => "error_block",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_db_str(value: &str) -> Result<Self> {
        match value {
            "canonical" => Ok(Self::Canonical),
            "stale" => Ok(Self::Stale),
            "error_block" => Ok(Self::ErrorBlock),
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
    /// Transient coinbase fetch failure; marks dependent verdicts provisional for retry.
    pub coinbase_unavailable: bool,
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
    pub rejection_reason: Option<String>,
    pub live_observed: bool,
    pub core_attested: bool,
    /// Candidate was provably absent from Core and all consensus checks completed;
    /// gates strict/weak orphan derivation and marks the proof provisional.
    pub core_absence_attested: bool,
    /// A tolerated RPC lookup failed mid-classification; marks the proof provisional for retry.
    pub incomplete: bool,
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
            rejection_reason: None,
            live_observed: false,
            core_attested: false,
            core_absence_attested: false,
            incomplete: false,
        }
    }

    pub fn incomplete_unknown(header: &Header) -> Self {
        Self {
            incomplete: true,
            ..Self::unknown(header)
        }
    }

    pub fn error_block(
        header: &Header,
        height: i32,
        height_source: HeightSource,
        difficulty_epoch_ok: Option<bool>,
        rejection_reason: impl Into<String>,
    ) -> Self {
        Self {
            kind: ParentKind::ErrorBlock,
            height: Some(height),
            height_source: Some(height_source),
            prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
            canonical_predecessor_header: None,
            canonical_competitor_header: None,
            canonical_competitor_hash: None,
            coinbase: None,
            difficulty_epoch_ok,
            rejection_reason: Some(rejection_reason.into()),
            live_observed: false,
            core_attested: false,
            core_absence_attested: false,
            incomplete: false,
        }
    }

    pub fn to_proof(&self) -> ClassificationProof {
        ClassificationProof {
            parent_kind: Some(self.kind),
            parent_height: self.height,
            difficulty_epoch_ok: self.difficulty_epoch_ok,
            provisional: self.incomplete || self.core_absence_attested,
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

    pub fn from_env_required() -> Result<Self> {
        match Self::from_env()? {
            Self::Disabled => {
                bail!("BITCOIN_RPC_URL is required for monitor capture and classification")
            }
            classifier => Ok(classifier),
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
        self.classify_parent_deferred(header, std::future::ready(Ok(preflight)))
            .await
    }

    /// Strict variant that fails on classification RPC errors (optional coinbase remains best-effort).
    pub async fn classify_parent_strict(
        &self,
        header: &Header,
        preflight: ParentPreflight,
    ) -> Result<ParentClassification> {
        match self {
            Self::Disabled => Ok(ParentClassification::unknown(header)),
            Self::BitcoinCore(classifier) => {
                classifier.classify_parent_strict(header, preflight).await
            }
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.classify_parent(header, preflight).await,
        }
    }

    /// Defer `preflight` lookup until Core proves the candidate header is absent.
    pub async fn classify_parent_deferred<F>(
        &self,
        header: &Header,
        preflight: F,
    ) -> Result<ParentClassification>
    where
        F: Future<Output = Result<ParentPreflight>>,
    {
        match self {
            Self::Disabled => Ok(ParentClassification::unknown(header)),
            Self::BitcoinCore(classifier) => {
                classifier.classify_parent_deferred(header, preflight).await
            }
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.classify_parent(header, preflight.await?).await,
        }
    }

    pub async fn classify_parent_deferred_strict<F>(
        &self,
        header: &Header,
        preflight: F,
    ) -> Result<ParentClassification>
    where
        F: Future<Output = Result<ParentPreflight>>,
    {
        match self {
            Self::Disabled => Ok(ParentClassification::unknown(header)),
            Self::BitcoinCore(classifier) => {
                classifier
                    .classify_parent_deferred_strict(header, preflight)
                    .await
            }
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.classify_parent(header, preflight.await?).await,
        }
    }

    pub fn max_concurrency(&self) -> usize {
        match self {
            Self::Disabled => 1,
            Self::BitcoinCore(classifier) => classifier.max_concurrency(),
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.max_concurrency(),
        }
    }

    pub async fn synced_tip(&self) -> Result<Option<SyncedTip>> {
        match self {
            Self::Disabled => Ok(None),
            Self::BitcoinCore(classifier) => classifier.synced_tip().await,
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.synced_tip().await,
        }
    }

    pub fn metrics(&self) -> Option<mmm_rpc::RpcMetrics> {
        match self {
            Self::Disabled => None,
            Self::BitcoinCore(classifier) => Some(classifier.metrics()),
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(_) => None,
        }
    }

    pub async fn canonical_header(&self, height: i32) -> Result<CoreHeader> {
        match self {
            Self::Disabled => {
                bail!("Bitcoin Core classifier disabled; cannot resolve canonical header")
            }
            Self::BitcoinCore(classifier) => classifier.canonical_header(height).await,
            #[cfg(any(test, feature = "db-integration"))]
            Self::Fake(classifier) => classifier.canonical_header(height).await,
        }
    }
}

mod core;
#[cfg(test)]
pub(crate) mod core_fixture;
#[cfg(test)]
mod core_fixture_tests;
#[cfg(any(test, feature = "db-integration"))]
mod fake;
#[cfg(test)]
mod tests;

pub use core::{BitcoinCoreParentClassifier, CoreHeader, SyncedTip};
#[cfg(any(test, feature = "db-integration"))]
pub use fake::{FakeParentClassifier, FakeParentClassifierGate};
