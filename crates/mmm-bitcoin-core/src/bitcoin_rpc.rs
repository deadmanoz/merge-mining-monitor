//! Thin Bitcoin Core RPC client.
//!
//! `corepc-client` is synchronous, so this module centralizes the async
//! boundary used by capture and read-model callers: env configuration, optional
//! auth, timeout validation, concurrency limiting, and blocking-task dispatch.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bitcoin::block::Header;
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use bitcoin::{Block, BlockHash};
use corepc_client::client_sync::v28::Client as CoreClient;
use corepc_client::client_sync::{Auth, Error as CoreError};
use tokio::sync::Semaphore;
use tokio::time::timeout;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
// Verified against jsonrpc 0.18.0 src/http/minreq_http.rs
// DEFAULT_TIMEOUT_SECONDS. Revisit this constant when corepc-client/jsonrpc is
// upgraded or when corepc-client exposes a timeout constructor.
const COREPC_MINREQ_HTTP_TIMEOUT_SECS: u64 = 15;
const DEFAULT_MAX_CONCURRENCY: usize = 4;

#[derive(Clone)]
pub struct BitcoinCoreRpcClient {
    client: Arc<CoreClient>,
    semaphore: Arc<Semaphore>,
    timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BitcoinCoreHeaderStatus {
    pub confirmations: i64,
    pub height: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BitcoinCoreChainStatus {
    pub blocks: i32,
    pub headers: i32,
    pub initial_block_download: bool,
    /// `getblockchaininfo.mediantime`: the median time past of the chain tip. Used
    /// to judge whether a synced tip is actually fresh (a stalled / isolated node
    /// can report `blocks == headers && !IBD` while being far behind the real tip).
    pub median_time: i64,
}

impl BitcoinCoreChainStatus {
    pub(crate) fn is_synced_tip(self) -> bool {
        !self.initial_block_download && self.blocks == self.headers
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitcoinCoreBlockCoinbase {
    pub txid: Vec<u8>,
    pub script: Vec<u8>,
    pub outputs: Vec<u8>,
}

impl BitcoinCoreRpcClient {
    pub fn from_env_url(url: &str) -> Result<Self> {
        let user = env::var("BITCOIN_RPC_USER").ok();
        let password = env::var("BITCOIN_RPC_PASSWORD").ok();
        let auth = auth_from_optional_user_pass(user, password)?;
        let client = match auth {
            Auth::None => CoreClient::new(url),
            auth => {
                CoreClient::new_with_auth(url, auth).context("create Bitcoin Core RPC client")?
            }
        };
        let timeout_secs = parse_env_or("BITCOIN_RPC_TIMEOUT_SECS", DEFAULT_TIMEOUT_SECS)?;
        let max_concurrency = parse_env_or("BITCOIN_RPC_MAX_CONCURRENCY", DEFAULT_MAX_CONCURRENCY)?;
        validate_timeout_secs(timeout_secs)?;
        validate_max_concurrency(max_concurrency)?;
        Ok(Self {
            client: Arc::new(client),
            semaphore: Arc::new(Semaphore::new(max_concurrency)),
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    pub async fn get_block_hash(&self, height: u64) -> Result<BlockHash> {
        let client = Arc::clone(&self.client);
        self.rpc_call(move || Ok(client.get_block_hash(height)?.block_hash()?))
            .await
    }

    pub async fn get_block_count(&self) -> Result<u64> {
        let client = Arc::clone(&self.client);
        self.rpc_call(move || Ok(client.get_block_count()?.0)).await
    }

    pub(crate) async fn get_chain_status(&self) -> Result<BitcoinCoreChainStatus> {
        let client = Arc::clone(&self.client);
        self.rpc_call(move || {
            let info = client.get_blockchain_info()?;
            Ok(BitcoinCoreChainStatus {
                blocks: rpc_height_to_i32(info.blocks)
                    .context("Bitcoin Core blocks overflows i32")?,
                headers: rpc_height_to_i32(info.headers)
                    .context("Bitcoin Core headers overflows i32")?,
                initial_block_download: info.initial_block_download,
                median_time: info.median_time,
            })
        })
        .await
    }

    pub async fn get_block_header(&self, hash: BlockHash) -> Result<Header> {
        let client = Arc::clone(&self.client);
        self.rpc_call(move || Ok(client.get_block_header(&hash)?.block_header()?))
            .await
    }

    pub(crate) async fn get_block_header_verbose(
        &self,
        hash: BlockHash,
    ) -> Result<BitcoinCoreHeaderStatus> {
        let client = Arc::clone(&self.client);
        self.rpc_call(move || {
            let verbose = client.get_block_header_verbose(&hash)?;
            Ok(BitcoinCoreHeaderStatus {
                confirmations: verbose.confirmations,
                height: verbose.height,
            })
        })
        .await
    }

    pub async fn get_block_coinbase(&self, hash: BlockHash) -> Result<BitcoinCoreBlockCoinbase> {
        let client = Arc::clone(&self.client);
        self.rpc_call(move || {
            let block = client.get_block(hash)?;
            coinbase_from_block(&block)
        })
        .await
    }

    async fn rpc_call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T> + Send + 'static,
    {
        let _permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .context("acquire Bitcoin Core RPC semaphore")?;
        let task = tokio::task::spawn_blocking(f);
        timeout(self.timeout, task)
            .await
            .context("Bitcoin Core RPC timed out")?
            .context("Bitcoin Core RPC blocking task panicked")?
    }
}

pub(crate) fn coinbase_from_block(block: &Block) -> Result<BitcoinCoreBlockCoinbase> {
    let tx = block
        .txdata
        .first()
        .context("Bitcoin Core block has no transactions")?;
    if !tx.is_coinbase() {
        bail!("Bitcoin Core block tx0 is not coinbase");
    }
    let input = tx
        .input
        .first()
        .context("Bitcoin Core coinbase transaction has no inputs")?;
    Ok(BitcoinCoreBlockCoinbase {
        txid: tx.compute_txid().to_byte_array().to_vec(),
        script: input.script_sig.as_bytes().to_vec(),
        outputs: serialize(&tx.output),
    })
}

fn auth_from_optional_user_pass(user: Option<String>, password: Option<String>) -> Result<Auth> {
    match (user, password) {
        (Some(user), Some(password)) => {
            if user.trim().is_empty() || password.trim().is_empty() {
                bail!("BITCOIN_RPC_USER and BITCOIN_RPC_PASSWORD must be non-empty when set");
            }
            Ok(Auth::UserPass(user, password))
        }
        (None, None) => Ok(Auth::None),
        _ => bail!("BITCOIN_RPC_USER and BITCOIN_RPC_PASSWORD must be set together"),
    }
}

fn parse_env_or<T>(name: &str, default: T) -> Result<T>
where
    T: std::str::FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match env::var(name) {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{name} has invalid value {value:?}")),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(err) => Err(err).with_context(|| format!("read {name}")),
    }
}

fn validate_timeout_secs(timeout_secs: u64) -> Result<()> {
    if timeout_secs <= COREPC_MINREQ_HTTP_TIMEOUT_SECS {
        bail!(
            "BITCOIN_RPC_TIMEOUT_SECS must be greater than {COREPC_MINREQ_HTTP_TIMEOUT_SECS}; corepc-client 0.8 minreq transport has a {COREPC_MINREQ_HTTP_TIMEOUT_SECS}s HTTP timeout"
        );
    }
    Ok(())
}

fn validate_max_concurrency(max_concurrency: usize) -> Result<()> {
    if max_concurrency == 0 {
        bail!("BITCOIN_RPC_MAX_CONCURRENCY must be positive");
    }
    Ok(())
}

fn rpc_height_to_i32(height: i64) -> std::result::Result<i32, std::num::TryFromIntError> {
    height.try_into()
}

pub(crate) fn is_not_found(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CoreError>()
        .and_then(core_rpc_error_code)
        .is_some_and(|code| code == -5)
}

fn core_rpc_error_code(err: &CoreError) -> Option<i32> {
    match err {
        CoreError::JsonRpc(jsonrpc::error::Error::Rpc(rpc)) => Some(rpc.code),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) fn test_not_found_error() -> anyhow::Error {
    anyhow::Error::new(CoreError::JsonRpc(jsonrpc::error::Error::Rpc(
        jsonrpc::error::RpcError {
            code: -5,
            message: "Block not found".to_owned(),
            data: None,
        },
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_auth_rejects_blank_credentials() {
        assert!(auth_from_optional_user_pass(None, None).is_ok());
        assert!(auth_from_optional_user_pass(Some("user".into()), Some("pass".into())).is_ok());
        assert!(auth_from_optional_user_pass(Some("".into()), Some("pass".into())).is_err());
        assert!(auth_from_optional_user_pass(Some("user".into()), Some(" ".into())).is_err());
        assert!(auth_from_optional_user_pass(Some("user".into()), None).is_err());
    }

    #[test]
    fn recognizes_bitcoin_core_not_found_by_rpc_code() {
        let not_found = test_not_found_error();
        assert!(is_not_found(&not_found));

        let method_missing = anyhow::Error::new(CoreError::JsonRpc(jsonrpc::error::Error::Rpc(
            jsonrpc::error::RpcError {
                code: -32601,
                message: "Block not found".to_owned(),
                data: None,
            },
        )));
        assert!(!is_not_found(&method_missing));
    }

    #[test]
    fn validates_timeout_above_corepc_transport_timeout() {
        assert!(validate_timeout_secs(COREPC_MINREQ_HTTP_TIMEOUT_SECS + 1).is_ok());
        assert!(validate_timeout_secs(COREPC_MINREQ_HTTP_TIMEOUT_SECS).is_err());
        assert!(validate_timeout_secs(0).is_err());
    }

    #[test]
    fn validates_positive_max_concurrency() {
        assert!(validate_max_concurrency(1).is_ok());
        assert!(validate_max_concurrency(0).is_err());
    }

    #[test]
    fn chain_status_sync_requires_blocks_equal_headers_and_not_ibd() {
        assert!(
            BitcoinCoreChainStatus {
                blocks: 953_305,
                headers: 953_305,
                initial_block_download: false,
                median_time: 0,
            }
            .is_synced_tip()
        );
        assert!(
            !BitcoinCoreChainStatus {
                blocks: 953_304,
                headers: 953_305,
                initial_block_download: false,
                median_time: 0,
            }
            .is_synced_tip()
        );
        assert!(
            !BitcoinCoreChainStatus {
                blocks: 953_305,
                headers: 953_305,
                initial_block_download: true,
                median_time: 0,
            }
            .is_synced_tip()
        );
    }

    #[test]
    fn extracts_coinbase_from_genesis_block() {
        let block = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
        let coinbase = coinbase_from_block(&block).unwrap();

        assert_eq!(
            coinbase.txid,
            block.txdata[0].compute_txid().to_byte_array()
        );
        assert_eq!(
            coinbase.script,
            block.txdata[0].input[0].script_sig.as_bytes()
        );
        assert_eq!(coinbase.outputs, serialize(&block.txdata[0].output));
    }
}
