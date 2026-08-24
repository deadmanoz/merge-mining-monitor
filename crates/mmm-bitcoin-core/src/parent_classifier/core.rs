//! The Bitcoin-Core-backed classifier: header lookups over the RPC source
//! and the canonical/stale/inferred-stale/unknown verdict constructors.

use super::*;

/// Canonical header data fetched from Bitcoin Core by height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoreHeader {
    pub height: i32,
    pub hash: BlockHash,
    pub nbits: u32,
    pub header_time: i64,
}

/// A Bitcoin Core tip that is synced, with whether it is also FRESH. `fresh` is
/// false when the tip's median time is older than [`MAX_TIP_AGE_SECS`]: a stalled
/// or isolated node can report `blocks == headers && !IBD` while sitting far behind
/// the real network tip, and trusting its lagging tip would wrongly classify a
/// valid beyond-horizon parent as a fabricated far-future height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncedTip {
    pub height: i32,
    pub fresh: bool,
}

/// Maximum age (seconds) of the chain tip's median time before a synced Core is
/// treated as stale for the far-future decision. ~24h, aligned with the 144-block
/// (~1 day) far-future tolerance: only once Core lags the real tip by more than the
/// tolerance can a genuine parent exceed `tip + tolerance`, and a tip that far
/// behind has a median time at least this old.
const MAX_TIP_AGE_SECS: i64 = 86_400;

/// Whether a tip whose median time is `median_time` is fresh as of `now_secs`.
/// Pure, so the freshness policy is unit-tested without a clock.
pub(crate) fn tip_is_fresh(median_time: i64, now_secs: i64) -> bool {
    now_secs.saturating_sub(median_time) <= MAX_TIP_AGE_SECS
}

fn now_unix_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

#[derive(Clone)]
pub struct BitcoinCoreParentClassifier {
    source: Arc<dyn CoreHeaderSource>,
    max_concurrency: usize,
}

pub(crate) struct CoreRpcHeaderSource {
    client: BitcoinCoreRpcClient,
}

pub(crate) type CoreRpcFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

pub(crate) trait CoreHeaderSource: Send + Sync {
    fn get_chain_status(&self) -> CoreRpcFuture<'_, BitcoinCoreChainStatus>;
    fn get_block_hash(&self, height: u64) -> CoreRpcFuture<'_, BlockHash>;
    fn get_header(&self, hash: BlockHash, height: i32) -> CoreRpcFuture<'_, ClassifiedHeader>;
    /// Header-only fetch (no coinbase / full block), used by the epoch nBits path.
    fn get_block_header(&self, hash: BlockHash) -> CoreRpcFuture<'_, Header>;
    fn get_header_verbose(&self, hash: BlockHash) -> CoreRpcFuture<'_, CoreHeaderStatus>;
    fn get_block_coinbase(&self, hash: BlockHash) -> CoreRpcFuture<'_, BitcoinCoreBlockCoinbase>;
}

impl BitcoinCoreParentClassifier {
    pub fn from_env_url(url: &str) -> Result<Self> {
        let client = BitcoinCoreRpcClient::from_env_url(url)?;
        let max_concurrency = client.max_concurrency();
        Ok(Self {
            source: Arc::new(CoreRpcHeaderSource { client }),
            max_concurrency,
        })
    }

    #[cfg(test)]
    pub(crate) fn from_source(source: Arc<dyn CoreHeaderSource>) -> Self {
        Self {
            source,
            max_concurrency: 1,
        }
    }

    pub async fn classify_parent(
        &self,
        header: &Header,
        preflight: ParentPreflight,
    ) -> Result<ParentClassification> {
        self.classify_parent_deferred_with_policy(header, std::future::ready(Ok(preflight)), false)
            .await
    }

    pub async fn classify_parent_strict(
        &self,
        header: &Header,
        preflight: ParentPreflight,
    ) -> Result<ParentClassification> {
        self.classify_parent_deferred_with_policy(header, std::future::ready(Ok(preflight)), true)
            .await
    }

    /// Classify a parent while deferring the read-model lookup until Core proves
    /// the candidate header is absent. Canonical and Core-indexed stale parents
    /// never consume the supplied future.
    pub async fn classify_parent_deferred<F>(
        &self,
        header: &Header,
        preflight: F,
    ) -> Result<ParentClassification>
    where
        F: Future<Output = Result<ParentPreflight>>,
    {
        self.classify_parent_deferred_with_policy(header, preflight, false)
            .await
    }

    pub async fn classify_parent_deferred_strict<F>(
        &self,
        header: &Header,
        preflight: F,
    ) -> Result<ParentClassification>
    where
        F: Future<Output = Result<ParentPreflight>>,
    {
        self.classify_parent_deferred_with_policy(header, preflight, true)
            .await
    }

