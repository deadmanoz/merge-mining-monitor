//! Read-API error codes, HTTP statuses, and error envelope (`docs/api-contract.md`).

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::{Value, json};

use super::SCHEMA_VERSION;
use super::envelope::now_epoch_secs;

#[derive(Debug)]
pub enum ApiError {
    /// HTTP 400: malformed or semantically invalid query parameters.
    InvalidQuery { message: String, details: Value },
    /// HTTP 400: `:hash` path parameter is not 64 hex characters.
    InvalidHash { raw: String },
    /// HTTP 400: well-formed but unregistered source code.
    UnsupportedSource { source: String },
    /// HTTP 404: valid normalized hash with no matching block.
    NotFound { hash: String },
    /// HTTP 422: valid request whose window exceeds an endpoint bound.
    RangeTooLarge {
        parameter: String,
        limit: u64,
        received: u64,
        message: String,
    },
    /// HTTP 409: Bitcoin height window is not covered by the synced Bitcoin Core backbone.
    BackboneUnsynced { details: Value },
    /// HTTP 409: Bitcoin height window has an inconsistent Bitcoin Core backbone.
    BackboneConflict { details: Value },
}

#[derive(Debug, Serialize)]
pub struct ErrorEnvelope {
    pub schema_version: &'static str,
    pub generated_at: u64,
    pub error: ErrorBody,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl ApiError {
    pub fn invalid_query(message: impl Into<String>, details: Value) -> Self {
        Self::InvalidQuery {
            message: message.into(),
            details,
        }
    }

    pub fn invalid_hash(raw: impl Into<String>) -> Self {
        Self::InvalidHash { raw: raw.into() }
    }

    pub fn unsupported_source(source: impl Into<String>) -> Self {
        Self::UnsupportedSource {
            source: source.into(),
        }
    }

    pub fn not_found(hash: impl Into<String>) -> Self {
        Self::NotFound { hash: hash.into() }
    }

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
    fn into_response(self) -> Response {
        (self.status(), Json(self.to_envelope())).into_response()
    }
}

/// HTTP 500 response for unexpected internal failures (logs real cause in handlers; leaks no details).
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
