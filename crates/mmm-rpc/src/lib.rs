//! Shared HTTP transport and envelope policy for the reqwest-based JSON-RPC clients.
//!
//! The Namecoin, RSK, and Syscoin clients all talk to remote nodes over HTTP,
//! frequently across idle/contended SSH tunnels. A bare `reqwest::Client::new()`
//! has no request timeout and uses default idle keep-alive pooling, so an idle
//! pooled socket can go stale; the next request then hangs (no timeout) or
//! errors on the dead socket, which stalls the live poller's `chain_tip` fetch
//! every tick while a fresh `curl` returns instantly.
//!
//! [`build_rpc_client`] centralizes the transport fix so all clients share one
//! tested HTTP policy:
//!
//! - an explicit whole-request `timeout` (operator-configurable per chain),
//! - an explicit `connect_timeout` (capped at the request timeout),
//! - a short `pool_idle_timeout` so a stale pooled socket is dropped before the
//!   next long-interval live tick reuses it, while still letting a tight
//!   backfill loop reuse the connection across its sub-second call gaps, and
//! - TCP keepalive so a half-open connection used mid-request is detected.
//!
//! [`RpcMetrics`] is the companion counter handle: every chain client owns
//! one and threads it through its calls (directly, for clients with their own
//! retry loop, or via [`post_json_rpc_call`] and
//! [`post_required_json_rpc_call`] for clients without one) so attempts,
//! retries, failures, and latency are visible without changing any
//! retry/timeout/ordering behavior.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use reqwest::{Client, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

mod metrics;
pub use metrics::{AttemptTimer, RpcMetrics, RpcMetricsDelta, RpcMetricsSnapshot};

/// Default whole-request timeout when `<PREFIX>_RPC_TIMEOUT_SECS` is unset.
pub const DEFAULT_RPC_TIMEOUT_SECS: u64 = 15;

/// Upper bound on the connect (TCP/TLS) phase. The effective connect timeout is
/// `min(request_timeout, DEFAULT_CONNECT_TIMEOUT)` so lowering the request
/// timeout below this never leaves a connect budget larger than the whole
/// request budget.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// How long an idle pooled connection may live before it is dropped. Kept well
/// below the 30s default live poll interval so each live tick opens a fresh
/// socket (never reusing a stale tunnel connection), but above the sub-second
/// gaps inside a backfill loop so historical replays keep connection reuse.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(5);

/// TCP keepalive probe interval, so the OS detects a half-open connection that
/// is being used mid-request.
const TCP_KEEPALIVE: Duration = Duration::from_secs(60);

/// Build a reqwest client with the shared RPC transport policy described in the
/// module docs. `request_timeout` is the whole-request deadline; the connect
/// timeout is derived as `min(request_timeout, DEFAULT_CONNECT_TIMEOUT)`.
pub fn build_rpc_client(request_timeout: Duration) -> Result<Client> {
    let connect_timeout = request_timeout.min(DEFAULT_CONNECT_TIMEOUT);
    Client::builder()
        .timeout(request_timeout)
        .connect_timeout(connect_timeout)
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .tcp_keepalive(TCP_KEEPALIVE)
        .build()
        .context("build RPC HTTP client")
}

/// Send one JSON-RPC HTTP POST, attaching basic auth only when both credentials
/// are present. Status interpretation is deliberately left to the caller so
/// public-endpoint clients can classify 429/5xx as retryable.
pub async fn post_json_rpc(
    http: &Client,
    url: &str,
    user: Option<&str>,
    password: Option<&str>,
    label: &str,
    method: &str,
    request: &Value,
) -> Result<Response> {
    let mut builder = http.post(url).json(request);
    if let (Some(user), Some(password)) = (user, password) {
        builder = builder.basic_auth(user, Some(password));
    }

    builder
        .send()
        .await
        .with_context(|| format!("send {label} RPC method {method}"))
}

/// The HTTP client plus its [`RpcMetrics`] handle, bundled so
/// [`post_json_rpc_call`] stays under clippy's argument-count lint.
pub struct RpcTransport<'a> {
    pub http: &'a Client,
    pub metrics: &'a RpcMetrics,
}

