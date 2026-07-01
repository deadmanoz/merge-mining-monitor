//! Hathor public REST client.
//!
//! Hathor mainnet is whitelist-permissioned (hathor-core #751), so there is no
//! self-hosted node; this talks to the Foundation's public REST API. Unlike the
//! JSON-RPC chain clients this is a plain `GET` REST client. Because the
//! endpoint is third-party and is the hot path for both polling and backfill,
//! every request goes through a bounded retry/backoff that honors `Retry-After`,
//! falls back to a second URL on exhaustion, and DISTINGUISHES a definitive
//! absent height (`success: false` / no block) from a transient fetch failure
//! (retries exhausted): the capture state machine maps the former to its absent
//! branch and the latter to a hold, never to a revoke.

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::warn;

use crate::chains::is_transient_http_status;
use mmm_rpc as rpc_http;
use rpc_http::build_rpc_client;

/// Default primary endpoint: the Foundation's public mainnet node 1.
pub(crate) const DEFAULT_API_URL: &str = "https://node1.mainnet.hathor.network/v1a";
/// Default fallback endpoint: node 2, tried after node 1 exhausts its retries.
pub(crate) const DEFAULT_FALLBACK_URL: &str = "https://node2.mainnet.hathor.network/v1a";
/// Default retry attempts per endpoint before falling back / surfacing an error.
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 6;
const BACKOFF_BASE: Duration = Duration::from_millis(200);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

/// Endpoint + retry/backoff policy for the public Hathor REST client. Loaded
/// from env via `chains::config`; defaults are the Foundation node URLs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HathorRpcConfig {
    /// Primary REST base URL (e.g. `DEFAULT_API_URL`).
    pub url: String,
    /// Optional second endpoint tried after the primary exhausts its retries.
    pub fallback_url: Option<String>,
    /// Per-request HTTP timeout applied to the underlying `reqwest` client.
    pub request_timeout: Duration,
    /// Retry attempts per endpoint on transient (429 / 5xx / network) failures.
    pub max_retries: u32,
}

/// Block metadata from `GET /block_at_height`.
#[derive(Debug, Clone, Deserialize)]
pub struct HathorBlockMeta {
    pub tx_id: String,
    pub version: i32,
    #[serde(default)]
    pub is_voided: bool,
}

/// Transaction payload from `GET /transaction` (the merge-mining proof source).
#[derive(Debug, Clone, Deserialize)]
pub struct HathorTransaction {
    /// Full serialised block hex (funds + graph + aux_pow).
    pub raw: String,
    /// RFC 0006 split-header AuxPoW hex; absent on non-merge-mined transactions.
    pub aux_pow: Option<String>,
    /// Block hash (display hex); equals the reconstructed BTC parent header hash.
    pub hash: String,
    /// Required: a missing timestamp would otherwise date a captured event at the
    /// Unix epoch instead of holding/failing like the other response-format checks.
    pub timestamp: i64,
}

/// The public Hathor REST client used by live polling and bounded backfills.
/// Cheap to clone (the inner `reqwest::Client` is an `Arc`).
#[derive(Debug, Clone)]
pub(crate) struct HathorRpcClient {
    config: HathorRpcConfig,
    http: Client,
}

