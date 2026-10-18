//! Success envelope (`schema_version`, `generated_at`, optional `query` echo).

use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::Value;

pub(crate) fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

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
    pub(crate) fn new(payload: T, query: Option<Value>) -> Self {
        Self {
            schema_version: super::SCHEMA_VERSION,
            generated_at: now_epoch_secs(),
            query,
            payload,
        }
    }

    /// Reuses a caller-captured `generated_at` so projection freshness and
    /// envelope metadata share one clock value.
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