/// One JSON-RPC call whose `result` may be JSON `null`: send the POST, require
/// a successful HTTP status, read the whole body, and interpret the envelope.
/// This is the metered boundary for clients with no retry loop of their own
/// (the bitcoind-family and RSK clients): the attempt is timed from dispatch
/// to the consumed body, since a block download is mostly body, and recorded
/// on `transport.metrics` whatever the result; any error, a transport
/// failure, a bad status, an undecodable body, or an RPC error object, is one
/// failed call, terminal here because the caller does not retry. Clients that
/// classify failures as retryable and drive their own retry loop (Elastos,
/// Hathor) instrument themselves around the bare [`post_json_rpc`] instead,
/// so they can call [`RpcMetrics::record_retry`] and decide when the call has
/// failed.
pub async fn post_json_rpc_call<T>(
    transport: &RpcTransport<'_>,
    url: &str,
    user: Option<&str>,
    password: Option<&str>,
    label: &str,
    method: &str,
    request: &Value,
) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    metered(transport.metrics, async {
        let envelope =
            post_json_rpc_envelope(transport.http, url, user, password, label, method, request)
                .await?;
        decode_json_rpc_envelope(label, method, &envelope)
    })
    .await
}

/// [`post_json_rpc_call`] for a method whose result must be present and
/// non-null; a `null` result is a failed call too.
pub async fn post_required_json_rpc_call<T>(
    transport: &RpcTransport<'_>,
    url: &str,
    user: Option<&str>,
    password: Option<&str>,
    label: &str,
    method: &str,
    request: &Value,
) -> Result<T>
where
    T: DeserializeOwned,
{
    metered(transport.metrics, async {
        let envelope =
            post_json_rpc_envelope(transport.http, url, user, password, label, method, request)
                .await?;
        decode_required_json_rpc_envelope(label, method, &envelope)
    })
    .await
}

/// Time one call through its consumed and interpreted response as one
/// attempt, recorded even if the caller abandons the call mid-flight, and
/// count an error as one failed call.
async fn metered<T>(metrics: &RpcMetrics, call: impl Future<Output = Result<T>>) -> Result<T> {
    let _attempt = metrics.attempt(1);
    let result = call.await;
    if result.is_err() {
        metrics.record_failure();
    }
    result
}

async fn post_json_rpc_envelope(
    http: &Client,
    url: &str,
    user: Option<&str>,
    password: Option<&str>,
    label: &str,
    method: &str,
    request: &Value,
) -> Result<Value> {
    let response = post_json_rpc(http, url, user, password, label, method, request)
        .await?
        .error_for_status()
        .with_context(|| format!("{label} RPC HTTP status for method {method}"))?;
    read_json_rpc_envelope(label, method, response).await
}

/// Read a JSON-RPC response body as its envelope.
pub async fn read_json_rpc_envelope(
    label: &str,
    method: &str,
    response: Response,
) -> Result<Value> {
    response
        .json()
        .await
        .with_context(|| format!("decode {label} RPC response for method {method}"))
}

/// Decode a JSON-RPC response whose result must be present and non-null.
pub async fn decode_required_json_rpc_response<T>(
    label: &str,
    method: &str,
    response: Response,
) -> Result<T>
where
    T: DeserializeOwned,
{
    let envelope = read_json_rpc_envelope(label, method, response).await?;
    decode_required_json_rpc_envelope(label, method, &envelope)
}

/// Interpret a JSON-RPC envelope whose result must be present and non-null.
pub fn decode_required_json_rpc_envelope<T>(
    label: &str,
    method: &str,
    envelope: &Value,
) -> Result<T>
where
    T: DeserializeOwned,
{
    decode_json_rpc_envelope(label, method, envelope)?
        .ok_or_else(|| anyhow::anyhow!("{label} RPC method {method} returned no result"))
}

/// Interpret a JSON-RPC envelope into a typed result, explicit null, or error.
///
/// A response missing both `result` and `error` is treated as a protocol
/// violation rather than silently becoming absence.
pub fn decode_json_rpc_envelope<T>(label: &str, method: &str, envelope: &Value) -> Result<Option<T>>
where
    T: DeserializeOwned,
{
    if let Some(error_value) = envelope.get("error").filter(|value| !value.is_null()) {
        let error: JsonRpcError = serde_json::from_value(error_value.clone())
            .with_context(|| format!("decode {label} RPC error object for method {method}"))?;
        bail!(
            "{label} RPC method {method} failed: code={} message={}",
            error.code,
            error.message
        );
    }

    let result = envelope.get("result").ok_or_else(|| {
        anyhow::anyhow!(
            "{label} RPC method {method} response has neither `result` nor `error` field"
        )
    })?;

    if result.is_null() {
        return Ok(None);
    }

    let value: T = serde_json::from_value(result.clone())
        .with_context(|| format!("decode {label} RPC result for method {method}"))?;
    Ok(Some(value))
}

