//! Thin RSK JSON-RPC client.
//!
//! RSK exposes Ethereum-compatible `eth_*` methods. The structure/capture
//! slice only needs three of them:
//!
//! - `eth_blockNumber`: current chain tip
//! - `eth_getBlockByNumber(blocknum, include_txs=false)`: canonical block
//!   with merge-mining metadata
//! - `eth_getUncleByBlockNumberAndIndex(blocknum, idx)`: uncle/ommer block
//!   carrying its own merge-mining metadata
//!
//! Block numbers, timestamps, and the integer `difficulty` are returned as
//! `0x`-prefixed hex strings; byte fields are also `0x`-prefixed hex. The
//! client strips the prefix and decodes lazily so callers see typed values.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use mmm_rpc as rpc_http;
use rpc_http::build_rpc_client;

/// Connection settings for the RSK JSON-RPC endpoint. Built from env by
/// `chains::config::rsk_rpc_config`; optional basic-auth credentials are only
/// sent when both `user` and `password` are present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RskRpcConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Whole-request timeout for the HTTP transport. Defaults to
    /// `RSK_RPC_TIMEOUT_SECS` (15s) and prevents a stale tunnel socket from
    /// hanging the live poller's tip fetch indefinitely.
    pub request_timeout: Duration,
}

/// Thin `eth_*` JSON-RPC client over a shared `reqwest::Client`. Cheap to
/// `Clone` (the inner HTTP client is `Arc`-backed), which the backfill prefetch
/// pipeline relies on to clone one source per in-flight height.
#[derive(Debug, Clone)]
pub struct RskRpcClient {
    config: RskRpcConfig,
    http: Client,
}

/// Raw RSK block fields returned by `eth_getBlockByNumber` and
/// `eth_getUncleByBlockNumberAndIndex`. Numeric fields stay as hex strings;
/// the capture path decodes them with [`decode_quantity_i64`] and
/// [`decode_hex_bytes`].
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RskBlock {
    /// `0x`-prefixed 32-byte block hash.
    pub hash: String,
    /// `0x`-prefixed hex-encoded block number.
    pub number: String,
    /// `0x`-prefixed hex-encoded unix timestamp.
    pub timestamp: String,
    /// `0x`-prefixed 20-byte RSK miner address.
    pub miner: String,
    /// `0x`-prefixed hex-encoded integer difficulty.
    #[serde(default)]
    pub difficulty: Option<String>,
    /// `0x`-prefixed 80-byte Bitcoin parent header. Pre-RSKIP-92 blocks
    /// return a non-80-byte payload here; callers must validate the length.
    #[serde(rename = "bitcoinMergedMiningHeader", default)]
    pub bitcoin_merged_mining_header: Option<String>,
    /// `0x`-prefixed coinbase transaction tail (RSKIP-92 midstate scheme).
    #[serde(rename = "bitcoinMergedMiningCoinbaseTransaction", default)]
    pub bitcoin_merged_mining_coinbase_transaction: Option<String>,
    /// `0x`-prefixed merkle proof bytes; stored opaquely until RSKj era
    /// boundaries are confirmed.
    #[serde(rename = "bitcoinMergedMiningMerkleProof", default)]
    pub bitcoin_merged_mining_merkle_proof: Option<String>,
    /// `0x`-prefixed 32-byte hash committed in the BTC coinbase.
    #[serde(rename = "hashForMergedMining", default)]
    pub hash_for_merged_mining: Option<String>,
    /// Canonical-only: list of uncle hashes. Walk via
    /// [`RskRpcClient::get_uncle_by_block_number_and_index`].
    #[serde(default)]
    pub uncles: Vec<String>,
}