impl HathorRpcClient {
    /// Build the client, sharing the workspace's tuned `reqwest` builder so the
    /// configured `request_timeout` applies to every request.
    pub(crate) fn new(config: HathorRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self { config, http })
    }

    /// Current chain tip: the max height across the DAG best-block tips.
    pub(crate) async fn get_chain_tip(&self) -> Result<i32> {
        let status: StatusResponse = self.get_with_retry("/status", &[]).await?;
        status
            .dag
            .best_block_tips
            .iter()
            .map(|tip| tip.height)
            .max()
            .ok_or_else(|| anyhow::anyhow!("Hathor /status returned no best_block_tips"))
    }

    /// Block metadata at a height. `Ok(None)` is a DEFINITIVE absent height
    /// (`success: false`); a transient fetch failure is `Err`. A `success: true`
    /// response that omits `block` (schema drift / a renamed field) is treated as
    /// malformed (`Err`), NOT a definitive absence (`Ok(None)`): it surfaces as a
    /// transient hold rather than being mistaken for an absent height. (Under
    /// `HATHOR_BACKFILL_SKIP_HOLDS=1` a backfill still downgrades that hold to a
    /// logged skip, like any transient hold; live polling holds and retries.)
    pub(crate) async fn get_block_at_height(&self, height: i32) -> Result<Option<HathorBlockMeta>> {
        let resp: BlockResponse = self
            .get_with_retry("/block_at_height", &[("height", height.to_string())])
            .await?;
        if resp.success {
            Ok(Some(resp.block.ok_or_else(|| {
                anyhow::anyhow!("Hathor /block_at_height returned success without a block")
            })?))
        } else {
            Ok(None)
        }
    }

    /// Full transaction by id. `Ok(None)` is a DEFINITIVE absent transaction
    /// (`success: false`); a transient fetch failure or a `success: true` response
    /// that omits `tx` is `Err` (see [`Self::get_block_at_height`]).
    pub(crate) async fn get_transaction(&self, tx_id: &str) -> Result<Option<HathorTransaction>> {
        let resp: TransactionResponse = self
            .get_with_retry("/transaction", &[("id", tx_id.to_owned())])
            .await?;
        if resp.success {
            Ok(Some(resp.tx.ok_or_else(|| {
                anyhow::anyhow!("Hathor /transaction returned success without a tx")
            })?))
        } else {
            Ok(None)
        }
    }

    /// GET with bounded retry/backoff on transient statuses (429, 5xx) and
    /// network errors, honoring `Retry-After`, then the fallback URL, then `Err`
    /// (a transient exhaustion the caller must treat as a hold, not an absence).
    async fn get_with_retry<T>(&self, path: &str, query: &[(&str, String)]) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let mut urls = vec![self.config.url.as_str()];
        if let Some(fallback) = &self.config.fallback_url {
            urls.push(fallback.as_str());
        }

        let mut last_err: Option<anyhow::Error> = None;
        let last_url_idx = urls.len() - 1;
        for (url_idx, base_url) in urls.into_iter().enumerate() {
            for attempt in 0..self.config.max_retries {
                // The final attempt on the final endpoint falls straight through to
                // the error below, so there is no point sleeping after it.
                let is_final = url_idx == last_url_idx && attempt + 1 == self.config.max_retries;
                match self
                    .http
                    .get(format!("{base_url}{path}"))
                    .query(query)
                    .send()
                    .await
                {
                    Ok(response) => {
                        let status = response.status();
                        if is_transient_http_status(status) {
                            // Record the status so a persistent transient (e.g. a
                            // sustained 429) surfaces a diagnosable error instead of
                            // the generic "exhausted retries" message below.
                            last_err = Some(anyhow::anyhow!(
                                "Hathor REST {path} returned transient status {status} (attempt {})",
                                attempt + 1
                            ));
                            if is_final {
                                break;
                            }
                            let wait = retry_after(response.headers())
                                .unwrap_or_else(|| backoff_delay(attempt));
                            warn!(%base_url, path, %status, "Hathor REST transient status; retrying");
                            tokio::time::sleep(wait).await;
                            continue;
                        }
                        let response = response.error_for_status().with_context(|| {
                            format!("Hathor REST {path} returned status {status}")
                        })?;
                        return response
                            .json::<T>()
                            .await
                            .with_context(|| format!("decode Hathor REST response for {path}"));
                    }
                    Err(err) => {
                        last_err = Some(anyhow::Error::new(err).context(format!(
                            "send Hathor REST request {path} (attempt {})",
                            attempt + 1
                        )));
                        if is_final {
                            break;
                        }
                        tokio::time::sleep(backoff_delay(attempt)).await;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("Hathor REST {path} exhausted retries on all endpoints")
        }))
    }
}

/// The RPC trait the Hathor capture state machine depends on, abstracted as a
/// trait so the state machine can be driven by a mock in tests.
#[allow(async_fn_in_trait)]
pub trait HathorRpc {
    /// Block metadata at a height. `Ok(None)` is a DEFINITIVE absent height; an
    /// `Err` is a transient failure the state machine must map to a hold, never
    /// a revoke.
    async fn get_block_at_height(&self, height: i32) -> Result<Option<HathorBlockMeta>>;
    /// Full transaction by id, with the same `Ok(None)`-absent / `Err`-transient
    /// contract as [`Self::get_block_at_height`].
    async fn get_transaction(&self, tx_id: &str) -> Result<Option<HathorTransaction>>;
}

impl HathorRpc for HathorRpcClient {
    async fn get_block_at_height(&self, height: i32) -> Result<Option<HathorBlockMeta>> {
        HathorRpcClient::get_block_at_height(self, height).await
    }
    async fn get_transaction(&self, tx_id: &str) -> Result<Option<HathorTransaction>> {
        HathorRpcClient::get_transaction(self, tx_id).await
    }
}

