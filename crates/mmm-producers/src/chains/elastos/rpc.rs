//! Elastos (ELA) JSON-RPC client.
//!
//! Elastos.ELA is a Go reimplementation (not a Bitcoin Core fork) with a
//! hand-rolled JSON-RPC 2.0 interface. `getcurrentheight` returns the tip;
//! `getblockbyheight {"height": N}` returns block JSON carrying the child header
//! fields plus a hex `auxpow` CAuxPow blob. The endpoint is configurable
//! (`ELASTOS_RPC_URL`): the self-hosted node by default, the public
//! `https://api.elastos.io/ela` as a drop-in fallback (same JSON-RPC format).
//!
//! The Elastos block hash covers an 84-byte header (the 80-byte Bitcoin prefix
//! plus a trailing `height` field), so [`ElastosBlock::reconstruct`] hashes 84
//! bytes and verifies the result against the RPC-reported `hash`. The 80-byte
//! prefix is kept only for its `bits`/`time`; the real Elastos block hash (which
//! cryptographically binds the height) is the verified one.

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
use rpc_http::build_rpc_client;

/// Default endpoint (localhost). Override via `ELASTOS_RPC_URL` for the actual
/// node: LAN-direct `http://<node-host>:20336` for a self-hosted deployment,
/// or the public `https://api.elastos.io/ela` drop-in fallback.
pub(crate) const DEFAULT_ELASTOS_RPC_URL: &str = "http://127.0.0.1:20336";

/// Cap on the `auxpow` hex string accepted from the (possibly public, untrusted)
/// endpoint, enforced BEFORE `hex::decode` so a hostile response cannot drive a
/// huge allocation. The parser re-checks the decoded byte length as defense in
/// depth.
const MAX_ELASTOS_AUXPOW_HEX_LEN: usize = MAX_ELASTOS_AUXPOW_BYTES * 2;

/// Bounded retries for a transient (HTTP 429 / 5xx / transport) failure against
/// the public endpoint before a single request gives up.
const MAX_TRANSIENT_RETRIES: usize = 4;

/// Connection config for the Elastos endpoint. Built from `chains::config`
/// (`ELASTOS_RPC_URL`/user/password/timeout); `user`/`password` are `None` for the
/// public endpoint and only sent as basic auth when both are present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ElastosRpcConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    /// Whole-request timeout for the HTTP transport. Defaults to
    /// `ELASTOS_RPC_TIMEOUT_SECS` (15s).
    pub request_timeout: Duration,
}

/// A block returned by `getblockbyheight`. Only the fields the producer needs are
/// deserialised; the endpoint returns many more fields which serde ignores.
#[derive(Debug, Clone, Deserialize)]
pub struct ElastosBlock {
    /// RPC-reported 84-byte Elastos block hash (hex). [`Self::reconstruct`]
    /// recomputes it from the header and rejects a mismatch.
    pub hash: String,
    /// Child height. Cross-checked against the requested height before any write:
    /// a stale/misrouted response must not act on the wrong child.
    pub height: i32,
    pub version: i32,
    pub previousblockhash: String,
    pub merkleroot: String,
    pub time: u32,
    pub bits: u32,
    pub nonce: u32,
    /// Hex CAuxPow blob; absent for a non-PoW / non-AuxPoW block.
    pub auxpow: Option<String>,
    /// Decoded transaction vector from Elastos RPC. The AuxPoW verifier does not
    /// authenticate this JSON; reward attribution records it as RPC-observed
    /// decoded identity.
    #[serde(default)]
    pub tx: Vec<ElastosTransaction>,
    /// Optional top-level miner tag from the RPC response.
    pub minerinfo: Option<String>,
}

/// A decoded RPC transaction. The first tx is treated as the coinbase for
/// reward-address and minerinfo identity extraction (not authenticated by the
/// CAuxPow blob, so recorded as RPC-observed decoded identity only).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElastosTransaction {
    #[serde(default)]
    pub vout: Vec<ElastosVout>,
    pub payload: Option<ElastosTransactionPayload>,
}

/// Elastos coinbase payload. `coinbasedata` is a candidate minerinfo source
/// alongside the block-level `minerinfo` tag.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElastosTransactionPayload {
    pub coinbasedata: Option<String>,
}

/// A transaction output. The RPC spreads payout addresses across several optional
/// shapes (`address`, `addresses`, nested `scriptPubKey`); [`Self::decoded_addresses`]
/// flattens them.
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
    /// Every payout address this output exposes, flattened across the four RPC
    /// shapes (top-level `address`/`addresses` then nested `scriptPubKey`),
    /// in source order. Not deduplicated; the identity layer dedupes downstream.
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

/// Nested output script descriptor; another source of payout addresses folded in
/// by [`ElastosVout::decoded_addresses`].
#[derive(Debug, Clone, Deserialize, Default)]
pub struct ElastosScriptPubKey {
    pub address: Option<String>,
    #[serde(default)]
    pub addresses: Vec<String>,
}

