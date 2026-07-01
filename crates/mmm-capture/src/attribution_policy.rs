//! Shared write-decision policy for keyed child-side attribution replays.
//!
//! The child reward/identity replays (Fractal reward, Elastos reward+minerinfo,
//! and reclassify-pools child payout) all load the existing
//! `event_pool_attribution` rows for an event as a JSON array, key them by
//! `(namespace, matched_value)`, and decide per candidate whether to
//! (over)write. The keying, JSON parsing, and full-tuple equality check were
//! duplicated byte-for-byte across those modules; this owns them once. The
//! per-replay conflict rule (overwrite-gated vs promote-only) is the only thing
//! that varies, expressed as [`WritePolicy`]. Pure: no I/O, no DB.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::capture::EventPoolAttribution;

/// How an existing row gates a candidate write once the candidate is neither
/// new (unseen key) nor an exact match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WritePolicy {
    /// Child-payout / reward replay: an existing resolved row is kept unless
    /// `overwrite` is set, and a candidate may never demote a resolved row to
    /// NULL.
    ChildPayout { overwrite: bool },
    /// Identity replay (Elastos): never remap a resolved row to a different
    /// pool, and never demote it; only promote NULL to resolved.
    IdentityPromoteOnly,
}

/// Existing child-side attribution rows for one event, keyed by
/// `(namespace, matched_value)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExistingAttributionSet {
    by_key: HashMap<(String, String), KeyedAttributionRow>,
}

impl ExistingAttributionSet {
    /// Parse the `jsonb_agg` array produced by the replay loaders. Rows missing
    /// any required string field are skipped (preserving prior behavior).
    pub fn from_json(value: &Value) -> Self {
        let mut by_key = HashMap::new();
        let Some(rows) = value.as_array() else {
            return Self::default();
        };

        for row in rows {
            let Some(namespace) = row.get("namespace").and_then(Value::as_str) else {
                continue;
            };
            let Some(matched_value) = row.get("matched_value").and_then(Value::as_str) else {
                continue;
            };
            let Some(source) = row.get("source").and_then(Value::as_str) else {
                continue;
            };
            let Some(match_kind) = row.get("match_kind").and_then(Value::as_str) else {
                continue;
            };
            let Some(confidence) = row.get("confidence").and_then(Value::as_str) else {
                continue;
            };

            by_key.insert(
                (namespace.to_owned(), matched_value.to_owned()),
                KeyedAttributionRow {
                    source: source.to_owned(),
                    match_kind: match_kind.to_owned(),
                    pool_id: row.get("pool_id").and_then(Value::as_i64),
                    pool_identity_id: row.get("pool_identity_id").and_then(Value::as_i64),
                    confidence: confidence.to_owned(),
                    details: row.get("details").cloned().unwrap_or_else(|| json!({})),
                },
            );
        }

        Self { by_key }
    }

    /// Whether `attribution` should be written, given the existing rows and the
    /// per-replay `policy`. An unseen key always writes; an exact match never
    /// rewrites; the policy decides the remaining conflict cases.
    pub fn should_write(&self, attribution: &EventPoolAttribution, policy: WritePolicy) -> bool {
        let key = (
            attribution.namespace.to_owned(),
            attribution.matched_value.clone(),
        );
        let Some(existing) = self.by_key.get(&key) else {
            return true;
        };
        if existing.matches(attribution) {
            return false;
        }
        match policy {
            WritePolicy::ChildPayout { overwrite } => {
                if existing.pool_id.is_some() && attribution.pool_id.is_none() {
                    return false;
                }
                if existing.pool_id.is_some() && !overwrite {
                    return false;
                }
            }
            WritePolicy::IdentityPromoteOnly => {
                if existing.pool_id.is_some()
                    && attribution.pool_id.is_some()
                    && (existing.pool_id != attribution.pool_id
                        || existing.pool_identity_id != attribution.pool_identity_id)
                {
                    return false;
                }
                if existing.pool_id.is_some() && attribution.pool_id.is_none() {
                    return false;
                }
            }
        }
        true
    }
}

/// One existing `event_pool_attribution` row, reduced to the fields that
/// participate in the full-tuple equality check (`matches`). Stored per
/// `(namespace, matched_value)` key inside [`ExistingAttributionSet`]; the key
/// fields themselves live in the map key, not here.
#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyedAttributionRow {
    source: String,
    match_kind: String,
    pool_id: Option<i64>,
    pool_identity_id: Option<i64>,
    confidence: String,
    details: Value,
}

