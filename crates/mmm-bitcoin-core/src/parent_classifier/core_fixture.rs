//! A scripted, in-process Bitcoin Core RPC fixture for exercising the
//! PRODUCTION `ConfiguredParentClassifier` end to end, without a real node.
//!
//! Answers exactly the five JSON-RPC methods `BitcoinCoreRpcClient` calls
//! (`getblockchaininfo`, `getblockcount`, `getblockhash`, `getblockheader`
//! verbose and non-verbose, `getblock` verbosity 0) from a canned, linked
//! header chain, over a plain blocking `std::net::TcpListener` (the same
//! style `chains::hathor::rpc`'s tests use): `BitcoinCoreRpcClient::rpc_call`
//! already dispatches every call through `spawn_blocking`, so a synchronous
//! transport is the accurate fixture for it, not an async server. Counts every
//! request it answers, per method, so a test can compare the server's own
//! counters against the client-side `RpcMetrics` the same classification run
//! produced.
//!
//! Test-only (unlike `fake.rs`, nothing outside this crate's own test module
//! constructs one, so this stays behind `#[cfg(test)]` rather than also being
//! reachable under the `db-integration` feature).

#![cfg(test)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bitcoin::block::{Header, Version};
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use bitcoin::{
    Amount, Block, BlockHash, CompactTarget, OutPoint, ScriptBuf, Sequence, Transaction, TxIn,
    TxMerkleNode, TxOut, Witness, absolute, transaction,
};
use serde_json::{Value, json};

/// Fixed difficulty bits every canned header shares, so `bits_match_expected`
/// (the inferred-stale same-epoch check) always passes without needing a real
/// difficulty-epoch table.
pub(crate) const FIXTURE_BITS: u32 = 0x207f_ffff;

/// Build one canned header. `nonce` is the caller's responsibility for
/// uniqueness (mirrors `tests/support/seed.rs::test_header_chain`'s
/// `height + 1` convention); a fixture chain never needs real proof of work.
pub(crate) fn header(prev: BlockHash, time: u32, nonce: u32) -> Header {
    Header {
        version: Version::ONE,
        prev_blockhash: prev,
        merkle_root: TxMerkleNode::all_zeros(),
        time,
        bits: CompactTarget::from_consensus(FIXTURE_BITS),
        nonce,
    }
}

fn coinbase_tx(height: i32) -> Transaction {
    Transaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(height.to_le_bytes().to_vec()),
            sequence: Sequence::MAX,
            witness: Witness::default(),
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new(),
        }],
    }
}

fn raw_block_for(header: Header, height: i32) -> Vec<u8> {
    serialize(&Block {
        header,
        txdata: vec![coinbase_tx(height)],
    })
}

/// One block the fixture can answer `getblockheader`/`getblock` for, whether
/// or not it is on the main chain.
struct FixtureBlock {
    header: Header,
    height: i32,
    /// Bitcoin Core's `confirmations`: `>= 0` for a main-chain block, `-1` for
    /// a known-but-not-main (stale) block.
    confirmations: i64,
    /// Full consensus-serialized block (one synthetic coinbase tx), answering
    /// `getblock <hash> 0`.
    raw_block: Vec<u8>,
}

/// The canned chain state: every block the fixture knows about
/// (`by_hash`, answers `getblockheader`/`getblock` regardless of chain
/// membership) and which heights are on the main chain (`canonical_by_height`,
/// answers `getblockhash` and gates a positive `confirmations`).
pub(crate) struct FixtureChain {
    by_hash: HashMap<BlockHash, FixtureBlock>,
    canonical_by_height: HashMap<i32, BlockHash>,
    tip_height: i32,
}

impl FixtureChain {
    /// A linked canonical chain of `tip_height + 1` headers at heights
    /// `0..=tip_height`, each registered as main-chain. Returns the chain plus
    /// the headers themselves (indexed by height) so a test can link
    /// scenario-specific candidates onto it.
    pub(crate) fn mainline(tip_height: i32, base_time: u32) -> (Self, Vec<Header>) {
        let mut by_hash = HashMap::new();
        let mut canonical_by_height = HashMap::new();
        let mut headers = Vec::new();
        let mut prev = BlockHash::all_zeros();
        for height in 0..=tip_height {
            #[allow(clippy::cast_sign_loss, reason = "height is always non-negative here")]
            let h = header(prev, base_time + height as u32, height as u32 + 1);
            let hash = h.block_hash();
            by_hash.insert(
                hash,
                FixtureBlock {
                    header: h,
                    height,
                    confirmations: i64::from(tip_height - height + 1),
                    raw_block: raw_block_for(h, height),
                },
            );
            canonical_by_height.insert(height, hash);
            headers.push(h);
            prev = hash;
        }
        (
            Self {
                by_hash,
                canonical_by_height,
                tip_height,
            },
            headers,
        )
    }