/// The reconstructed child block: the 80-byte Bitcoin-shaped prefix header (for
/// its `bits`/`time`), the real Elastos 84-byte block hash (verified against the
/// RPC `hash`), the RPC height, and the decoded CAuxPow blob (if present).
#[derive(Debug, Clone)]
pub struct ReconstructedBlock {
    /// The 80-byte Bitcoin-shaped prefix, kept only for its `bits`/`time`; NOT the
    /// thing the Elastos hash covers.
    pub prefix_header: ParsedHeader,
    /// The verified 84-byte Elastos block hash (rust-bitcoin newtype, wire bytes),
    /// the value the CAuxPow commitment is checked against.
    pub block_hash: BlockHash,
    pub height: i32,
    pub time: u32,
    /// Decoded CAuxPow blob, `None` for a non-AuxPoW block (the caller skips it).
    pub auxpow: Option<Vec<u8>>,
}

impl ElastosBlock {
    /// Reconstruct the child header and compute + verify the Elastos block hash.
    ///
    /// The 80-byte Bitcoin prefix gives `bits`/`time`; the Elastos block hash is
    /// `sha256d(prefix || height_le)` (84 bytes), so the height is cryptographically
    /// bound. The computed hash MUST equal the RPC-reported `hash` (a cheap guard
    /// against an inconsistent third-party response), else this rejects the block.
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

        // Elastos block hash = sha256d(80-byte prefix || height u32 LE).
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

/// The RPC trait the Elastos capture state machine depends on, abstracted as a
/// trait so the state machine can be driven by a mock in tests (mirrors
/// [`crate::chains::hathor::rpc::HathorRpc`]).
#[allow(async_fn_in_trait)]
pub trait ElastosRpc {
    /// Fetch one block by height (`getblockbyheight`). The endpoint is untrusted,
    /// so the caller re-verifies (reconstruct, commitment, targets) before writing.
    async fn get_block_by_height(&self, height: i32) -> Result<ElastosBlock>;
}

#[derive(Debug, Clone)]
pub(crate) struct ElastosRpcClient {
    config: ElastosRpcConfig,
    http: Client,
}

impl ElastosRpcClient {
    /// Build the client over the shared `mmm_rpc` HTTP transport (connection pool,
    /// the config's whole-request timeout).
    pub(crate) fn new(config: ElastosRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self { config, http })
    }

    /// Tip height via `getcurrentheight`.
    pub(crate) async fn get_current_height(&self) -> Result<i32> {
        self.call("getcurrentheight", json!([])).await
    }

    /// One block via `getblockbyheight` (named `height` param, the Go-RPC format).
    pub(crate) async fn get_block_by_height(&self, height: i32) -> Result<ElastosBlock> {
        self.call("getblockbyheight", json!({ "height": height }))
            .await
    }

    /// JSON-RPC 2.0 call with bounded retry/backoff on transient failures (429,
    /// 5xx, transport), so a rate-limiting public endpoint does not fail a tick or
    /// a backfill height on the first hiccup. A protocol-level RPC error or a 4xx
    /// other than 429 is permanent and returned immediately.
    async fn call<T: DeserializeOwned>(&self, method: &str, params: Value) -> Result<T> {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let mut attempt = 0;
        loop {
            match self.try_call::<T>(method, &request).await {
                Ok(value) => return Ok(value),
                Err(call_err) if call_err.transient && attempt < MAX_TRANSIENT_RETRIES => {
                    attempt += 1;
                    tracing::warn!(
                        method,
                        attempt,
                        error = %call_err.err,
                        "Elastos RPC transient failure; retrying"
                    );
                    tokio::time::sleep(Duration::from_millis(200 * attempt as u64)).await;
                }
                Err(call_err) => return Err(call_err.err),
            }
        }
    }

    /// A single HTTP attempt, classifying the failure as transient (worth a retry:
    /// transport error, 429, 5xx) or permanent (decode failure, protocol-level RPC
    /// error, missing result, any other 4xx) so [`Self::call`] can decide.
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

/// An RPC error tagged with whether it is worth retrying.
struct CallError {
    err: anyhow::Error,
    transient: bool,
}

impl CallError {
    /// Tag a failure as retryable (the caller backs off and tries again).
    fn transient(err: anyhow::Error) -> Self {
        Self {
            err,
            transient: true,
        }
    }

    /// Tag a failure as non-retryable (the caller returns it immediately).
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
    fn reconstruct_rejects_hash_mismatch() {
        let mut block = block_360062();
        block.hash = "0000000000000000000000000000000000000000000000000000000000000000".to_owned();
        assert!(block.reconstruct().is_err());
    }

    #[test]
    fn reconstruct_rejects_oversize_auxpow_hex() {
        let mut block = block_360062();
        block.auxpow = Some("ab".repeat(MAX_ELASTOS_AUXPOW_HEX_LEN));
        assert!(block.reconstruct().is_err());
    }

    #[test]
    fn dummy_block_reconstructs_but_carries_zero_parent_bits() {
        // ELA 100000 is a pre-activation dummy: it still reconstructs (the child
        // header is real) but its embedded parent header has bits == 0, which the
        // capture path filters before commitment verification.
        let block: ElastosBlock = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-100000.json"
        )))
        .unwrap();
        assert!(block.reconstruct().is_ok());
    }

    #[test]
    fn config_default_url_is_the_local_node() {
        assert_eq!(DEFAULT_ELASTOS_RPC_URL, "http://127.0.0.1:20336");
    }
}
