//! The success envelope: `schema_version`, `generated_at`, and an optional
//! normalized `query` echo. See `docs/api-contract.md` (Common Envelope).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

/// Epoch seconds for the `generated_at` field. Shared by the success and error
/// envelopes so both use one clock source.
pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The success envelope wrapping a typed endpoint payload `T`. `query` is the
/// normalized query echo for endpoints that take query parameters;
/// `/api/v1/block/:hash` omits it. Payload fields are flattened to the top
/// level to match the fixtures (e.g. `tree.json` has `nodes` alongside
/// `schema_version`).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct SuccessEnvelope<T> {
    pub(crate) schema_version: &'static str,
    pub(crate) generated_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) query: Option<Value>,
    #[serde(flatten)]
    pub(crate) payload: T,
}

impl<T> SuccessEnvelope<T> {
    /// Build the envelope stamping `generated_at` from the wall clock now. The
    /// default for endpoints whose payload does not itself depend on the
    /// timestamp (use `with_generated_at` when the projection and the envelope
    /// must share one clock value).
    pub(crate) fn new(payload: T, query: Option<Value>) -> Self {
        Self {
            schema_version: super::SCHEMA_VERSION,
            generated_at: now_epoch_secs(),
            query,
            payload,
        }
    }

    /// Build the envelope reusing a `generated_at` the caller already captured.
    /// `/api/v1/sources` computes per-source freshness against the same epoch it
    /// stamps here (`projection::sources(reference_now)`), so the envelope clock
    /// must match the projection clock; a second `now_epoch_secs()` call could
    /// diverge.
    pub(crate) fn with_generated_at(payload: T, query: Option<Value>, generated_at: u64) -> Self {
        Self {
            schema_version: super::SCHEMA_VERSION,
            generated_at,
            query,
            payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn success_envelope_shape_and_query_echo() {
        let env = SuccessEnvelope::new(json!({ "stales": [] }), Some(json!({ "limit": 10 })));
        let v = serde_json::to_value(&env).unwrap();
        assert_eq!(v["schema_version"], "v1");
        assert!(v["generated_at"].as_u64().unwrap() > 0);
        assert_eq!(v["query"]["limit"], 10);
        assert_eq!(v["stales"], json!([]));
    }

    #[test]
    fn query_omitted_when_none() {
        let env = SuccessEnvelope::new(json!({ "sources": [] }), None);
        let v = serde_json::to_value(&env).unwrap();
        assert!(v.get("query").is_none());
        assert_eq!(v["sources"], json!([]));
    }
}
