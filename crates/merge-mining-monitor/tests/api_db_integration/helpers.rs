use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::sync::Arc;

use anyhow::{Context, Result};
use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_api::ApiError;
use mmm_api::projection::TreePayload;
use mmm_api::projection::{self, ProjectionError};
use mmm_api::query::{self};
use mmm_bitcoin_core::BitcoinCoreBlockCoinbase;
use mmm_producers::{BitcoinCoreBackboneSource, BitcoinCoreBackboneTip};
use tokio_postgres::Client;

use crate::support::seed::{hash_bytes, insert_block};

pub(crate) async fn project_tree(
    client: &Client,
    query_string: Option<&str>,
) -> Result<TreePayload> {
    let query = query::parse_tree_query(query_string).map_err(format_api_error)?;
    projection::tree(client, &query)
        .await
        .map_err(format_projection_error)
}

pub(crate) async fn expect_tree_api_error(client: &Client, query_string: &str) -> Result<ApiError> {
    let query = query::parse_tree_query(Some(query_string)).map_err(format_api_error)?;
    match projection::tree(client, &query).await {
        Err(ProjectionError::Api(api)) => Ok(api),
        Err(ProjectionError::Internal(err)) => {
            anyhow::bail!("expected tree API error, got internal: {err}")
        }
        Ok(_) => anyhow::bail!("expected tree API error"),
    }
}

/// Seed canonical blocks with deterministic hashes from `first_hash_seed` and
/// timestamps from `first_timestamp`.
/// If `omitted_height` is set, the missing block is skipped but the following
/// block still points to the omitted hash, matching a broken canonical span.
pub(crate) async fn seed_canonical_chain(
    client: &Client,
    heights: RangeInclusive<i32>,
    first_hash_seed: u32,
    prev_hash_seed: u32,
    first_timestamp: i64,
    omitted_height: Option<i32>,
) -> Result<BTreeMap<i32, Vec<u8>>> {
    let first_height = *heights.start();
    let mut hashes = BTreeMap::new();
    let mut prev = hash_bytes(prev_hash_seed);
    for height in heights {
        let offset = height
            .checked_sub(first_height)
            .context("canonical test height offset underflow")?;
        let offset_seed = u32::try_from(offset).context("canonical test height offset overflow")?;
        let hash_seed = first_hash_seed
            .checked_add(offset_seed)
            .context("canonical test hash seed overflow")?;
        let hash = hash_bytes(hash_seed);
        if omitted_height != Some(height) {
            insert_block(
                client,
                &hash,
                &prev,
                Some(height),
                "canonical",
                first_timestamp + i64::from(offset),
                None,
            )
            .await?;
        }
        prev = hash.clone();
        hashes.insert(height, hash);
    }
    Ok(hashes)
}

pub(crate) async fn insert_unknown_block(
    client: &Client,
    hash: &[u8],
    prev_hash: &[u8],
    timestamp: i64,
) -> Result<()> {
    insert_block(client, hash, prev_hash, None, "unknown", timestamp, None).await
}

/// Set `block.btc_orphan_class` for one block (test helper; the reconciler does
/// this in production).
pub(crate) async fn set_orphan_class(client: &Client, hash: &[u8], class: &str) -> Result<()> {
    client
        .execute(
            "UPDATE block SET btc_orphan_class = $2 WHERE btc_header_hash = $1",
            &[&hash, &class],
        )
        .await?;
    Ok(())
}

/// Classify every genuine PoW-valid unknown as `strict_btc_orphan` so the default
/// strict+weak navigator/anchor filter returns them. A `pow_validated=false`
/// revocation husk is left pending (and is excluded by the pow_validated guard
/// regardless). Run after any husk `pow_validated` demotion in the same test.
pub(crate) async fn classify_all_unknowns_strict(client: &Client) -> Result<()> {
    client
        .execute(
            "UPDATE block SET btc_orphan_class = 'strict_btc_orphan' \
             WHERE kind = 'unknown' AND pow_validated",
            &[],
        )
        .await?;
    Ok(())
}

#[derive(Clone)]
pub(crate) struct FakeBitcoinCoreBackboneSource {
    pub(crate) tip_height: i32,
    pub(crate) headers: Arc<BTreeMap<i32, Header>>,
    pub(crate) failures: Arc<BTreeSet<Vec<u8>>>,
    // Hashes for which block_coinbase / block_header were actually fetched, so a
    // test can prove the missing_only fast path SKIPPED already-complete rows.
    pub(crate) coinbase_calls: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
    pub(crate) header_calls: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
}

impl FakeBitcoinCoreBackboneSource {
    pub(crate) fn new(tip_height: i32, headers: BTreeMap<i32, Header>) -> Self {
        Self {
            tip_height,
            headers: Arc::new(headers),
            failures: Arc::new(BTreeSet::new()),
            coinbase_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            header_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn with_coinbase_failures(
        tip_height: i32,
        headers: BTreeMap<i32, Header>,
        failures: BTreeSet<Vec<u8>>,
    ) -> Self {
        Self {
            tip_height,
            headers: Arc::new(headers),
            failures: Arc::new(failures),
            coinbase_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
            header_calls: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    pub(crate) fn header_for_hash(&self, hash: BlockHash) -> Result<Header> {
        self.headers
            .values()
            .find(|header| header.block_hash() == hash)
            .copied()
            .with_context(|| format!("missing fake Core header for {hash}"))
    }

    pub(crate) fn coinbase_fetched(&self, hash: &[u8]) -> bool {
        self.coinbase_calls
            .lock()
            .unwrap()
            .iter()
            .any(|h| h == hash)
    }

    pub(crate) fn header_fetched(&self, hash: &[u8]) -> bool {
        self.header_calls.lock().unwrap().iter().any(|h| h == hash)
    }
}

impl BitcoinCoreBackboneSource for FakeBitcoinCoreBackboneSource {
    async fn tip(&self) -> Result<BitcoinCoreBackboneTip> {
        let header = self
            .headers
            .get(&self.tip_height)
            .with_context(|| format!("missing fake Core tip at {}", self.tip_height))?;
        Ok(BitcoinCoreBackboneTip {
            height: self.tip_height,
            hash: header.block_hash(),
        })
    }

    async fn block_hash(&self, height: i32) -> Result<BlockHash> {
        self.headers
            .get(&height)
            .map(Header::block_hash)
            .with_context(|| format!("missing fake Core hash at height {height}"))
    }

    async fn block_header(&self, hash: BlockHash) -> Result<Header> {
        self.header_calls
            .lock()
            .unwrap()
            .push(hash.to_byte_array().to_vec());
        self.header_for_hash(hash)
    }

    async fn block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        let hash_bytes = hash.to_byte_array().to_vec();
        self.coinbase_calls.lock().unwrap().push(hash_bytes.clone());
        if self.failures.contains(&hash_bytes) {
            anyhow::bail!("scripted fake Core coinbase failure for {hash}");
        }
        Ok(BitcoinCoreBlockCoinbase {
            txid: hash_bytes,
            script: b"/fake-core/".to_vec(),
            outputs: vec![0],
        })
    }
}

pub(crate) fn format_projection_error(err: ProjectionError) -> anyhow::Error {
    match err {
        ProjectionError::Api(err) => anyhow::anyhow!("api error: {}", err.message()),
        ProjectionError::Internal(err) => err,
    }
}

pub(crate) fn format_api_error(err: ApiError) -> anyhow::Error {
    anyhow::anyhow!("api error: {}", err.message())
}