    async fn classify_parent_deferred_with_policy<F>(
        &self,
        header: &Header,
        preflight: F,
        fail_on_rpc_error: bool,
    ) -> Result<ParentClassification>
    where
        F: Future<Output = Result<ParentPreflight>>,
    {
        let candidate_hash = header.block_hash();
        let verbose = match self.source.get_header_verbose(candidate_hash).await {
            Ok(v) => v,
            Err(err) if bitcoin_rpc::is_not_found(&err) => {
                // The candidate header is provably absent from Core's main chain
                // and stale set. Any `unknown` returned from this path (whatever
                // internal exit produced it: predecessor not-found/transient,
                // missing competitor, bits-mismatch, height overflow) is a
                // genuine BTC-orphan candidate, so stamp `core_absence_attested`.
                // A `stale` result from the inferred-stale path is left untouched.
                let preflight = preflight.await?;
                let mut classification = self
                    .classify_core_unknown(header, preflight, fail_on_rpc_error)
                    .await?;
                if classification.kind == ParentKind::Unknown {
                    classification.core_absence_attested = true;
                }
                return Ok(classification);
            }
            Err(err) => {
                if fail_on_rpc_error {
                    return Err(err).with_context(|| {
                        format!("Bitcoin Core parent-header lookup failed for {candidate_hash}")
                    });
                }
                warn!(error = %err, hash = %candidate_hash, "Bitcoin Core parent-header lookup failed");
                return Ok(ParentClassification::unknown(header));
            }
        };

        let height: i32 = match core_height_to_i32(verbose.height) {
            Ok(height) => height,
            Err(_) => {
                warn!(
                    height = verbose.height,
                    "Bitcoin Core header height overflows i32"
                );
                return Ok(ParentClassification::unknown(header));
            }
        };

        let coinbase = self.fetch_coinbase(candidate_hash).await;

        if verbose.confirmations >= 0 {
            return Ok(classify_core_canonical_header(header, height, coinbase));
        }

        Ok(classify_core_stale_header(
            header,
            height,
            self.fetch_competitor(height, fail_on_rpc_error).await?,
            coinbase,
        ))
    }

    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    pub async fn synced_tip_height(&self) -> Result<Option<i32>> {
        let status = self.source.get_chain_status().await?;
        Ok(status.is_synced_tip().then_some(status.blocks))
    }

    /// The synced Core tip with its freshness, or `None` when Core is not at a
    /// synced tip (IBD or `blocks != headers`). `fresh` separates a tip Core has
    /// actually advanced to recently from a stalled node's lagging tip, so the
    /// far-future decision never revokes a valid parent against a stale tip.
    pub async fn synced_tip(&self) -> Result<Option<SyncedTip>> {
        let status = self.source.get_chain_status().await?;
        if !status.is_synced_tip() {
            return Ok(None);
        }
        Ok(Some(SyncedTip {
            height: status.blocks,
            fresh: tip_is_fresh(status.median_time, now_unix_secs()),
        }))
    }

    /// Resolve a canonical header by height without fetching its block body.
    /// Callers decide when the result is sufficiently confirmed to persist.
    pub async fn canonical_header(&self, height: i32) -> Result<CoreHeader> {
        let height_u64 = u64::try_from(height)
            .with_context(|| format!("Bitcoin Core header height {height} is negative"))?;
        let hash = self.source.get_block_hash(height_u64).await?;
        let header = self.source.get_block_header(hash).await?;
        Ok(CoreHeader {
            height,
            hash,
            nbits: header.bits.to_consensus(),
            header_time: i64::from(header.time),
        })
    }

