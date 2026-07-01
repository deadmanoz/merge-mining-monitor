//! Read-API handlers. Each route validates its own query params before checking
//! out a DB connection, then wraps the typed projection in the shared success
//! envelope.

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use deadpool_postgres::Object;
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;
use tracing::error;

use super::AppState;
use super::envelope::{SuccessEnvelope, now_epoch_secs};
use super::error::{ApiError, internal_error_response};
use super::normalize::normalize_hash;
use super::projection::{self, ProjectionError};
use super::query::{NavigatorTarget, parse_navigator_query, parse_tree_query};
use super::version;

/// `/api/v1/tree` projects the merge-mining attribution tree (optional
/// `source`/`window` query). Validates query params before DB checkout;
/// payload pinned by fixtures/api/tree.json and tree-unheighted-anchor.json.
pub(crate) async fn tree(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    match tree_response(&state, query).await {
        Ok(response) => response,
        Err(EndpointError::Api(err)) => err.into_response(),
        Err(EndpointError::Internal(err)) => {
            error!(error = %err, "tree endpoint failed");
            internal_error_response()
        }
    }
}

/// `/api/v1/navigator/{target}` is the unified navigator index for stale
/// blocks, stale branches, BTC orphans, and orphan branches. It supports latest,
/// cursor page, and anchor-hash locate modes through one parser.
pub(crate) async fn navigator(
    State(state): State<AppState>,
    Path(target): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    match navigator_response(&state, &target, query).await {
        Ok(response) => response,
        Err(EndpointError::Api(err)) => err.into_response(),
        Err(EndpointError::Internal(err)) => {
            error!(error = %err, "navigator endpoint failed");
            internal_error_response()
        }
    }
}

/// `/api/v1/sources` has NO query validation per the contract.
pub(crate) async fn sources(State(state): State<AppState>) -> Response {
    match sources_response(&state).await {
        Ok(response) => response,
        Err(EndpointError::Api(err)) => err.into_response(),
        Err(EndpointError::Internal(err)) => {
            error!(error = %err, "sources endpoint failed");
            internal_error_response()
        }
    }
}

/// `/api/v1/version` serves compile-time application version metadata and the
/// full release-note projection. It never checks out a database connection.
pub(crate) async fn version_metadata() -> Response {
    success_response(version::payload(), None)
}

/// `/health` is process liveness. It intentionally does not touch Postgres so
/// supervisors can distinguish a running process from a ready read service.
pub(crate) async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// `/ready` is the public readiness probe: the process is running and can
/// complete a bounded checkout plus trivial Postgres query.
pub(crate) async fn ready(State(state): State<AppState>) -> StatusCode {
    let Ok(Ok(client)) = timeout(Duration::from_secs(2), state.pool().get()).await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let Ok(Ok(_)) = timeout(Duration::from_secs(2), client.simple_query("SELECT 1")).await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    StatusCode::NO_CONTENT
}

/// `/api/v1/block/{hash}` owns the invalid_hash envelope even when axum's `Path`
/// extractor would reject (a percent-escape that decodes to invalid UTF-8, e.g.
/// `%ff`). A `PathRejection` maps to invalid_hash using the raw last path
/// segment from `OriginalUri`; otherwise the segment is normalized before DB
/// lookup.
pub(crate) async fn block(
    State(state): State<AppState>,
    OriginalUri(uri): OriginalUri,
    hash: Result<Path<String>, PathRejection>,
) -> Response {
    let hash = match hash {
        Ok(Path(hash)) => hash,
        Err(_) => {
            let raw = uri.path().rsplit('/').next().unwrap_or_default().to_owned();
            return ApiError::invalid_hash(raw).into_response();
        }
    };
    match block_response(&state, &hash).await {
        Ok(response) => response,
        Err(EndpointError::Api(err)) => err.into_response(),
        Err(EndpointError::Internal(err)) => {
            error!(error = %err, "block endpoint failed");
            internal_error_response()
        }
    }
}

/// Handler-local split: `Api` errors render the typed client envelope as-is;
/// `Internal` errors are logged then masked behind the generic 500
/// (`internal_error_response`) so no internal detail leaks.
enum EndpointError {
    Api(ApiError),
    Internal(anyhow::Error),
}

impl From<ApiError> for EndpointError {
    fn from(err: ApiError) -> Self {
        Self::Api(err)
    }
}

impl From<ProjectionError> for EndpointError {
    fn from(err: ProjectionError) -> Self {
        match err {
            ProjectionError::Api(err) => Self::Api(err),
            ProjectionError::Internal(err) => Self::Internal(err),
        }
    }
}

async fn db_client(state: &AppState) -> Result<Object, EndpointError> {
    state
        .pool()
        .get()
        .await
        .map_err(|err| EndpointError::Internal(err.into()))
}

fn success_response<T: Serialize>(payload: T, query: Option<Value>) -> Response {
    Json(SuccessEnvelope::new(payload, query)).into_response()
}

/// Fallible body of the matching handler: validate/parse the query first, THEN
/// check out a pooled client, so a malformed request 400s without consuming a
/// connection. (sources_response additionally threads one `generated_at` into
/// both envelope and payload.)
async fn tree_response(
    state: &AppState,
    raw_query: Option<String>,
) -> Result<Response, EndpointError> {
    let query = parse_tree_query(raw_query.as_deref())?;
    let client = db_client(state).await?;
    let payload = projection::tree(&client, &query).await?;
    Ok(success_response(payload, Some(query.query)))
}

/// Fallible body of the matching handler: validate/parse the query first, THEN
/// check out a pooled client, so a malformed request 400s without consuming a
/// connection. (sources_response additionally threads one `generated_at` into
/// both envelope and payload.)
async fn block_response(state: &AppState, hash: &str) -> Result<Response, EndpointError> {
    let hash = normalize_hash(hash)?;
    let client = db_client(state).await?;
    let payload = projection::block(&client, &hash).await?;
    Ok(success_response(payload, None))
}

/// Fallible body of the matching handler: validate/parse the query first, THEN
/// check out a pooled client, so a malformed request 400s without consuming a
/// connection. (sources_response additionally threads one `generated_at` into
/// both envelope and payload.)
async fn navigator_response(
    state: &AppState,
    raw_target: &str,
    raw_query: Option<String>,
) -> Result<Response, EndpointError> {
    let target = NavigatorTarget::parse(raw_target)?;
    let query = parse_navigator_query(target, raw_query.as_deref())?;
    let client = db_client(state).await?;
    let payload = projection::navigator(&client, &query).await?;
    Ok(success_response(payload, Some(query.query)))
}

/// Fallible body of the matching handler: validate/parse the query first, THEN
/// check out a pooled client, so a malformed request 400s without consuming a
/// connection. (sources_response additionally threads one `generated_at` into
/// both envelope and payload.)
async fn sources_response(state: &AppState) -> Result<Response, EndpointError> {
    let generated_at = now_epoch_secs();
    let client = db_client(state).await?;
    let payload = projection::sources(&client, generated_at).await?;
    Ok(Json(SuccessEnvelope::with_generated_at(
        payload,
        None,
        generated_at,
    ))
    .into_response())
}
