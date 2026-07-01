//! The release read API behind the `serve` subcommand: axum routing, shared
//! application state, success/error envelopes, query normalization, DB-backed
//! JSON handlers, and `ServeDir` static serving for `www/`.

mod envelope;
mod error;
mod handlers;
mod normalize;
#[cfg(feature = "db-integration")]
pub mod projection;
#[cfg(not(feature = "db-integration"))]
mod projection;
#[cfg(feature = "db-integration")]
pub mod query;
#[cfg(not(feature = "db-integration"))]
mod query;
mod version;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::Request;
use axum::http::header::{CACHE_CONTROL, HeaderValue};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use tokio_postgres::NoTls;
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use mmm_pg::PgConfig;

#[cfg(feature = "db-integration")]
pub use error::ApiError;
#[cfg(feature = "db-integration")]
pub use normalize::Classification;

/// The schema version echoed in every envelope.
pub const SCHEMA_VERSION: &str = "v1";

/// The application SemVer compiled from the workspace package version.
pub const APPLICATION_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Shared application state. Cloned per request; `deadpool_postgres::Pool` is an
/// `Arc` internally, so cloning is cheap.
#[derive(Clone)]
pub struct AppState {
    pool: Pool,
}

impl AppState {
    /// Build state around the lazily-connecting DB pool.
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Borrow the connection pool. Read handlers call this to check out a
    /// pooled client.
    pub(crate) fn pool(&self) -> &Pool {
        &self.pool
    }
}

/// `serve` subcommand configuration, sourced from the environment.
pub struct ServeConfig {
    pub pg: PgConfig,
    pub bind_addr: SocketAddr,
    pub www_dir: PathBuf,
    pub db_pool_size: usize,
    pub bitcoin_rpc_url: Option<String>,
}

impl ServeConfig {
    /// Read the serve configuration from the environment. Empty-or-whitespace
    /// `BITCOIN_RPC_URL` is treated as unset when empty or whitespace (not as a
    /// literal empty value); `SERVE_DB_POOL_SIZE` defaults to 8, `SERVE_BIND_ADDR`
    /// to 127.0.0.1:8080.
    pub fn from_env() -> Result<Self> {
        let bind_addr =
            std::env::var("SERVE_BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_owned());
        let bind_addr = bind_addr.parse::<SocketAddr>().with_context(|| {
            format!("SERVE_BIND_ADDR {bind_addr:?} is not a valid socket address")
        })?;
        let www_dir = std::env::var("SERVE_WWW_DIR").unwrap_or_else(|_| "www".to_owned());
        let db_pool_size = std::env::var("SERVE_DB_POOL_SIZE")
            .ok()
            .map(|raw| {
                raw.parse::<usize>()
                    .context("SERVE_DB_POOL_SIZE must be a usize")
            })
            .transpose()?
            .unwrap_or(8);
        let bitcoin_rpc_url = match std::env::var("BITCOIN_RPC_URL") {
            Ok(url) if !url.trim().is_empty() => Some(url),
            Ok(_) | Err(std::env::VarError::NotPresent) => None,
            Err(err) => return Err(err).context("read BITCOIN_RPC_URL"),
        };
        Ok(Self {
            pg: PgConfig::from_env()?,
            bind_addr,
            www_dir: PathBuf::from(www_dir),
            db_pool_size,
            bitcoin_rpc_url,
        })
    }
}

/// Build a `deadpool_postgres::Pool` WITHOUT connecting (deadpool connects
/// lazily on first checkout), reusing `PgConfig::to_tokio_config`.
pub fn build_pool(pg: &PgConfig, max_size: usize) -> Result<Pool> {
    let manager = Manager::from_config(
        pg.to_tokio_config(),
        NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .build()
        .context("build Postgres connection pool")
}

/// Assemble the axum router: the `/api/v1` routes plus a `ServeDir` static
/// fallback for `www/`, wrapped in a request-tracing layer.
pub fn router(state: AppState, www_dir: impl AsRef<Path>) -> Router {
    let serve_dir = ServeDir::new(www_dir.as_ref()).append_index_html_on_directories(true);

    Router::new()
        .route("/api/v1/tree", get(handlers::tree))
        .route("/api/v1/block/{hash}", get(handlers::block))
        .route("/api/v1/navigator/{target}", get(handlers::navigator))
        .route("/api/v1/sources", get(handlers::sources))
        .route("/api/v1/version", get(handlers::version_metadata))
        .route("/health", get(handlers::health))
        .route("/ready", get(handlers::ready))
        .fallback_service(serve_dir)
        .layer(
            ServiceBuilder::new()
                .layer(middleware::from_fn(cache_headers))
                .layer(TraceLayer::new_for_http()),
        )
        .with_state(state)
}

async fn cache_headers(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    apply_cache_policy(&path, response.status(), response.headers_mut());
    response
}

fn apply_cache_policy(path: &str, status: StatusCode, headers: &mut HeaderMap) {
    let cache_control = if status.is_success() || status == StatusCode::NOT_MODIFIED {
        cache_control_for_path(path)
    } else {
        HeaderValue::from_static("no-store")
    };

    headers.insert(CACHE_CONTROL, cache_control);
}

fn cache_control_for_path(path: &str) -> HeaderValue {
    match path {
        "/health" | "/ready" => HeaderValue::from_static("no-store"),
        path if path.starts_with("/api/") => HeaderValue::from_static(api_cache_control()),
        "/" | "/index.html" => HeaderValue::from_static(
            "public, max-age=0, s-maxage=30, stale-while-revalidate=60, must-revalidate",
        ),
        "/vendor/d3.v7.min.js" => {
            HeaderValue::from_static("public, max-age=31536000, s-maxage=31536000, immutable")
        }
        _ => HeaderValue::from_static(
            "public, max-age=300, s-maxage=3600, stale-while-revalidate=86400",
        ),
    }
}

fn api_cache_control() -> &'static str {
    "public, max-age=15, s-maxage=30, stale-while-revalidate=60"
}

/// Boot the read-API service: build the pool, warn on a missing static dir,
/// bind, and serve until ctrl-c.
pub async fn serve(cfg: ServeConfig) -> Result<()> {
    let pool = build_pool(&cfg.pg, cfg.db_pool_size)?;
    if cfg.bitcoin_rpc_url.is_some() {
        warn!(
            "BITCOIN_RPC_URL is ignored by serve; run sync-bitcoin-core to populate tree backbone rows"
        );
    }
    let state = AppState::new(pool);

    if !cfg.www_dir.is_dir() {
        warn!(
            www_dir = %cfg.www_dir.display(),
            "SERVE_WWW_DIR does not exist; static requests will 404 (the UI lands in SP5)"
        );
    }

    let app = router(state, &cfg.www_dir);

    let listener = tokio::net::TcpListener::bind(cfg.bind_addr)
        .await
        .with_context(|| format!("bind read-API listener on {}", cfg.bind_addr))?;
    info!(bind_addr = %cfg.bind_addr, "read API listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("read-API server error")?;
    Ok(())
}

/// Resolve when ctrl-c fires (or immediately on handler-install failure, logging
/// a warning) so axum can drain in-flight requests.
async fn shutdown_signal() {
    if let Err(err) = tokio::signal::ctrl_c().await {
        warn!(error = %err, "failed to install ctrl-c handler; shutting down");
    }
}