    /// Register an additional block Core has seen but does NOT consider
    /// main-chain (`confirmations == -1`): a losing sibling at `height`.
    pub(crate) fn insert_stale(&mut self, height: i32, header: Header) {
        self.by_hash.insert(
            header.block_hash(),
            FixtureBlock {
                header,
                height,
                confirmations: -1,
                raw_block: raw_block_for(header, height),
            },
        );
    }

    fn getblockchaininfo(&self) -> String {
        json!({
            "chain": "main",
            "blocks": self.tip_height,
            "headers": self.tip_height,
            "bestblockhash": self.canonical_by_height[&self.tip_height].to_string(),
            "difficulty": 1.0,
            "mediantime": i64::from(self.tip_height),
            "verificationprogress": 1.0,
            "initialblockdownload": false,
            "chainwork": "00",
            "size_on_disk": 0,
            "pruned": false,
            "warnings": [],
        })
        .to_string()
    }

    fn getblockcount(&self) -> String {
        self.tip_height.to_string()
    }

    fn getblockhash(&self, params: &[Value]) -> Result<String, RpcFault> {
        let height = params
            .first()
            .and_then(Value::as_i64)
            .and_then(|h| i32::try_from(h).ok())
            .ok_or(BAD_PARAMS)?;
        let hash = self
            .canonical_by_height
            .get(&height)
            .ok_or(HEIGHT_OUT_OF_RANGE)?;
        Ok(json_string(hash.to_string()))
    }

    fn getblockheader(&self, params: &[Value]) -> Result<String, RpcFault> {
        let hash = parse_hash(params.first())?;
        let verbose = params.get(1).and_then(Value::as_bool).unwrap_or(true);
        let block = self.by_hash.get(&hash).ok_or(NOT_FOUND)?;
        if verbose {
            Ok(json!({
                "hash": hash.to_string(),
                "confirmations": block.confirmations,
                "height": block.height,
                "version": 1,
                "versionHex": "00000001",
                "merkleroot": block.header.merkle_root.to_string(),
                "time": block.header.time,
                "mediantime": block.header.time,
                "nonce": block.header.nonce,
                "bits": format!("{:08x}", block.header.bits.to_consensus()),
                "difficulty": 1.0,
                "chainwork": "00",
                "nTx": 1,
                "previousblockhash": block.header.prev_blockhash.to_string(),
            })
            .to_string())
        } else {
            Ok(json_string(hex::encode(serialize(&block.header))))
        }
    }

    fn getblock(&self, params: &[Value]) -> Result<String, RpcFault> {
        let hash = parse_hash(params.first())?;
        let block = self.by_hash.get(&hash).ok_or(NOT_FOUND)?;
        Ok(json_string(hex::encode(&block.raw_block)))
    }

    fn dispatch(&self, method: &str, params: &[Value]) -> Result<String, RpcFault> {
        match method {
            "getblockchaininfo" => Ok(self.getblockchaininfo()),
            "getblockcount" => Ok(self.getblockcount()),
            "getblockhash" => self.getblockhash(params),
            "getblockheader" => self.getblockheader(params),
            "getblock" => self.getblock(params),
            _unknown => Err(METHOD_NOT_FOUND),
        }
    }
}

fn parse_hash(value: Option<&Value>) -> Result<BlockHash, RpcFault> {
    value
        .and_then(Value::as_str)
        .and_then(|s| s.parse().ok())
        .ok_or(BAD_PARAMS)
}

fn json_string(s: String) -> String {
    serde_json::to_string(&s).expect("string always serializes")
}

/// A JSON-RPC error, mirroring real Bitcoin Core's error codes the classifier
/// already special-cases (`is_not_found`, `is_block_height_out_of_range`).
struct RpcFault {
    code: i32,
    message: &'static str,
}

const NOT_FOUND: RpcFault = RpcFault {
    code: -5,
    message: "Block not found",
};
const HEIGHT_OUT_OF_RANGE: RpcFault = RpcFault {
    code: -8,
    message: "Block height out of range",
};
const BAD_PARAMS: RpcFault = RpcFault {
    code: -1,
    message: "invalid parameters",
};
const METHOD_NOT_FOUND: RpcFault = RpcFault {
    code: -32601,
    message: "Method not found",
};