impl KeyedAttributionRow {
    /// Full-tuple equality against a candidate: source, match_kind, pool_id,
    /// pool_identity_id, confidence (compared via `as_db_str`, the persisted
    /// form), and the `details` JSON. A match means the row already encodes this
    /// exact attribution, so `should_write` returns false. Any persisted field
    /// added to `EventPoolAttribution` MUST be added here too, or replays will
    /// spuriously rewrite unchanged rows.
    fn matches(&self, attribution: &EventPoolAttribution) -> bool {
        self.source == attribution.source
            && self.match_kind == attribution.match_kind
            && self.pool_id == attribution.pool_id
            && self.pool_identity_id == attribution.pool_identity_id
            && self.confidence == attribution.confidence.as_db_str()
            && self.details == attribution.details
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{PoolAttributionConfidence, PoolAttributionSide};

    const NS: &str = "elastos_reward_address";
    const VALUE: &str = "EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR";
    const SOURCE: &str = "child_payout_registry";

    fn attribution(pool_id: Option<i64>, pool_identity_id: Option<i64>) -> EventPoolAttribution {
        EventPoolAttribution {
            side: PoolAttributionSide::ChildBlock,
            namespace: NS,
            match_kind: "reward_address",
            matched_value: VALUE.to_owned(),
            pool_id,
            pool_identity_id,
            source: SOURCE,
            confidence: PoolAttributionConfidence::Medium,
            details: json!({}),
        }
    }

    fn existing_from(attribution: &EventPoolAttribution) -> ExistingAttributionSet {
        ExistingAttributionSet::from_json(&json!([{
            "source": attribution.source,
            "namespace": attribution.namespace,
            "match_kind": attribution.match_kind,
            "matched_value": attribution.matched_value,
            "pool_id": attribution.pool_id,
            "pool_identity_id": attribution.pool_identity_id,
            "confidence": attribution.confidence.as_db_str(),
            "details": attribution.details,
        }]))
    }

    #[test]
    fn unseen_key_always_writes() {
        let empty = ExistingAttributionSet::default();
        let candidate = attribution(Some(1), Some(2));
        assert!(empty.should_write(&candidate, WritePolicy::ChildPayout { overwrite: false }));
        assert!(empty.should_write(&candidate, WritePolicy::IdentityPromoteOnly));
    }

    #[test]
    fn exact_match_never_rewrites() {
        let candidate = attribution(Some(1), Some(2));
        let existing = existing_from(&candidate);
        assert!(!existing.should_write(&candidate, WritePolicy::ChildPayout { overwrite: true }));
        assert!(!existing.should_write(&candidate, WritePolicy::IdentityPromoteOnly));
    }

    #[test]
    fn child_payout_keeps_resolved_without_overwrite_and_never_demotes() {
        let resolved = attribution(Some(1), Some(2));
        let existing = existing_from(&resolved);
        // Different resolution, no overwrite: keep existing.
        let remap = attribution(Some(9), Some(10));
        assert!(!existing.should_write(&remap, WritePolicy::ChildPayout { overwrite: false }));
        // Different resolution, overwrite: write.
        assert!(existing.should_write(&remap, WritePolicy::ChildPayout { overwrite: true }));
        // Candidate would demote a resolved row to NULL: never, even with overwrite.
        let demote = attribution(None, None);
        assert!(!existing.should_write(&demote, WritePolicy::ChildPayout { overwrite: true }));
    }

    #[test]
    fn identity_promote_only_never_remaps_or_demotes_but_promotes_null() {
        let resolved = attribution(Some(1), Some(2));
        let existing = existing_from(&resolved);
        // Never remap a resolved row to a different pool.
        let remap = attribution(Some(9), Some(10));
        assert!(!existing.should_write(&remap, WritePolicy::IdentityPromoteOnly));
        // Never demote a resolved row to NULL.
        let demote = attribution(None, None);
        assert!(!existing.should_write(&demote, WritePolicy::IdentityPromoteOnly));
        // Promote an existing NULL row to resolved.
        let unresolved_existing = existing_from(&attribution(None, None));
        assert!(unresolved_existing.should_write(&resolved, WritePolicy::IdentityPromoteOnly));
    }
}
