//! The shared thin JSON-RPC client for bitcoind-family chains.
//!
//! Namecoin, Syscoin, and Fractal expose Bitcoin Core-style RPC; the only
//! per-chain differences are configuration (endpoint, auth mode, timeout) and
//! which proof-fetch method the capture path calls, so ONE client serves all
//! three. The chain label parameterizes error contexts; auth material arrives
//! resolved from `chains::config` (this module never reads process env).

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use bitcoin::BlockHash;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use mmm_rpc as rpc_http;
use rpc_http::build_rpc_client;

/// Resolved transport configuration for a bitcoind-family endpoint. Built by
/// `chains::config::bitcoind_rpc_config`; carries values, never env var names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BitcoindRpcConfig {
    /// Full JSON-RPC endpoint URL (scheme, host, port).
    pub url: String,
    /// HTTP basic-auth user, paired with `password`; both `None` for an
    /// unauthenticated endpoint (auth header sent only when both are present).
    pub user: Option<String>,
    /// HTTP basic-auth password, paired with `user`.
    pub password: Option<String>,
    /// Whole-request timeout for the HTTP transport. Defaults to
    /// `<PREFIX>_RPC_TIMEOUT_SECS` (15s) and prevents a stale tunnel socket
    /// from hanging the live poller's tip fetch indefinitely.
    pub request_timeout: Duration,
}

/// Thin JSON-RPC client over a shared reqwest transport. One instance serves
/// any bitcoind-family chain; the chain `label` only colors error contexts.
#[derive(Debug, Clone)]
pub(crate) struct BitcoindRpcClient {
    /// Chain label for error contexts ("Namecoin", "Syscoin", "Fractal").
    label: &'static str,
    config: BitcoindRpcConfig,
    http: Client,
}

impl BitcoindRpcClient {
    /// Build the client, materializing the reqwest transport with the config's
    /// request timeout. `label` is folded into every later error context.
    pub(crate) fn new(label: &'static str, config: BitcoindRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self {
            label,
            config,
            http,
        })
    }

    /// Current chain height via `getblockcount`, the poller's tip bound.
    pub(crate) async fn get_block_count(&self) -> Result<i32> {
        self.call("getblockcount", vec![]).await
    }

    /// Canonical block hash at `height` via `getblockhash`. Parses the RPC hex
    /// through the rust-bitcoin newtype, which reverses display order back to
    /// internal byte order on the way in.
    pub(crate) async fn get_block_hash(&self, height: i32) -> Result<BlockHash> {
        let hash: String = self.call("getblockhash", vec![json!(height)]).await?;
        BlockHash::from_str(&hash).with_context(|| format!("parse {} block hash", self.label))
    }

    /// Fetch the full raw block via `getblock <hash> 0` (carries the CAuxPow
    /// inline for Namecoin/Syscoin).
    pub(crate) async fn get_block_raw(&self, hash: &BlockHash) -> Result<Vec<u8>> {
        let raw_hex: String = self
            .call("getblock", vec![json!(hash.to_string()), json!(0)])
            .await?;
        hex::decode(&raw_hex).with_context(|| format!("decode {} raw block hex", self.label))
    }

    /// Fetch the `[child header][CAuxPow]` bytes via `getblockheader <hash>
    /// false true` (verbose=false returns hex; the trailing `true` includes the
    /// AuxPoW tail). Fractal's `getblock 0` does NOT carry the CAuxPow, so this
    /// is its merge-mining proof source.
    pub(crate) async fn get_header_with_auxpow(&self, hash: &BlockHash) -> Result<Vec<u8>> {
        let raw_hex: String = self
            .call(
                "getblockheader",
                vec![json!(hash.to_string()), json!(false), json!(true)],
            )
            .await?;
        hex::decode(&raw_hex)
            .with_context(|| format!("decode {} getblockheader-auxpow hex", self.label))
    }

    /// Issue one JSON-RPC 1.0 call and deserialize the `result`. Layers error
    /// contexts at each failure stage (send, HTTP status, body decode), surfaces
    /// a JSON-RPC `error` object as a labeled bail, and treats a `null` result
    /// as a hard error. Basic auth is attached only when both user and password
    /// are configured.
    async fn call<T>(&self, method: &str, params: Vec<Value>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let label = self.label;
        let request = json!({"jsonrpc": "1.0", "id": method, "method": method, "params": params});
        let response = rpc_http::post_json_rpc_for_status(
            &self.http,
            &self.config.url,
            self.config.user.as_deref(),
            self.config.password.as_deref(),
            label,
            method,
            &request,
        )
        .await?;
        rpc_http::decode_required_json_rpc_response(label, method, response).await
    }
}