/// Per-JSON-RPC-method request counters the fixture increments as it answers
/// each call, so a test can compare them against the client's own
/// `RpcMetrics` for the same run.
#[derive(Default)]
pub(crate) struct FixtureRequestCounts {
    getblockchaininfo: AtomicU64,
    getblockcount: AtomicU64,
    getblockhash: AtomicU64,
    getblockheader: AtomicU64,
    getblock: AtomicU64,
}

impl FixtureRequestCounts {
    fn record(&self, method: &str) {
        let counter = match method {
            "getblockchaininfo" => &self.getblockchaininfo,
            "getblockcount" => &self.getblockcount,
            "getblockhash" => &self.getblockhash,
            "getblockheader" => &self.getblockheader,
            "getblock" => &self.getblock,
            _ => return,
        };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    pub(crate) fn getblockchaininfo(&self) -> u64 {
        self.getblockchaininfo.load(Ordering::SeqCst)
    }
    pub(crate) fn getblockcount(&self) -> u64 {
        self.getblockcount.load(Ordering::SeqCst)
    }
    pub(crate) fn getblockhash(&self) -> u64 {
        self.getblockhash.load(Ordering::SeqCst)
    }
    pub(crate) fn getblockheader(&self) -> u64 {
        self.getblockheader.load(Ordering::SeqCst)
    }
    pub(crate) fn getblock(&self) -> u64 {
        self.getblock.load(Ordering::SeqCst)
    }
}

/// A spawned fixture: the address `BitcoinCoreRpcClient::from_env_url` should
/// point at, and the live per-method request counters.
pub(crate) struct SpawnedFixture {
    pub(crate) addr: SocketAddr,
    pub(crate) counts: Arc<FixtureRequestCounts>,
}

/// Spawn the fixture on a background thread bound to an OS-assigned loopback
/// port. Serves up to `max_requests` requests (one per connection, matching
/// the corepc-client `minreq` transport's one-connection-per-call behavior)
/// then stops accepting; a test picks `max_requests` generously so a
/// miscounted expectation fails on an explicit assertion rather than on a
/// dropped connection that would otherwise trigger the classifier's own
/// connection-refused retry/backoff.
pub(crate) fn spawn(chain: FixtureChain, max_requests: usize) -> SpawnedFixture {
    spawn_with_delay(chain, max_requests, Duration::ZERO)
}

/// Inject one response delay per HTTP request for production-RTT measurements.
pub(crate) fn spawn_with_delay(
    chain: FixtureChain,
    max_requests: usize,
    delay: Duration,
) -> SpawnedFixture {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture listener");
    let addr = listener.local_addr().expect("fixture listener local addr");
    let counts = Arc::new(FixtureRequestCounts::default());
    let counts_for_thread = Arc::clone(&counts);
    std::thread::spawn(move || {
        for stream in listener.incoming().take(max_requests) {
            let Ok(mut stream) = stream else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            std::thread::sleep(delay);
            handle_connection(&mut stream, &chain, &counts_for_thread);
        }
    });
    SpawnedFixture { addr, counts }
}

fn handle_connection(stream: &mut TcpStream, chain: &FixtureChain, counts: &FixtureRequestCounts) {
    let Some(body) = read_json_rpc_body(stream) else {
        return;
    };
    let Ok(request) = serde_json::from_slice::<Value>(&body) else {
        return;
    };
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let params = request
        .get("params")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    counts.record(&method);

    let body = match chain.dispatch(&method, &params) {
        Ok(result_json) => format!(r#"{{"result":{result_json},"error":null,"id":1}}"#),
        Err(fault) => format!(
            r#"{{"result":null,"error":{{"code":{},"message":{}}},"id":1}}"#,
            fault.code,
            json_string(fault.message.to_owned())
        ),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes());
}

/// Read one full HTTP request body off `stream`, using `Content-Length` to
/// know when the body is complete (the request bodies here are always small
/// JSON-RPC calls, but a single `read` is not guaranteed to return the whole
/// request over a real socket, so this loops until it has one).
fn read_json_rpc_body(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(header_end) = find_double_crlf(&buf) {
            let content_length = content_length_from_headers(&buf[..header_end]);
            let body_start = header_end + 4;
            if buf.len() >= body_start + content_length {
                return Some(buf[body_start..body_start + content_length].to_vec());
            }
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn content_length_from_headers(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().ok())
                .flatten()
        })
        .unwrap_or(0)
}
