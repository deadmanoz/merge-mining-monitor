//! The first-wave read-API error contract: the error codes, their HTTP
//! statuses, and the shared error envelope. This module is value-compatible with
//! the `fixtures/api/error-*.json` fixtures. See `docs/api-contract.md` (Errors).
//!
//! `InvalidQuery` and `RangeTooLarge` are reused across many endpoints and call
//! sites, so they carry a per-call `message` (each error fixture is only one
//! instance of that code). The other codes have stable messages.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

use super::SCHEMA_VERSION;
use super::envelope::now_epoch_secs;

#[derive(Debug)]
pub enum ApiError {
    /// HTTP 400. Malformed or semantically invalid query parameters; `details`
    /// is an object keyed by the invalid parameter(s).
    InvalidQuery { message: String, details: Value },
    /// HTTP 400. The `:hash` path parameter is not 64 hex characters.
    InvalidHash { raw: String },
    /// HTTP 400. A well-formed but unregistered source code.
    UnsupportedSource { source: String },
    /// HTTP 404. A valid, normalized hash with no matching block.
    NotFound { hash: String },
    /// HTTP 422. A valid request whose window exceeds an endpoint bound.
    RangeTooLarge {
        parameter: String,
        limit: u64,
        received: u64,
        message: String,
    },
    /// HTTP 409. The requested Bitcoin height window is not covered by the
    /// synced Bitcoin Core backbone.
    BackboneUnsynced { details: Value },
    /// HTTP 409. The requested Bitcoin height window has an inconsistent
    /// Bitcoin Core backbone, such as duplicate canonical rows or broken links.
    BackboneConflict { details: Value },
}

/// The wire body format: `schema_version`, `generated_at`, `error{...}`. A typed
/// struct (not a `json!` map) so serialization preserves the fixture field order.
#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    pub schema_version: &'static str,
    pub generated_at: u64,
    pub error: ErrorBody,
}

/// The inner `error` object: `code`, `message`, `details`. Field order is the
/// wire contract (this is the object every `error-*.json` fixture asserts
/// value-equal, ignoring the volatile envelope `generated_at`).
#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl ApiError {
    /// Construct an `invalid_query` (HTTP 400) with a per-call message and a
    /// `details` object keyed by the offending parameter(s).
    pub fn invalid_query(message: impl Into<String>, details: Value) -> Self {
        Self::InvalidQuery {
            message: message.into(),
            details,
        }
    }

    /// Construct an `invalid_hash` (HTTP 400) carrying the raw, un-normalized
    /// `:hash` for the `details.hash` echo.
    pub fn invalid_hash(raw: impl Into<String>) -> Self {
        Self::InvalidHash { raw: raw.into() }
    }

    /// Construct an `unsupported_source` (HTTP 400) for a well-formed but
    /// unregistered source code.
    pub fn unsupported_source(source: impl Into<String>) -> Self {
        Self::UnsupportedSource {
            source: source.into(),
        }
    }

    /// Construct a `not_found` (HTTP 404) for a valid normalized hash that
    /// matches no block (or no in-set anchor).
    pub fn not_found(hash: impl Into<String>) -> Self {
        Self::NotFound { hash: hash.into() }
    }

    /// Construct a `range_too_large` (HTTP 422) with a per-call message; the
    /// `parameter`/`limit`/`received` triple becomes `details` (pinned by
    /// `error-range-too-large.json`).
    pub fn range_too_large(
        parameter: impl Into<String>,
        limit: u64,
        received: u64,
        message: impl Into<String>,
    ) -> Self {
        Self::RangeTooLarge {
            parameter: parameter.into(),
            limit,
            received,
            message: message.into(),
        }
    }

    /// The wire `error.code` string.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidQuery { .. } => "invalid_query",
            Self::InvalidHash { .. } => "invalid_hash",
            Self::UnsupportedSource { .. } => "unsupported_source",
            Self::NotFound { .. } => "not_found",
            Self::RangeTooLarge { .. } => "range_too_large",
            Self::BackboneUnsynced { .. } => "backbone_unsynced",
            Self::BackboneConflict { .. } => "backbone_conflict",
        }
    }

    /// The HTTP status for this code.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::InvalidQuery { .. }
            | Self::InvalidHash { .. }
            | Self::UnsupportedSource { .. } => StatusCode::BAD_REQUEST,
            Self::NotFound { .. } => StatusCode::NOT_FOUND,
            Self::RangeTooLarge { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            Self::BackboneUnsynced { .. } | Self::BackboneConflict { .. } => StatusCode::CONFLICT,
        }
    }

    /// The wire `error.message` string.
    pub fn message(&self) -> String {
        match self {
            Self::InvalidQuery { message, .. } => message.clone(),
            Self::InvalidHash { .. } => "hash must be 64 hex characters".to_owned(),
            Self::UnsupportedSource { .. } => "source code is not registered".to_owned(),
            Self::NotFound { .. } => "block was not found".to_owned(),
            Self::RangeTooLarge { message, .. } => message.clone(),
            Self::BackboneUnsynced { .. } => {
                "Bitcoin Core backbone is not synced for this tree window".to_owned()
            }
            Self::BackboneConflict { .. } => {
                "Bitcoin Core backbone is inconsistent for this tree window".to_owned()
            }
        }
    }

    /// The wire `error.details` object.
    pub fn details(&self) -> Value {
        match self {
            Self::InvalidQuery { details, .. } => details.clone(),
            Self::InvalidHash { raw } => json!({ "hash": raw }),
            Self::UnsupportedSource { source } => json!({ "source": source }),
            Self::NotFound { hash } => json!({ "hash": hash }),
            Self::RangeTooLarge {
                parameter,
                limit,
                received,
                ..
            } => json!({
                "parameter": parameter,
                "limit": limit,
                "received": received,
            }),
            Self::BackboneUnsynced { details } | Self::BackboneConflict { details } => {
                details.clone()
            }
        }
    }

    /// Build the full error envelope (schema_version + generated_at + error).
    pub fn to_envelope(&self) -> ErrorEnvelope {
        ErrorEnvelope {
            schema_version: SCHEMA_VERSION,
            generated_at: now_epoch_secs(),
            error: ErrorBody {
                code: self.code(),
                message: self.message(),
                details: self.details(),
            },
        }
    }
}