    async fn classify_core_unknown(
        &self,
        header: &Header,
        preflight: ParentPreflight,
        fail_on_rpc_error: bool,
    ) -> Result<ParentClassification> {
        if let Some(known_prev) = preflight.known_prev
            && matches!(known_prev.kind, BlockKind::Canonical | BlockKind::Stale)
            && let Some(prev_height) = known_prev.btc_height
        {
            return self
                .classify_inferred_stale(
                    header,
                    prev_height,
                    None,
                    known_prev.kind,
                    fail_on_rpc_error,
                )
                .await;
        }

        let prev_verbose = match self.source.get_header_verbose(header.prev_blockhash).await {
            Ok(v) if v.confirmations >= 0 => v,
            Ok(_) => return Ok(ParentClassification::unknown(header)),
            Err(err) if bitcoin_rpc::is_not_found(&err) => {
                return Ok(ParentClassification::unknown(header));
            }
            Err(err) => {
                if fail_on_rpc_error {
                    return Err(err).with_context(|| {
                        format!(
                            "Bitcoin Core predecessor lookup failed for {}",
                            header.prev_blockhash
                        )
                    });
                }
                warn!(error = %err, prev_hash = %header.prev_blockhash, "Bitcoin Core predecessor lookup failed");
                return Ok(ParentClassification::unknown(header));
            }
        };
        let prev_height: i32 = match core_height_to_i32(prev_verbose.height) {
            Ok(height) => height,
            Err(_) => return Ok(ParentClassification::unknown(header)),
        };
        let predecessor = match self
            .source
            .get_header(header.prev_blockhash, prev_height)
            .await
        {
            Ok(header) => Some(header),
            Err(err) => {
                if fail_on_rpc_error {
                    return Err(err).with_context(|| {
                        format!(
                            "Bitcoin Core predecessor header fetch failed for {}",
                            header.prev_blockhash
                        )
                    });
                }
                warn!(error = %err, prev_hash = %header.prev_blockhash, "Bitcoin Core predecessor header fetch failed");
                return Ok(ParentClassification::unknown(header));
            }
        };
        self.classify_inferred_stale(
            header,
            prev_height,
            predecessor,
            BlockKind::Canonical,
            fail_on_rpc_error,
        )
        .await
    }

    async fn classify_inferred_stale(
        &self,
        header: &Header,
        prev_height: i32,
        predecessor: Option<ClassifiedHeader>,
        prev_kind: BlockKind,
        fail_on_rpc_error: bool,
    ) -> Result<ParentClassification> {
        let height = match prev_height.checked_add(1) {
            Some(height) => height,
            None => return Ok(ParentClassification::unknown(header)),
        };
        Ok(classify_inferred_stale_with_competitor(
            header,
            height,
            predecessor,
            prev_kind,
            self.fetch_competitor(height, fail_on_rpc_error).await?,
        ))
    }

    async fn fetch_competitor(
        &self,
        height: i32,
        fail_on_rpc_error: bool,
    ) -> Result<Option<ClassifiedHeader>> {
        let height_u64 = match height.try_into() {
            Ok(height) => height,
            Err(_) => return Ok(None),
        };
        let hash = match self.source.get_block_hash(height_u64).await {
            Ok(hash) => hash,
            Err(err)
                if bitcoin_rpc::is_block_height_out_of_range(&err)
                    || bitcoin_rpc::is_not_found(&err) =>
            {
                return Ok(None);
            }
            Err(err) => {
                if fail_on_rpc_error {
                    return Err(err).with_context(|| {
                        format!("Bitcoin Core competitor hash fetch failed at {height}")
                    });
                }
                warn!(height, error = %err, "Bitcoin Core same-height competitor hash fetch failed");
                return Ok(None);
            }
        };
        match self.source.get_header(hash, height).await {
            Ok(header) => Ok(Some(header)),
            Err(err) if bitcoin_rpc::is_not_found(&err) => Ok(None),
            Err(err) => {
                if fail_on_rpc_error {
                    return Err(err).with_context(|| {
                        format!("Bitcoin Core competitor header fetch failed for {hash}")
                    });
                }
                warn!(height, hash = %hash, error = %err, "Bitcoin Core same-height competitor header fetch failed");
                Ok(None)
            }
        }
    }

    async fn fetch_coinbase(&self, hash: BlockHash) -> Option<BitcoinCoreBlockCoinbase> {
        match self.source.get_block_coinbase(hash).await {
            Ok(coinbase) => Some(coinbase),
            Err(err) => {
                warn!(hash = %hash, error = %err, "Bitcoin Core coinbase fetch failed");
                None
            }
        }
    }
}

impl CoreRpcHeaderSource {
    async fn get_chain_status_impl(&self) -> Result<BitcoinCoreChainStatus> {
        self.client.get_chain_status().await
    }

    async fn get_block_hash_impl(&self, height: u64) -> Result<BlockHash> {
        self.client.get_block_hash(height).await
    }

    async fn get_header_impl(&self, hash: BlockHash, height: i32) -> Result<ClassifiedHeader> {
        let header = self.client.get_block_header(hash).await?;
        let coinbase = match self.client.get_block_coinbase(hash).await {
            Ok(coinbase) => Some(coinbase),
            Err(err) => {
                warn!(hash = %hash, error = %err, "Bitcoin Core coinbase fetch failed");
                None
            }
        };
        Ok(ClassifiedHeader {
            hash: hash.to_byte_array().to_vec(),
            prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
            header,
            height,
            coinbase,
        })
    }

    async fn get_block_header_impl(&self, hash: BlockHash) -> Result<Header> {
        self.client.get_block_header(hash).await
    }

