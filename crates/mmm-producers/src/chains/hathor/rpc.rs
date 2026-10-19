//! Hathor public REST client (`/status`, `/block_at_height`, `/transaction`).
//!
//! Uses bounded retry/backoff honoring `Retry-After` with fallback URL support,
//! and distinguishes a definitive absent height/tx (`success: false` -> `Ok(None)`)
//! from a transient or malformed response (`Err`).

use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tracing::warn;

use crate::chains::is_transient_http_status;
use mmm_rpc as rpc_http;
use rpc_http::{RpcMetrics, build_rpc_client};

pub(crate) const DEFAULT_API_URL: &str = "https://node1.mainnet.hathor.network/v1a";
pub(crate) const DEFAULT_FALLBACK_URL: &str = "https://node2.mainnet.hathor.network/v1a";
pub(crate) const DEFAULT_MAX_RETRIES: u32 = 6;
const BACKOFF_BASE: Duration = Duration::from_millis(200);
const BACKOFF_CAP: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HathorRpcConfig {
    pub url: String,
    pub fallback_url: Option<String>,
    pub request_timeout: Duration,
    pub max_retries: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HathorBlockMeta {
    pub tx_id: String,
    pub version: i32,
    pub height: i32,
    #[serde(default)]
    pub is_voided: bool,
    #[serde(default)]
    pub timestamp: Option<i64>,
    #[serde(default)]
    pub weight: Option<f64>,
    /// Display-order hashes; a block's first parent is its parent block.
    #[serde(default)]
    pub parents: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HathorTransaction {
    pub raw: String,
    pub aux_pow: Option<String>,
    pub hash: String,
    pub timestamp: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct HathorRpcClient {
    config: HathorRpcConfig,
    http: Client,
    metrics: RpcMetrics,
}

impl HathorRpcClient {
    pub(crate) fn new(config: HathorRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self {
            config,
            http,
            metrics: RpcMetrics::new("hathor"),
        })
    }

    pub(crate) fn metrics(&self) -> RpcMetrics {
        self.metrics.clone()
    }

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

    /// Block metadata at `height`. `Ok(None)` is a definitive absence (`success: false`);
    /// a `success: true` response without `block` or a transport failure is `Err`.
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
                let is_final = url_idx == last_url_idx && attempt + 1 == self.config.max_retries;
                let outcome = {
                    let _attempt = self.metrics.attempt(1);
                    self.try_get::<T>(base_url, path, query).await
                };
                let retry_after = match outcome {
                    GetOutcome::Ok(value) => return Ok(value),
                    GetOutcome::Failed(err) => {
                        self.metrics.record_failure();
                        return Err(err);
                    }
                    GetOutcome::Transient {
                        err,
                        status,
                        retry_after,
                    } => {
                        if let Some(status) = status
                            && !is_final
                        {
                            warn!(%base_url, path, %status, "Hathor REST transient status; retrying");
                        }
                        last_err = Some(err.context(format!("attempt {}", attempt + 1)));
                        retry_after
                    }
                };
                if is_final {
                    break;
                }
                self.metrics.record_retry();
                tokio::time::sleep(retry_after.unwrap_or_else(|| backoff_delay(attempt))).await;
            }
        }
        self.metrics.record_failure();
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("Hathor REST {path} exhausted retries on all endpoints")
        }))
    }

    async fn try_get<T>(
        &self,
        base_url: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> GetOutcome<T>
    where
        T: DeserializeOwned,
    {
        let response = match self
            .http
            .get(format!("{base_url}{path}"))
            .query(query)
            .send()
            .await
        {
            Ok(response) => response,
            Err(err) => {
                return GetOutcome::Transient {
                    err: anyhow::Error::new(err)
                        .context(format!("send Hathor REST request {path}")),
                    status: None,
                    retry_after: None,
                };
            }
        };
        let status = response.status();
        if is_transient_http_status(status) {
            return GetOutcome::Transient {
                err: anyhow::anyhow!("Hathor REST {path} returned transient status {status}"),
                status: Some(status),
                retry_after: retry_after(response.headers()),
            };
        }
        let decoded = async {
            response
                .error_for_status()
                .with_context(|| format!("Hathor REST {path} returned status {status}"))?
                .json::<T>()
                .await
                .with_context(|| format!("decode Hathor REST response for {path}"))
        }
        .await;
        match decoded {
            Ok(value) => GetOutcome::Ok(value),
            Err(err) => GetOutcome::Failed(err),
        }
    }
}

enum GetOutcome<T> {
    Ok(T),
    Transient {
        err: anyhow::Error,
        status: Option<reqwest::StatusCode>,
        retry_after: Option<Duration>,
    },
    Failed(anyhow::Error),
}

#[allow(async_fn_in_trait)]
pub trait HathorRpc {
    async fn get_block_at_height(&self, height: i32) -> Result<Option<HathorBlockMeta>>;
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
        let client = test_client(addr, 3);
        let tip = client.get_chain_tip().await.unwrap();
        assert_eq!(tip, 42);
        let snapshot = client.metrics().snapshot();
        assert_eq!(snapshot.http_attempts, 2);
        assert_eq!(snapshot.retries, 1);
        assert_eq!(snapshot.failures, 0);
    }

    #[tokio::test]
    async fn get_with_retry_surfaces_a_persistent_transient_status() {
        // Every attempt on the only endpoint returns 429: the error must name the
        // transient status, not the generic "exhausted retries on all endpoints".
        let addr = spawn_http_mock(vec![http_429(), http_429()]);
        let client = test_client(addr, 2);
        let err = client.get_chain_tip().await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("429") || msg.to_lowercase().contains("transient status"),
            "error should name the transient status, got: {msg}"
        );
        // Two attempts, one retry between them, and the exhausted call is the
        // one failure.
        let snapshot = client.metrics().snapshot();
        assert_eq!(snapshot.http_attempts, 2);
        assert_eq!(snapshot.retries, 1);
        assert_eq!(snapshot.failures, 1);
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
            r#"{"success":true,"block":{"tx_id":"ab","version":3,"height":7,"is_voided":false}}"#,
        )
        .unwrap();
        assert!(present.success && present.block.is_some());

        let absent: BlockResponse = serde_json::from_str(r#"{"success":false}"#).unwrap();
        assert!(!absent.success && absent.block.is_none());
    }
}
