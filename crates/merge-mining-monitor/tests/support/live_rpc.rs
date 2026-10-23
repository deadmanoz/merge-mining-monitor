//! Stateful loopback RPC fixture for the actual producer binaries.

use anyhow::Result;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bitcoin::{block::Header, consensus::serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI32, AtomicU8, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

#[derive(Default)]
pub struct RpcState {
    /// 0 ready, 1 warmup, 2 IBD, 3 header lag, 4 stale, 5 auth, 6 malformed,
    /// 7 blackhole, 8 wrong network, 9 impossible verbose height,
    /// 10 body not downloaded, 11 height race.
    pub mode: AtomicU8,
    pub delay_ms: AtomicU64,
    pub tip: AtomicI32,
    pub requests: Mutex<Vec<(Instant, String, Value)>>,
    headers: Mutex<HashMap<String, Header>>,
}

pub struct RpcFixture {
    pub url: String,
    pub state: Arc<RpcState>,
    task: JoinHandle<()>,
}
impl Drop for RpcFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RpcFixture {
    pub async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://{}", listener.local_addr()?);
        let state = Arc::new(RpcState::default());
        let router = Router::new()
            .route("/", post(core))
            .route("/child", post(child))
            .route("/hathor/block_at_height", get(hathor_hold))
            .with_state(state.clone());
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Ok(Self { url, state, task })
    }

    pub fn count(&self, method: &str) -> usize {
        self.state
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, name, _)| name == method)
            .count()
    }
}

async fn hathor_hold(State(state): State<Arc<RpcState>>) -> Json<Value> {
    state
        .requests
        .lock()
        .unwrap()
        .push((Instant::now(), "hathor_hold".into(), Value::Null));
    Json(json!({"success":false}))
}

async fn child(State(state): State<Arc<RpcState>>, Json(request): Json<Value>) -> Json<Value> {
    state.requests.lock().unwrap().push((
        Instant::now(),
        "child".into(),
        request["params"].clone(),
    ));
    assert_eq!(request["method"], "getblockcount");
    Json(json!({"result":0,"error":null,"id":request["id"]}))
}

async fn core(State(state): State<Arc<RpcState>>, Json(request): Json<Value>) -> Response {
    let method = request["method"].as_str().unwrap().to_owned();
    state.requests.lock().unwrap().push((
        Instant::now(),
        method.clone(),
        request["params"].clone(),
    ));
    let mode = state.mode.load(Ordering::Relaxed);
    if mode == 7 {
        tokio::time::sleep(Duration::from_secs(60)).await;
    }
    tokio::time::sleep(Duration::from_millis(
        state.delay_ms.load(Ordering::Relaxed),
    ))
    .await;
    if mode == 5 {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if mode == 6 {
        return "invalid JSON".into_response();
    }
    if mode == 1 {
        return Json(json!({"result":null,"error":{"code":-28,"message":"fixture warmup"},"id":request["id"]})).into_response();
    }
    if (mode == 10 && method == "getblock") || (mode == 11 && method == "getblockhash") {
        return Json(json!({"result":null,"error":{"code":if mode==10 { -1 } else { -8 },
            "message":if mode==10 { "Block not available (not fully downloaded)" } else { "Block height out of range" }},"id":request["id"]})).into_response();
    }
    let hash = request["params"][0].as_str().unwrap_or_default();
    let genesis_hash = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin)
        .block_hash()
        .to_string();
    if method == "getblockheader"
        && hash != genesis_hash
        && !state.headers.lock().unwrap().contains_key(hash)
    {
        return Json(json!({"result":null,"error":{"code":-5,"message":"Block not found"},"id":request["id"]})).into_response();
    }
    let result = answer(&state, &method, &request["params"], mode);
    Json(json!({"result":result,"error":null,"id":request["id"]})).into_response()
}

fn answer(state: &RpcState, method: &str, params: &Value, mode: u8) -> Value {
    let tip = state.tip.load(Ordering::Relaxed);
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin);
    match method {
        "getblockchaininfo" => json!({
            "chain":if mode==8 { "test" } else { "main" }, "blocks":tip, "headers":tip+i32::from(mode==3),
            "bestblockhash":genesis.block_hash().to_string(), "difficulty":1.0,
            "mediantime":if mode==4 { 1 } else { mmm_capture::capture::now_epoch_seconds().unwrap() },
            "verificationprogress":1.0, "initialblockdownload":mode==2,
            "chainwork":"00", "size_on_disk":0, "pruned":false, "warnings":[],
        }),
        "getblockcount" => json!(tip),
        "getblockhash" => {
            let height = params[0].as_u64().unwrap();
            let mut header = genesis.header;
            if height > 0 {
                header.nonce = u32::try_from(height).unwrap();
                header.time = u32::try_from(1_700_000_000 + height).unwrap();
            }
            let hash = header.block_hash().to_string();
            state.headers.lock().unwrap().insert(hash.clone(), header);
            json!(hash)
        }
        "getblockheader" => {
            let hash = params[0].as_str().unwrap();
            let header = state
                .headers
                .lock()
                .unwrap()
                .get(hash)
                .copied()
                .unwrap_or(genesis.header);
            if params[1] == false {
                json!(hex::encode(serialize(&header)))
            } else {
                json!({"hash":hash,"confirmations":1,"height":if mode==9 { u32::MAX } else { 0 },"version":1,"versionHex":"00000001",
                "merkleroot":header.merkle_root.to_string(),"time":header.time,"mediantime":header.time,
                "nonce":header.nonce,"bits":format!("{:08x}",header.bits.to_consensus()),"difficulty":1.0,
                "chainwork":"00","nTx":1,"previousblockhash":genesis.header.prev_blockhash.to_string()})
            }
        }
        "getblock" => json!(hex::encode(serialize(&genesis))),
        other => panic!("unexpected Core method {other}"),
    }
}