    async fn get_header_verbose_impl(&self, hash: BlockHash) -> Result<CoreHeaderStatus> {
        self.client.get_block_header_verbose(hash).await
    }

    async fn get_block_coinbase_impl(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        self.client.get_block_coinbase(hash).await
    }
}

impl CoreHeaderSource for CoreRpcHeaderSource {
    fn get_chain_status(&self) -> CoreRpcFuture<'_, BitcoinCoreChainStatus> {
        Box::pin(self.get_chain_status_impl())
    }

    fn get_block_hash(&self, height: u64) -> CoreRpcFuture<'_, BlockHash> {
        Box::pin(self.get_block_hash_impl(height))
    }

    fn get_header(&self, hash: BlockHash, height: i32) -> CoreRpcFuture<'_, ClassifiedHeader> {
        Box::pin(self.get_header_impl(hash, height))
    }

    fn get_block_header(&self, hash: BlockHash) -> CoreRpcFuture<'_, Header> {
        Box::pin(self.get_block_header_impl(hash))
    }

    fn get_header_verbose(&self, hash: BlockHash) -> CoreRpcFuture<'_, CoreHeaderStatus> {
        Box::pin(self.get_header_verbose_impl(hash))
    }

    fn get_block_coinbase(&self, hash: BlockHash) -> CoreRpcFuture<'_, BitcoinCoreBlockCoinbase> {
        Box::pin(self.get_block_coinbase_impl(hash))
    }
}

pub(crate) fn core_height_to_i32(
    height: i64,
) -> std::result::Result<i32, std::num::TryFromIntError> {
    height.try_into()
}

pub(crate) fn classify_core_canonical_header(
    header: &Header,
    height: i32,
    coinbase: Option<BitcoinCoreBlockCoinbase>,
) -> ParentClassification {
    ParentClassification {
        kind: ParentKind::Canonical,
        height: Some(height),
        height_source: Some(HeightSource::BitcoinCore),
        prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
        canonical_predecessor_header: None,
        canonical_competitor_header: None,
        canonical_competitor_hash: None,
        coinbase,
        difficulty_epoch_ok: Some(true),
        live_observed: true,
        core_attested: true,
        core_absence_attested: false,
    }
}

pub(crate) fn classify_core_stale_header(
    header: &Header,
    height: i32,
    competitor: Option<ClassifiedHeader>,
    coinbase: Option<BitcoinCoreBlockCoinbase>,
) -> ParentClassification {
    let Some(competitor) = competitor else {
        return ParentClassification::unknown(header);
    };
    let candidate_hash = header.block_hash();
    if competitor.hash == candidate_hash.to_byte_array() {
        warn!(hash = %candidate_hash, height, "Bitcoin Core returned candidate as its own stale competitor");
        return ParentClassification::unknown(header);
    }

    ParentClassification {
        kind: ParentKind::Stale,
        height: Some(height),
        height_source: Some(HeightSource::BitcoinCore),
        prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
        canonical_predecessor_header: None,
        canonical_competitor_hash: Some(competitor.hash.clone()),
        canonical_competitor_header: Some(competitor),
        coinbase,
        difficulty_epoch_ok: Some(true),
        live_observed: true,
        core_attested: true,
        core_absence_attested: false,
    }
}

pub(crate) fn classify_inferred_stale_with_competitor(
    header: &Header,
    height: i32,
    predecessor: Option<ClassifiedHeader>,
    prev_kind: BlockKind,
    competitor: Option<ClassifiedHeader>,
) -> ParentClassification {
    let Some(competitor) = competitor else {
        return ParentClassification::unknown(header);
    };
    if !bits_match_expected(header, competitor.header.bits) {
        return ParentClassification {
            difficulty_epoch_ok: Some(false),
            ..ParentClassification::unknown(header)
        };
    }

    ParentClassification {
        kind: ParentKind::Stale,
        height: Some(height),
        height_source: Some(match prev_kind {
            BlockKind::Canonical => HeightSource::PrevCanonical,
            BlockKind::Stale => HeightSource::PrevStale,
            BlockKind::ErrorBlock => {
                unreachable!("error-block predecessor is not eligible for stale inference")
            }
            BlockKind::Unknown => unreachable!("unknown predecessor kind is not classified"),
        }),
        prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
        canonical_predecessor_header: predecessor,
        canonical_competitor_hash: Some(competitor.hash.clone()),
        canonical_competitor_header: Some(competitor),
        coinbase: None,
        difficulty_epoch_ok: Some(true),
        live_observed: false,
        core_attested: false,
        core_absence_attested: false,
    }
}

fn bits_match_expected(header: &Header, expected: CompactTarget) -> bool {
    header.bits == expected
}
