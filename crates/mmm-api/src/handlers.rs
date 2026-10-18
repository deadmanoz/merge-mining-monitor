//! Read-API handlers.

use axum::Json;
use axum::extract::rejection::PathRejection;
use axum::extract::{OriginalUri, Path, RawQuery, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use deadpool_postgres::Object;
use serde::Serialize;
use serde_json::Value;
use std::io::ErrorKind;
use std::time::Duration;
use tokio::time::timeout;
use tracing::error;

use super::envelope::{SuccessEnvelope, now_epoch_secs};
use super::error::{ApiError, internal_error_response};
use super::normalize::normalize_hash;
use super::projection::{self, ProjectionError};
use super::query::{NavigatorTarget, parse_navigator_query, parse_tree_query};
use super::version;
use super::{AppState, FRONTEND_ASSET_PREFIX};

pub(crate) async fn index_html(State(state): State<AppState>) -> Response {
    let index_path = state.www_dir().join("index.html");
    match tokio::fs::read_to_string(&index_path).await {
        Ok(raw) => {
            let css_attr = format!("href=\"{FRONTEND_ASSET_PREFIX}/css/");
            let js_attr = format!("src=\"{FRONTEND_ASSET_PREFIX}/js/");
            let rendered = raw
                .replace("href=\"/css/", &css_attr)
                .replace("src=\"/js/", &js_attr);
            Html(rendered).into_response()
        }
        Err(err) if err.kind() == ErrorKind::NotFound => StatusCode::NOT_FOUND.into_response(),
        Err(err) => {
            error!(error = %err, path = %index_path.display(), "failed to read index.html");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub(crate) async fn tree(State(state): State<AppState>, RawQuery(query): RawQuery) -> Response {
    respond("tree", tree_response(&state, query).await)
}

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

/// No query validation per the API contract.
pub(crate) async fn sources(State(state): State<AppState>) -> Response {
    respond("sources", sources_response(&state).await)
}

/// No query validation per the API contract; serves the full set for client filtering.
pub(crate) async fn competitions(State(state): State<AppState>) -> Response {
    respond("competitions", competitions_response(&state).await)
}

/// Compile-time version metadata and release notes; never touches Postgres.
pub(crate) async fn version_metadata() -> Response {
    success_response(version::payload(), None)
}

/// Process liveness probe; does not touch Postgres.
pub(crate) async fn health() -> StatusCode {
    StatusCode::NO_CONTENT
}

pub(crate) async fn ready(State(state): State<AppState>) -> StatusCode {
    let Ok(Ok(client)) = timeout(Duration::from_secs(2), state.pool().get()).await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    let Ok(Ok(_)) = timeout(Duration::from_secs(2), client.simple_query("SELECT 1")).await else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    StatusCode::NO_CONTENT
}

/// Maps `PathRejection` (e.g. non-UTF-8 percent-escapes like `%ff`) to
/// `invalid_hash` using the raw last path segment from `OriginalUri`.
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