impl IntoResponse for ApiError {
    /// Render the error to its locked wire form: HTTP `status()` + the
    /// `ErrorEnvelope` JSON body. The single production mapping every handler
    /// error funnels through.
    fn into_response(self) -> Response {
        (self.status(), Json(self.to_envelope())).into_response()
    }
}

/// The HTTP 500 response for unexpected internal failures. Deliberately not
/// an [`ApiError`] variant: it leaks no details, has a fixed `internal_error`
/// code and message, and empty `details{}`. Handlers return it after logging
/// the real cause.
pub(crate) fn internal_error_response() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(ErrorEnvelope {
            schema_version: SCHEMA_VERSION,
            generated_at: now_epoch_secs(),
            error: ErrorBody {
                code: "internal_error",
                message: "internal server error".to_owned(),
                details: json!({}),
            },
        }),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Value {
        let path = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."))
            .join("fixtures/api")
            .join(name);
        serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|err| {
            panic!("read {}: {err}", path.display());
        }))
        .unwrap()
    }

    /// Assert the status, the envelope wrapper, and that the `error` object is
    /// value-equal to the fixture (the volatile `generated_at` is ignored).
    fn assert_matches_fixture(err: &ApiError, status: StatusCode, fixture_name: &str) {
        assert_eq!(err.status(), status, "{fixture_name} status");
        let env = serde_json::to_value(err.to_envelope()).unwrap();
        assert_eq!(env["schema_version"], "v1", "{fixture_name} schema_version");
        assert!(
            env["generated_at"].as_u64().unwrap() > 0,
            "{fixture_name} generated_at"
        );
        assert_eq!(
            env["error"],
            fixture(fixture_name)["error"],
            "{fixture_name} error object"
        );
    }

    #[test]
    fn invalid_query_matches_fixture() {
        let err = ApiError::invalid_query(
            "from_height must be less than or equal to to_height",
            json!({ "from_height": 700010, "to_height": 700000 }),
        );
        assert_matches_fixture(&err, StatusCode::BAD_REQUEST, "error-invalid-query.json");
    }

    #[test]
    fn invalid_hash_matches_fixture() {
        let err = ApiError::invalid_hash("not-a-hash");
        assert_matches_fixture(&err, StatusCode::BAD_REQUEST, "error-invalid-hash.json");
    }

    #[test]
    fn unsupported_source_matches_fixture() {
        let err = ApiError::unsupported_source("auxpow:not-a-registered-chain");
        assert_matches_fixture(
            &err,
            StatusCode::BAD_REQUEST,
            "error-unsupported-source.json",
        );
    }

    // not_found and range_too_large are not HTTP-reachable in SP1; these assert
    // their status + envelope at the response-mapping level so SP2/SP3 inherit a
    // proven contract.
    #[test]
    fn not_found_matches_fixture() {
        let err =
            ApiError::not_found("abababababababababababababababababababababababababababababababab");
        assert_matches_fixture(&err, StatusCode::NOT_FOUND, "error-not-found.json");
    }

    #[test]
    fn range_too_large_matches_fixture() {
        let err = ApiError::range_too_large(
            "height_window",
            2016,
            4096,
            "requested tree window exceeds the first-wave limit",
        );
        assert_matches_fixture(
            &err,
            StatusCode::UNPROCESSABLE_ENTITY,
            "error-range-too-large.json",
        );
    }

    #[test]
    fn backbone_unsynced_matches_fixture() {
        let err = ApiError::BackboneUnsynced {
            details: json!({
                "from_height": 100,
                "to_height": 115,
                "first_missing_height": 104,
                "missing_count": 2u64,
                "partial_count": 1u64,
                "conflict_count": 0u64,
                "action": "run sync-bitcoin-core",
            }),
        };
        assert_matches_fixture(&err, StatusCode::CONFLICT, "error-backbone-unsynced.json");
    }

    #[test]
    fn backbone_conflict_matches_fixture() {
        let err = ApiError::BackboneConflict {
            details: json!({
                "from_height": 100,
                "to_height": 115,
                "first_missing_height": null,
                "missing_count": 0u64,
                "partial_count": 0u64,
                "conflict_count": 1u64,
                "conflict_height": 108,
                "conflict_reason": "link_mismatch",
                "hashes": [
                    "0808080808080808080808080808080808080808080808080808080808080808"
                ],
                "action": "run sync-bitcoin-core",
            }),
        };
        assert_matches_fixture(&err, StatusCode::CONFLICT, "error-backbone-conflict.json");
    }
}
