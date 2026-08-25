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
    respond("tree", tree_response(&state, query).await)
}

/// `/api/v1/navigator/{target}` is the unified navigator index for stale
/// blocks, stale branches, consensus-invalid error blocks, BTC orphans, and orphan
/// branches. It supports latest, cursor page, and anchor-hash locate modes
/// through one parser.
pub(crate) async fn navigator(
    State(state): State<AppState>,
    Path(target): Path<String>,
    RawQuery(query): RawQuery,
) -> Response {
    respond(
        "navigator",
        navigator_response(&state, &target, query).await,
    )
}

/// `/api/v1/sources` has NO query validation per the contract.
pub(crate) async fn sources(State(state): State<AppState>) -> Response {
    respond("sources", sources_response(&state).await)
}

/// `/api/v1/competitions` has NO query validation per the contract: it serves
/// the whole competition set and the client filters locally.
pub(crate) async fn competitions(State(state): State<AppState>) -> Response {
    respond("competitions", competitions_response(&state).await)
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
    respond("block", block_response(&state, &hash).await)
}

fn respond(endpoint: &'static str, result: Result<Response, ProjectionError>) -> Response {
    match result {
        Ok(response) => response,
        Err(ProjectionError::Api(err)) => err.into_response(),
        Err(ProjectionError::Internal(err)) => {
            error!(error = %err, "{endpoint} endpoint failed");
            internal_error_response()
        }
    }
}

async fn db_client(state: &AppState) -> Result<Object, ProjectionError> {
    state
        .pool()
        .get()
        .await
        .map_err(|err| ProjectionError::Internal(err.into()))
}

fn success_response<T: Serialize>(payload: T, query: Option<Value>) -> Response {
    Json(SuccessEnvelope::new(payload, query)).into_response()
}

async fn tree_response(
    state: &AppState,
    raw_query: Option<String>,
) -> Result<Response, ProjectionError> {
    let query = parse_tree_query(raw_query.as_deref())?;
    let client = db_client(state).await?;
    let payload = projection::tree(&client, &query).await?;
    Ok(success_response(payload, Some(query.query)))
}

async fn block_response(state: &AppState, hash: &str) -> Result<Response, ProjectionError> {
    let hash = normalize_hash(hash)?;
    let client = db_client(state).await?;
    let payload = projection::block(&client, &hash).await?;
    Ok(success_response(payload, None))
}

async fn navigator_response(
    state: &AppState,
    raw_target: &str,
    raw_query: Option<String>,
) -> Result<Response, ProjectionError> {
    let target = NavigatorTarget::parse(raw_target)?;
    let query = parse_navigator_query(target, raw_query.as_deref())?;
    let client = db_client(state).await?;
    let payload = projection::navigator(&client, &query).await?;
    Ok(success_response(payload, Some(query.query)))
}

/// Threads one timestamp into both the payload and envelope.
async fn sources_response(state: &AppState) -> Result<Response, ProjectionError> {
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

/// Fallible body of the competitions handler. No query to validate, so it goes
/// straight to a pooled client.
async fn competitions_response(state: &AppState) -> Result<Response, ProjectionError> {
    let generated_at = now_epoch_secs();
    let client = db_client(state).await?;
    let payload = projection::competitions(&client).await?;
    Ok(Json(SuccessEnvelope::with_generated_at(
        payload,
        None,
        generated_at,
    ))
    .into_response())
}
