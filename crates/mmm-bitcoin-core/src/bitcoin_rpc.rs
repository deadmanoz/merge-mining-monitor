//! Thin Bitcoin Core RPC client.
//!
//! `corepc-client` is synchronous, so this module centralizes the async
//! boundary used by capture and read-model callers: env configuration, optional
//! auth, timeout validation, concurrency limiting, and blocking-task dispatch.

use std::env;
use std::fmt;
use std::io;
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
use tokio::time::{sleep, timeout};
use tracing::warn;

const DEFAULT_TIMEOUT_SECS: u64 = 30;
// Verified against jsonrpc 0.18.0 src/http/minreq_http.rs
// DEFAULT_TIMEOUT_SECONDS. Revisit this constant when corepc-client/jsonrpc is
// upgraded or when corepc-client exposes a timeout constructor.
const COREPC_MINREQ_HTTP_TIMEOUT_SECS: u64 = 15;
const DEFAULT_MAX_CONCURRENCY: usize = 4;
const RPC_MAX_ATTEMPTS: usize = 5;
const RPC_RETRY_BASE_DELAY: Duration = Duration::from_millis(250);
const RPC_RETRY_MAX_DELAY: Duration = Duration::from_secs(4);

#[derive(Debug)]
struct RpcCallTimeout;

impl fmt::Display for RpcCallTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Bitcoin Core RPC timed out")
    }
}

impl std::error::Error for RpcCallTimeout {}

#[derive(Clone)]
pub struct BitcoinCoreRpcClient {
    client: Arc<CoreClient>,
    semaphore: Arc<Semaphore>,
    max_concurrency: usize,
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
            max_concurrency,
            timeout: Duration::from_secs(timeout_secs),
        })
    }

    pub(crate) fn max_concurrency(&self) -> usize {
        self.max_concurrency
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
        F: Fn() -> Result<T> + Send + Sync + 'static,
    {
        self.rpc_call_with_policy(f, RPC_MAX_ATTEMPTS, RPC_RETRY_BASE_DELAY)
            .await
    }

    async fn rpc_call_with_policy<T, F>(
        &self,
        f: F,
        max_attempts: usize,
        retry_base_delay: Duration,
    ) -> Result<T>
    where
        T: Send + 'static,
        F: Fn() -> Result<T> + Send + Sync + 'static,
    {
        assert!(max_attempts > 0, "RPC retry policy requires an attempt");
        let f = Arc::new(f);
        for attempt in 1..=max_attempts {
            let result = match timeout(self.timeout, async {
                let permit = Arc::clone(&self.semaphore)
                    .acquire_owned()
                    .await
                    .context("acquire Bitcoin Core RPC semaphore")?;
                let call = Arc::clone(&f);
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    call()
                })
                .await
                .context("Bitcoin Core RPC blocking task panicked")?
            })
            .await
            {
                Ok(result) => result,
                // Dropping the future detaches a started blocking call. Its
                // owned permit remains held until minreq's shorter timeout
                // returns, while later attempts still have a bounded deadline
                // for reacquiring a slot.
                Err(_) => Err(anyhow::Error::new(RpcCallTimeout)),
            };

            match result {
                Ok(value) => return Ok(value),
                Err(err) if is_transient_rpc_error(&err) && attempt < max_attempts => {
                    let delay = rpc_retry_delay(attempt, retry_base_delay);
                    warn!(attempt, max_attempts, ?delay, error = %err, "retrying transient Bitcoin Core RPC failure");
                    sleep(delay).await;
                }
                Err(err) if is_transient_rpc_error(&err) => {
                    return Err(err).with_context(|| {
                        format!("Bitcoin Core RPC failed after {attempt} attempts")
                    });
                }
                Err(err) => return Err(err),
            }
        }
        unreachable!("positive RPC attempt count must return from the loop")
    }
}

fn rpc_retry_delay(attempt: usize, base: Duration) -> Duration {
    let exponent = u32::try_from(attempt.saturating_sub(1)).unwrap_or(u32::MAX);
    let multiplier = 2_u32.checked_pow(exponent).unwrap_or(u32::MAX);
    base.saturating_mul(multiplier).min(RPC_RETRY_MAX_DELAY)
}

