//! Shared thin JSON-RPC 1.0 client for bitcoind-family chains (Namecoin, Syscoin,
//! Fractal, Qbit, Terracoin).

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use bitcoin::BlockHash;
use reqwest::Client;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use mmm_rpc as rpc_http;
use rpc_http::{RpcMetrics, build_rpc_client};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoindRpcConfig {
    pub url: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub request_timeout: Duration,
    /// Older daemons require boolean `false` rather than numeric `0` for `getblock`.
    pub raw_block_boolean_verbose: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[tokio::test]
    async fn raw_block_rpc_preserves_numeric_and_boolean_wire_contracts() -> Result<()> {
        for boolean in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let url = format!("http://{}", listener.local_addr()?);
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut data = Vec::new();
                let request = loop {
                    let mut buf = [0; 4096];
                    let n = stream.read(&mut buf).unwrap();
                    assert!(n > 0);
                    data.extend_from_slice(&buf[..n]);
                    if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n")
                        && let Ok(value) = serde_json::from_slice::<Value>(&data[end + 4..])
                    {
                        break value;
                    }
                };
                let body = r#"{"result":"00","error":null,"id":"getblock"}"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
                request
            });
            let client = BitcoindRpcClient::new(
                "fixture",
                BitcoindRpcConfig {
                    url,
                    user: Some("fixture".into()),
                    password: Some("fixture".into()),
                    request_timeout: Duration::from_secs(5),
                    raw_block_boolean_verbose: boolean,
                },
            )?;
            let hash =
                "ca98b313dac20b3d6d6d3c61d035610d0d7aca66cd5070812021d824d08199d3".parse()?;
            assert_eq!(client.get_block_raw(&hash).await?, vec![0]);
            let request = server.join().unwrap();
            assert_eq!(request["method"], "getblock");
            assert_eq!(
                request["params"],
                json!([
                    hash.to_string(),
                    if boolean { json!(false) } else { json!(0) }
                ])
            );
        }
        Ok(())
    }
}

#[allow(async_fn_in_trait)]
pub trait BitcoindRpc {
    async fn get_block_count(&self) -> Result<i32>;
    async fn get_block_hash(&self, height: i32) -> Result<BlockHash>;
    async fn get_block_raw(&self, hash: &BlockHash) -> Result<Vec<u8>>;
    async fn get_header_with_auxpow(&self, hash: &BlockHash) -> Result<Vec<u8>>;
}

#[derive(Debug, Clone)]
pub struct BitcoindRpcClient {
    label: &'static str,
    config: BitcoindRpcConfig,
    http: Client,
    metrics: RpcMetrics,
}

impl BitcoindRpcClient {
    pub fn new(label: &'static str, config: BitcoindRpcConfig) -> Result<Self> {
        let http = build_rpc_client(config.request_timeout)?;
        Ok(Self {
            label,
            config,
            http,
            metrics: RpcMetrics::new(label),
        })
    }

    pub fn metrics(&self) -> RpcMetrics {
        self.metrics.clone()
    }

    async fn call<T>(&self, method: &str, params: Vec<Value>) -> Result<T>
    where
        T: DeserializeOwned,
    {
        let label = self.label;
        let request = json!({"jsonrpc": "1.0", "id": method, "method": method, "params": params});
        let transport = rpc_http::RpcTransport {
            http: &self.http,
            metrics: &self.metrics,
        };
        rpc_http::post_required_json_rpc_call(
            &transport,
            &self.config.url,
            self.config.user.as_deref(),
            self.config.password.as_deref(),
            label,
            method,
            &request,
        )
        .await
    }
}

impl BitcoindRpc for BitcoindRpcClient {
    async fn get_block_count(&self) -> Result<i32> {
        self.call("getblockcount", vec![]).await
    }

    async fn get_block_hash(&self, height: i32) -> Result<BlockHash> {
        let hash: String = self.call("getblockhash", vec![json!(height)]).await?;
        BlockHash::from_str(&hash).with_context(|| format!("parse {} block hash", self.label))
    }

    async fn get_block_raw(&self, hash: &BlockHash) -> Result<Vec<u8>> {
        let raw_hex: String = self
            .call(
                "getblock",
                vec![
                    json!(hash.to_string()),
                    if self.config.raw_block_boolean_verbose {
                        json!(false)
                    } else {
                        json!(0)
                    },
                ],
            )
            .await?;
        hex::decode(&raw_hex).with_context(|| format!("decode {} raw block hex", self.label))
    }

    /// Fetch `[child header][CAuxPow]` via `getblockheader <hash> false true` (Fractal).
    async fn get_header_with_auxpow(&self, hash: &BlockHash) -> Result<Vec<u8>> {
        let raw_hex: String = self
            .call(
                "getblockheader",
                vec![json!(hash.to_string()), json!(false), json!(true)],
            )
            .await?;
        hex::decode(&raw_hex)
            .with_context(|| format!("decode {} getblockheader-auxpow hex", self.label))
    }
}