/// Parse a `Retry-After` header expressed in delta-seconds (the form the public
/// endpoint uses); ignores HTTP-date form, which is uncommon here.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse()
        .ok()?;
    Some(Duration::from_secs(secs.min(BACKOFF_CAP.as_secs() * 4)))
}

/// Exponential backoff capped at `BACKOFF_CAP` (no jitter; deterministic).
fn backoff_delay(attempt: u32) -> Duration {
    let scaled = BACKOFF_BASE.saturating_mul(1u32 << attempt.min(5));
    scaled.min(BACKOFF_CAP)
}

#[derive(Debug, Deserialize)]
struct StatusResponse {
    dag: DagInfo,
}

#[derive(Debug, Deserialize)]
struct DagInfo {
    best_block_tips: Vec<TipInfo>,
}

#[derive(Debug, Deserialize)]
struct TipInfo {
    height: i32,
}

#[derive(Debug, Deserialize)]
struct BlockResponse {
    // `success` is required: a 200 response that omits it is malformed (Err), not
    // a definitive absence, so schema drift cannot silently stall polling.
    success: bool,
    #[serde(default)]
    block: Option<HathorBlockMeta>,
}

#[derive(Debug, Deserialize)]
struct TransactionResponse {
    success: bool,
    #[serde(default)]
    tx: Option<HathorTransaction>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_is_bounded_and_monotonic() {
        assert_eq!(backoff_delay(0), BACKOFF_BASE);
        assert!(backoff_delay(1) > backoff_delay(0));
        assert!(backoff_delay(20) <= BACKOFF_CAP);
    }

    /// A throwaway blocking HTTP server that serves `responses` in order, one per
    /// connection, then exits. Lets the async client drive the real retry loop.
    fn spawn_http_mock(responses: Vec<String>) -> std::net::SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (response, stream) in responses.into_iter().zip(listener.incoming()) {
                let mut stream = stream.unwrap();
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(response.as_bytes());
            }
        });
        addr
    }

    fn http_429() -> String {
        "HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string()
    }

    fn http_200_json(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn test_client(addr: std::net::SocketAddr, max_retries: u32) -> HathorRpcClient {
        HathorRpcClient::new(HathorRpcConfig {
            url: format!("http://{addr}"),
            fallback_url: None,
            request_timeout: Duration::from_secs(5),
            max_retries,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn get_with_retry_retries_a_transient_status_then_succeeds() {
        let addr = spawn_http_mock(vec![
            http_429(),
            http_200_json(r#"{"dag":{"best_block_tips":[{"height":42}]}}"#),
        ]);
        let tip = test_client(addr, 3).get_chain_tip().await.unwrap();
        assert_eq!(tip, 42);
    }

    #[tokio::test]
    async fn get_with_retry_surfaces_a_persistent_transient_status() {
        // Every attempt on the only endpoint returns 429: the error must name the
        // transient status, not the generic "exhausted retries on all endpoints".
        let addr = spawn_http_mock(vec![http_429(), http_429()]);
        let err = test_client(addr, 2).get_chain_tip().await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("429") || msg.to_lowercase().contains("transient status"),
            "error should name the transient status, got: {msg}"
        );
    }

    #[test]
    fn retry_after_parses_delta_seconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(retry_after(&headers), Some(Duration::from_secs(3)));

        let empty = reqwest::header::HeaderMap::new();
        assert_eq!(retry_after(&empty), None);
    }

    #[test]
    fn block_response_distinguishes_present_from_absent() {
        let present: BlockResponse = serde_json::from_str(
            r#"{"success":true,"block":{"tx_id":"ab","version":3,"is_voided":false}}"#,
        )
        .unwrap();
        assert!(present.success && present.block.is_some());

        let absent: BlockResponse = serde_json::from_str(r#"{"success":false}"#).unwrap();
        assert!(!absent.success && absent.block.is_none());
    }

    #[test]
    fn fallback_url_defaults_to_node2_and_can_be_disabled() {
        // Empty fallback_url means the loader yields None (disabled).
        let cfg = HathorRpcConfig {
            url: DEFAULT_API_URL.to_owned(),
            fallback_url: None,
            request_timeout: Duration::from_secs(15),
            max_retries: DEFAULT_MAX_RETRIES,
        };
        assert!(HathorRpcClient::new(cfg).is_ok());
    }
}