fn is_transient_rpc_error(err: &anyhow::Error) -> bool {
    if err.downcast_ref::<RpcCallTimeout>().is_some() {
        return true;
    }
    match err.downcast_ref::<CoreError>() {
        Some(CoreError::Io(err)) => is_transient_io_error(err),
        Some(CoreError::JsonRpc(jsonrpc::error::Error::Rpc(err))) => err.code == -28,
        Some(CoreError::JsonRpc(jsonrpc::error::Error::Transport(transport))) => transport
            .downcast_ref::<jsonrpc::minreq_http::Error>()
            .is_some_and(|err| match err {
                jsonrpc::minreq_http::Error::Minreq(err) => std::error::Error::source(err)
                    .and_then(|source| source.downcast_ref::<io::Error>())
                    .is_some_and(is_transient_io_error),
                jsonrpc::minreq_http::Error::Http(err) => {
                    matches!(err.status_code, 408 | 425 | 429 | 500..=599)
                }
                _ => false,
            }),
        _ => false,
    }
}

fn is_transient_io_error(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::HostUnreachable
            | io::ErrorKind::NetworkDown
            | io::ErrorKind::NetworkUnreachable
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::UnexpectedEof
            | io::ErrorKind::WouldBlock
    )
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

pub(crate) fn is_block_height_out_of_range(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CoreError>()
        .and_then(core_rpc_error_code)
        .is_some_and(|code| code == -8)
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_client() -> BitcoinCoreRpcClient {
        BitcoinCoreRpcClient {
            client: Arc::new(CoreClient::new("http://127.0.0.1:1")),
            semaphore: Arc::new(Semaphore::new(1)),
            max_concurrency: 1,
            timeout: Duration::from_secs(1),
        }
    }

    fn transient_transport_error() -> anyhow::Error {
        anyhow::Error::new(CoreError::JsonRpc(jsonrpc::error::Error::Transport(
            Box::new(jsonrpc::minreq_http::Error::Http(
                jsonrpc::minreq_http::HttpError {
                    status_code: 503,
                    body: "test unavailable".to_owned(),
                },
            )),
        )))
    }

    fn unknown_transport_error() -> anyhow::Error {
        anyhow::Error::new(CoreError::JsonRpc(jsonrpc::error::Error::Transport(
            Box::new(io::Error::new(io::ErrorKind::TimedOut, "test timeout")),
        )))
    }

    fn warmup_rpc_error() -> anyhow::Error {
        anyhow::Error::new(CoreError::JsonRpc(jsonrpc::error::Error::Rpc(
            jsonrpc::error::RpcError {
                code: -28,
                message: "Loading block index...".to_owned(),
                data: None,
            },
        )))
    }

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

        let height_out_of_range = anyhow::Error::new(CoreError::JsonRpc(
            jsonrpc::error::Error::Rpc(jsonrpc::error::RpcError {
                code: -8,
                message: "Block height out of range".to_owned(),
                data: None,
            }),
        ));
        assert!(is_block_height_out_of_range(&height_out_of_range));
        assert!(!is_not_found(&height_out_of_range));
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
    fn unreachable_network_io_failures_are_transient() {
        for kind in [
            io::ErrorKind::HostUnreachable,
            io::ErrorKind::NetworkDown,
            io::ErrorKind::NetworkUnreachable,
        ] {
            assert!(is_transient_io_error(&io::Error::from(kind)), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn retries_transient_transport_failures_until_success() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let value = test_client()
            .rpc_call_with_policy(
                move || {
                    if observed.fetch_add(1, Ordering::SeqCst) < 2 {
                        Err(transient_transport_error())
                    } else {
                        Ok(42_u8)
                    }
                },
                3,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn retries_bitcoin_core_warmup_until_success() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let value = test_client()
            .rpc_call_with_policy(
                move || {
                    if observed.fetch_add(1, Ordering::SeqCst) < 2 {
                        Err(warmup_rpc_error())
                    } else {
                        Ok(42_u8)
                    }
                },
                3,
                Duration::ZERO,
            )
            .await
            .unwrap();
        assert_eq!(value, 42);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn reports_transient_failure_after_bounded_attempts() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let error = test_client()
            .rpc_call_with_policy(
                move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(transient_transport_error())
                },
                3,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("failed after 3 attempts"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn bounds_permit_reacquisition_after_outer_timeout() {
        let mut client = test_client();
        client.timeout = Duration::from_millis(1);
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let error = timeout(
            Duration::from_millis(100),
            client.rpc_call_with_policy(
                move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(200));
                    Ok(())
                },
                2,
                Duration::ZERO,
            ),
        )
        .await
        .expect("retry policy must not wait indefinitely for a retained permit")
        .unwrap_err();
        assert!(error.to_string().contains("failed after 2 attempts"));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_retry_rpc_failures() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        let error = test_client()
            .rpc_call_with_policy(
                move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(test_not_found_error())
                },
                5,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
        assert!(is_not_found(&error));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn does_not_retry_unknown_transport_failures() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&attempts);
        test_client()
            .rpc_call_with_policy(
                move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(unknown_transport_error())
                },
                5,
                Duration::ZERO,
            )
            .await
            .unwrap_err();
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
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