/// JSON-RPC `error` object, surfaced verbatim in the caller's error context.
#[derive(Debug, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

/// Pure timeout parser driven by an arbitrary lookup. Returns the default when
/// the key is unset; otherwise parses the value as `u64` seconds and requires it
/// to be `> 0` (a zero/blank timeout would mean "no timeout", which defeats the
/// purpose of setting one). Callers that read the process environment pass
/// `|key| std::env::var(key).ok()`; unit tests drive a map so they never mutate
/// the global (and in Rust 2024 `unsafe`) process environment.
pub fn parse_timeout_secs_from_lookup<F>(
    name: &str,
    default_secs: u64,
    lookup: F,
) -> Result<Duration>
where
    F: Fn(&str) -> Option<String>,
{
    let secs = match lookup(name) {
        Some(value) => value
            .parse::<u64>()
            .with_context(|| format!("{name} has invalid value {value:?}"))?,
        None => default_secs,
    };
    ensure!(secs > 0, "{name} must be positive");
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn lookup_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn timeout_defaults_when_unset() -> Result<()> {
        let map: HashMap<String, String> = HashMap::new();
        let timeout = parse_timeout_secs_from_lookup(
            "NAMECOIN_RPC_TIMEOUT_SECS",
            DEFAULT_RPC_TIMEOUT_SECS,
            |k| map.get(k).cloned(),
        )?;
        assert_eq!(timeout, Duration::from_secs(DEFAULT_RPC_TIMEOUT_SECS));
        Ok(())
    }

    #[test]
    fn timeout_reads_override() -> Result<()> {
        let map = lookup_from(&[("RSK_RPC_TIMEOUT_SECS", "42")]);
        let timeout = parse_timeout_secs_from_lookup(
            "RSK_RPC_TIMEOUT_SECS",
            DEFAULT_RPC_TIMEOUT_SECS,
            |k| map.get(k).cloned(),
        )?;
        assert_eq!(timeout, Duration::from_secs(42));
        Ok(())
    }

    #[test]
    fn timeout_rejects_zero() {
        let map = lookup_from(&[("SYSCOIN_RPC_TIMEOUT_SECS", "0")]);
        let err = parse_timeout_secs_from_lookup(
            "SYSCOIN_RPC_TIMEOUT_SECS",
            DEFAULT_RPC_TIMEOUT_SECS,
            |k| map.get(k).cloned(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("must be positive"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn timeout_rejects_non_numeric() {
        let map = lookup_from(&[("NAMECOIN_RPC_TIMEOUT_SECS", "nope")]);
        let err = parse_timeout_secs_from_lookup(
            "NAMECOIN_RPC_TIMEOUT_SECS",
            DEFAULT_RPC_TIMEOUT_SECS,
            |k| map.get(k).cloned(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("invalid value"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn build_rpc_client_succeeds_for_normal_timeout() {
        assert!(build_rpc_client(Duration::from_secs(15)).is_ok());
    }

    #[test]
    fn build_rpc_client_succeeds_for_short_timeout() {
        // A request timeout below DEFAULT_CONNECT_TIMEOUT still builds; the
        // connect timeout is clamped down to the request timeout.
        assert!(build_rpc_client(Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn envelope_interprets_null_value_missing_and_error() {
        let envelope: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"x","result":null}"#).unwrap();
        let parsed =
            decode_json_rpc_envelope::<String>("RSK", "eth_getBlockByNumber", &envelope).unwrap();
        assert_eq!(parsed, None);

        let envelope: Value =
            serde_json::from_str(r#"{"jsonrpc":"2.0","id":"x","result":"0x222df"}"#).unwrap();
        let parsed =
            decode_json_rpc_envelope::<String>("RSK", "eth_blockNumber", &envelope).unwrap();
        assert_eq!(parsed.as_deref(), Some("0x222df"));

        let envelope: Value = serde_json::from_str(r#"{"jsonrpc":"2.0","id":"x"}"#).unwrap();
        let err = decode_json_rpc_envelope::<String>("RSK", "eth_getBlockByNumber", &envelope)
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("neither `result` nor `error`"),
            "unexpected error: {msg}"
        );

        let envelope: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":"x","error":{"code":-32000,"message":"oops"}}"#,
        )
        .unwrap();
        let err = decode_json_rpc_envelope::<String>("RSK", "eth_getBlockByNumber", &envelope)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("-32000"), "unexpected error: {msg}");
        assert!(msg.contains("oops"), "unexpected error: {msg}");
    }
}
