//! RSK Ethereum-compatible JSON-RPC client (`eth_blockNumber`,
//! `eth_getBlockByNumber`, `eth_getUncleByBlockNumberAndIndex`).

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use mmm_rpc as rpc_http;
use rpc_http::{RpcMetrics, build_rpc_client};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RskRpcConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub request_timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct RskRpcClient {
    config: RskRpcConfig,
    http: Client,
    metrics: RpcMetrics,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RskBlock {
    pub hash: String,
    pub number: String,
    pub timestamp: String,
    pub miner: String,
    #[serde(default)]
    pub difficulty: Option<String>,
    #[serde(rename = "parentHash", default)]
    pub parent_hash: Option<String>,
    /// `0x`-prefixed 80-byte Bitcoin parent header, or a shorter pre-Orchid/RSKIP-92
    /// fallback signature on early blocks.
    #[serde(rename = "bitcoinMergedMiningHeader", default)]
    pub bitcoin_merged_mining_header: Option<String>,
    #[serde(rename = "bitcoinMergedMiningCoinbaseTransaction", default)]
    pub bitcoin_merged_mining_coinbase_transaction: Option<String>,
    #[serde(rename = "bitcoinMergedMiningMerkleProof", default)]
    pub bitcoin_merged_mining_merkle_proof: Option<String>,
    #[serde(rename = "hashForMergedMining", default)]
    pub hash_for_merged_mining: Option<String>,
    #[serde(default)]
    pub uncles: Vec<String>,
}

impl RskRpcClient {
    pub fn new(config: RskRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self {
            config,
            http,
            metrics: RpcMetrics::new("rsk"),
        })
    }

    pub fn metrics(&self) -> RpcMetrics {
        self.metrics.clone()
    }

    pub async fn get_block_number(&self) -> Result<i64> {
        let raw: String = self.call_required("eth_blockNumber", vec![]).await?;
        decode_quantity_i64(&raw).context("decode eth_blockNumber response")
    }

    /// Canonical block at `height` (`Ok(None)` when the node returns `{"result": null}`).
    pub async fn get_block_by_number(&self, height: i64) -> Result<Option<RskBlock>> {
        let params = vec![json!(encode_quantity(height)), json!(false)];
        self.call_optional("eth_getBlockByNumber", params).await
    }

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

    async fn call_required<T>(&self, method: &str, params: Vec<Value>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let request = json!({"jsonrpc": "2.0", "id": method, "method": method, "params": params});
        rpc_http::post_required_json_rpc_call(
            &self.transport(),
            &self.config.url,
            self.config.user.as_deref(),
            self.config.password.as_deref(),
            "RSK",
            method,
            &request,
        )
        .await
    }

    fn transport(&self) -> rpc_http::RpcTransport<'_> {
        rpc_http::RpcTransport {
            http: &self.http,
            metrics: &self.metrics,
        }
    }

    async fn call_optional<T>(&self, method: &str, params: Vec<Value>) -> Result<Option<T>>
    where
        T: DeserializeOwned,
    {
        let request = json!({"jsonrpc": "2.0", "id": method, "method": method, "params": params});
        rpc_http::post_json_rpc_call(
            &self.transport(),
            &self.config.url,
            self.config.user.as_deref(),
            self.config.password.as_deref(),
            "RSK",
            method,
            &request,
        )
        .await
    }
}

/// Decode an Ethereum-style `0x`-prefixed hex quantity to `i64`.
/// Rejects bare `"0x"` / `""` (`0` is `"0x0"`) so empty fields become
/// `MalformedSkipped` rather than silently producing height/timestamp `0`.
pub(crate) fn decode_quantity_i64(raw: &str) -> Result<i64> {
    let trimmed = raw.trim().trim_start_matches("0x");
    if trimmed.is_empty() {
        anyhow::bail!("hex quantity {raw:?} is empty; expected 0x0 for zero");
    }
    i64::from_str_radix(trimmed, 16).with_context(|| format!("parse hex quantity {raw:?} as i64"))
}

/// Decimal form of an RSK hex difficulty quantity; `None` when it is not a `u128` quantity.
pub(crate) fn decimal_difficulty(raw: &str) -> Option<String> {
    u128::from_str_radix(raw.trim().trim_start_matches("0x"), 16)
        .ok()
        .map(|value| value.to_string())
}

pub(crate) fn decode_hex_bytes(raw: &str) -> Result<Vec<u8>> {
    hex::decode(raw.trim().trim_start_matches("0x"))
        .with_context(|| format!("decode hex byte string {raw:?}"))
}

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