impl RskRpcClient {
    /// Build a client over a `request_timeout`-bounded HTTP transport so a stale
    /// tunnel socket cannot hang the poller's tip fetch indefinitely.
    pub fn new(config: RskRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self { config, http })
    }

    /// Current chain tip via `eth_blockNumber`. A null result is an error: a
    /// healthy node never returns null for the tip.
    pub async fn get_block_number(&self) -> Result<i64> {
        let raw: String = self.call_required("eth_blockNumber", vec![]).await?;
        decode_quantity_i64(&raw).context("decode eth_blockNumber response")
    }

    /// Canonical block at `height` (txs excluded). `Ok(None)` when RSKj returns
    /// `{"result": null}` for an absent block, so the caller can `Hold`/skip the
    /// height rather than treating it as an error.
    pub async fn get_block_by_number(&self, height: i64) -> Result<Option<RskBlock>> {
        let params = vec![json!(encode_quantity(height)), json!(false)];
        self.call_optional("eth_getBlockByNumber", params).await
    }

    /// One uncle of the canonical block at `block_height` by listed index.
    /// `Ok(None)` for a `null` result (a listed uncle the node no longer serves),
    /// matching [`get_block_by_number`](Self::get_block_by_number).
    pub async fn get_uncle_by_block_number_and_index(
        &self,
        block_height: i64,
        uncle_index: i32,
    ) -> Result<Option<RskBlock>> {
        let params = vec![
            json!(encode_quantity(block_height)),
            json!(encode_quantity(uncle_index as i64)),
        ];
        self.call_optional("eth_getUncleByBlockNumberAndIndex", params)
            .await
    }

    /// Call an RSK RPC method whose `result` must be non-null. A JSON `null`
    /// result is treated as an error (eth_blockNumber et al. never return
    /// null on a healthy node).
    async fn call_required<T>(&self, method: &str, params: Vec<Value>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.call_optional(method, params)
            .await?
            .ok_or_else(|| anyhow::anyhow!("RSK RPC method {method} returned a null result"))
    }

    /// Call an RSK RPC method whose `result` may be JSON `null` (the
    /// canonical way RSKj signals "no such block" / "no such uncle" without
    /// an `error` object). Explicit `null` becomes `Ok(None)`; a response
    /// missing both `result` and `error` is treated as a protocol violation
    /// and surfaces as `Err` rather than silently masquerading as a missing
    /// block.
    async fn call_optional<T>(&self, method: &str, params: Vec<Value>) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let request = json!({"jsonrpc": "2.0", "id": method, "method": method, "params": params});
        let response = rpc_http::post_json_rpc_for_status(
            &self.http,
            &self.config.url,
            self.config.user.as_deref(),
            self.config.password.as_deref(),
            "RSK",
            method,
            &request,
        )
        .await?;
        rpc_http::decode_json_rpc_response("RSK", method, response).await
    }
}

/// Decode an Ethereum-style `0x`-prefixed hex quantity to i64.
///
/// RSK heights and timestamps fit comfortably in i64 across the chain's
/// lifetime; we use signed types throughout the producer to match the
/// existing schema (`INTEGER` heights, `BIGINT` epoch timestamps).
///
/// Per the JSON-RPC hex quantity spec, zero is encoded `0x0` (one digit),
/// not the bare `0x` prefix. An empty quantity is treated as malformed so
/// downstream callers can convert that to a `MalformedSkipped` outcome
/// instead of writing `height = 0` rows that look like a valid genesis
/// record.
pub(crate) fn decode_quantity_i64(raw: &str) -> Result<i64> {
    let trimmed = raw.trim().trim_start_matches("0x");
    if trimmed.is_empty() {
        anyhow::bail!("hex quantity {raw:?} is empty; expected 0x0 for zero");
    }
    i64::from_str_radix(trimmed, 16).with_context(|| format!("parse hex quantity {raw:?} as i64"))
}

/// Decode an Ethereum-style `0x`-prefixed hex byte string.
pub(crate) fn decode_hex_bytes(raw: &str) -> Result<Vec<u8>> {
    hex::decode(raw.trim().trim_start_matches("0x"))
        .with_context(|| format!("decode hex byte string {raw:?}"))
}

/// Encode an i64 as an Ethereum-style `0x`-prefixed minimal hex quantity.
pub(crate) fn encode_quantity(value: i64) -> String {
    if value == 0 {
        "0x0".to_owned()
    } else {
        format!("0x{value:x}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_quantity() {
        assert_eq!(decode_quantity_i64("0x0").unwrap(), 0);
        assert_eq!(decode_quantity_i64("0x1f").unwrap(), 31);
        assert_eq!(decode_quantity_i64("1f").unwrap(), 31);
        assert_eq!(decode_quantity_i64("0x8773a1").unwrap(), 8_876_961);
    }

    #[test]
    fn empty_quantity_is_rejected_not_silently_zero() {
        // Per the JSON-RPC hex-quantity spec, zero is encoded `0x0`. A bare
        // `0x` is malformed; treating it as zero would let degraded RPC
        // responses sneak into the producer as height/time 0 rows.
        let err = decode_quantity_i64("0x").unwrap_err();
        assert!(err.to_string().contains("empty"), "unexpected error: {err}");
        let err = decode_quantity_i64("").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn decodes_hex_bytes() {
        assert_eq!(
            decode_hex_bytes("0xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            decode_hex_bytes("deadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert!(decode_hex_bytes("0xnotvalidhex").is_err());
    }

    #[test]
    fn encodes_quantity_minimally() {
        assert_eq!(encode_quantity(0), "0x0");
        assert_eq!(encode_quantity(1), "0x1");
        assert_eq!(encode_quantity(15), "0xf");
        assert_eq!(encode_quantity(255), "0xff");
        assert_eq!(encode_quantity(139_999), "0x222df");
    }
}
