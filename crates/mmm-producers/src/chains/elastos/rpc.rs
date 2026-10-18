//! Elastos (ELA) JSON-RPC client (`getcurrentheight`, `getblockbyheight`).
//!
//! The Elastos block hash covers an 84-byte header (80-byte Bitcoin prefix plus
//! trailing little-endian `height`), verified against the RPC-reported `hash` in
//! [`ElastosBlock::reconstruct`].

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitcoin::block::{Header, Version};
use bitcoin::consensus::serialize;
use bitcoin::hashes::{Hash as _, sha256d};
use bitcoin::{BlockHash, CompactTarget};
use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::chains::is_transient_http_status;
use mmm_capture::auxpow::{MAX_ELASTOS_AUXPOW_BYTES, ParsedHeader};
use mmm_rpc as rpc_http;
use rpc_http::{RpcMetrics, build_rpc_client};

pub(crate) const DEFAULT_ELASTOS_RPC_URL: &str = "http://127.0.0.1:20336";

/// Pre-decode hex cap enforced before `hex::decode`.
const MAX_ELASTOS_AUXPOW_HEX_LEN: usize = MAX_ELASTOS_AUXPOW_BYTES * 2;
const MAX_TRANSIENT_RETRIES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElastosRpcConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub request_timeout: Duration,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ElastosBlock {
    pub hash: String,
    pub height: i32,
    pub version: i32,
    pub previousblockhash: String,
    pub merkleroot: String,
    pub time: u32,
    pub bits: u32,
    pub nonce: u32,
    pub auxpow: Option<String>,
    /// Decoded transactions (unauthenticated by `CAuxPow`; used for RPC-observed identity).
    #[serde(default)]
    pub tx: Vec<ElastosTransaction>,
    pub minerinfo: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElastosTransaction {
    #[serde(default)]
    pub vout: Vec<ElastosVout>,
    pub payload: Option<ElastosTransactionPayload>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElastosTransactionPayload {
    pub coinbasedata: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElastosVout {
    pub n: Option<i32>,
    pub address: Option<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(rename = "scriptPubKey")]
    pub script_pub_key: Option<ElastosScriptPubKey>,
}

impl ElastosVout {
    /// Flatten payout addresses across top-level `address`/`addresses` and nested `scriptPubKey`.
    pub fn decoded_addresses(&self) -> Vec<&str> {
        let mut addresses = Vec::new();
        if let Some(address) = self.address.as_deref() {
            addresses.push(address);
        }
        addresses.extend(self.addresses.iter().map(String::as_str));
        if let Some(script_pub_key) = &self.script_pub_key {
            if let Some(address) = script_pub_key.address.as_deref() {
                addresses.push(address);
            }
            addresses.extend(script_pub_key.addresses.iter().map(String::as_str));
        }
        addresses
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElastosScriptPubKey {
    pub address: Option<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ReconstructedBlock {
    /// 80-byte Bitcoin-shaped prefix (kept for `bits`/`time`).
    pub prefix_header: ParsedHeader,
    /// Verified 84-byte Elastos block hash (`sha256d(prefix || height_le)`).
    pub block_hash: BlockHash,
    pub height: i32,
    pub time: u32,
    pub auxpow: Option<Vec<u8>>,
}

impl ElastosBlock {
    /// Reconstruct the child header and verify `sha256d(prefix || height_le)` against `self.hash`.
    pub fn reconstruct(&self) -> Result<ReconstructedBlock> {
        let prefix = Header {
            version: Version::from_consensus(self.version),
            prev_blockhash: self
                .previousblockhash
                .parse()
                .context("parse Elastos previousblockhash")?,
            merkle_root: self
                .merkleroot
                .parse()
                .context("parse Elastos merkleroot")?,
            time: self.time,
            bits: CompactTarget::from_consensus(self.bits),
            nonce: self.nonce,
        };

        let mut header84 = serialize(&prefix);
        debug_assert_eq!(header84.len(), Header::SIZE);
        header84.extend_from_slice(&self.height.to_le_bytes());
        let block_hash = BlockHash::from_byte_array(sha256d::Hash::hash(&header84).to_byte_array());

        let reported: BlockHash = self.hash.parse().context("parse RPC-reported block hash")?;
        ensure!(
            block_hash == reported,
            "reconstructed Elastos block hash {block_hash} != RPC-reported {reported}"
        );

        let auxpow = match self.auxpow.as_deref().map(str::trim) {
            Some(hex_str) if !hex_str.is_empty() => {
                ensure!(
                    hex_str.len() <= MAX_ELASTOS_AUXPOW_HEX_LEN,
                    "Elastos auxpow hex is {} chars, over the {MAX_ELASTOS_AUXPOW_HEX_LEN} cap",
                    hex_str.len()
                );
                Some(hex::decode(hex_str).context("decode Elastos auxpow hex")?)
            }
            _ => None,
        };

        Ok(ReconstructedBlock {
            prefix_header: ParsedHeader { header: prefix },
            block_hash,
            height: self.height,
            time: self.time,
            auxpow,
        })
    }
}

#[allow(async_fn_in_trait)]
pub trait ElastosRpc {
    async fn get_block_by_height(&self, height: i32) -> Result<ElastosBlock>;
}

#[derive(Debug, Clone)]
pub(crate) struct ElastosRpcClient {
    config: ElastosRpcConfig,
    http: Client,
    metrics: RpcMetrics,
}

impl ElastosRpcClient {
    pub(crate) fn new(config: ElastosRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self {
            config,
            http,
            metrics: RpcMetrics::new("elastos"),
        })
    }

    pub(crate) fn metrics(&self) -> RpcMetrics {
        self.metrics.clone()
    }

    pub(crate) async fn get_current_height(&self) -> Result<i32> {
        self.call("getcurrentheight", json!([])).await
    }

    pub(crate) async fn get_block_by_height(&self, height: i32) -> Result<ElastosBlock> {
        self.call("getblockbyheight", json!({ "height": height }))
            .await
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut attempt = 0;
        loop {
            let result = {
                let _attempt = self.metrics.attempt(1);
                self.try_call::<T>(method, &request).await
            };
            match result {
                Ok(value) => return Ok(value),
                Err(call_err) if call_err.transient && attempt < MAX_TRANSIENT_RETRIES => {
                    attempt += 1;
                    self.metrics.record_retry();
                    tracing::warn!(
                        method,
                        attempt,
                        error = %call_err.err,
                        "Elastos RPC transient failure; retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                }
                Err(call_err) => {
                    self.metrics.record_failure();
                    return Err(call_err.err);
                }
            }
        }
    }

    async fn try_call<T: DeserializeOwned>(
        &self,
        method: &str,
        request: &Value,
    ) -> std::result::Result<T, CallError> {
        let response = rpc_http::post_json_rpc(
            &self.http,
            &self.config.url,
            self.config.user.as_deref(),
            self.config.password.as_deref(),
            "Elastos",
            method,
            request,
        )
        .await
        .map_err(CallError::transient)?;

        let status = response.status();
        if !status.is_success() {
            let transient = is_transient_http_status(status);
            let err = anyhow::anyhow!("Elastos RPC HTTP {status} for method {method}");
            return Err(CallError { err, transient });
        }

        rpc_http::decode_required_json_rpc_response("Elastos", method, response)
            .await
            .map_err(CallError::permanent)
    }
}

impl ElastosRpc for ElastosRpcClient {
    async fn get_block_by_height(&self, height: i32) -> Result<ElastosBlock> {
        ElastosRpcClient::get_block_by_height(self, height).await
    }
}

struct CallError {
    err: anyhow::Error,
    transient: bool,
}

impl CallError {
    fn transient(err: anyhow::Error) -> Self {
        Self {
            err,
            transient: true,
        }
    }

    fn permanent(err: anyhow::Error) -> Self {
        Self {
            err,
            transient: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block_360062() -> ElastosBlock {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-360062.json"
        )))
        .expect("deserialize Elastos fixture")
    }

    #[test]
    fn reconstruct_computes_and_verifies_the_84_byte_block_hash() {
        let recon = block_360062().reconstruct().expect("reconstruct");
        assert_eq!(
            recon.block_hash.to_string(),
            "427b83fd71e7494601d841736c91b85a04251b41d029dda4a12d7ba8a1cd1b9b",
        );
        assert_eq!(recon.height, 360_062);
        // bits / time come from the 80-byte prefix header.
        assert_eq!(recon.time, 1_555_689_043);
        assert_eq!(
            recon.prefix_header.bits(),
            CompactTarget::from_consensus(402_839_749)
        );
        assert!(recon.auxpow.is_some());
    }

    #[test]
    fn reconstruct_rejects_oversize_auxpow_hex() {
        let mut block = block_360062();
        block.auxpow = Some("ab".repeat(MAX_ELASTOS_AUXPOW_HEX_LEN));
        assert!(block.reconstruct().is_err());
    }

    #[test]
    fn config_default_url_is_the_local_node() {
        assert_eq!(DEFAULT_ELASTOS_RPC_URL, "http://127.0.0.1:20336");
    }
}
