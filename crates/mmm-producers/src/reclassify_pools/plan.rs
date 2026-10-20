//! Pure planning layer for `reclassify-pools`: decode one keyset page of
//! candidate rows and plan per-parent attribution updates.

use std::collections::HashMap;

use bitcoin::consensus::deserialize;
use serde_json::Value;
use tracing::warn;

use mmm_capture::attribution_policy::{ExistingAttributionSet, WritePolicy};
use mmm_capture::auxpow::TxOut;
use mmm_capture::capture::{
    EventPoolAttribution, published_parent_coinbase_output_addresses,
    resolve_child_payout_attributions, resolve_child_pool_attribution_from_script,
    resolve_parent_pool_attribution_from_coinbase,
    resolve_parent_pool_attribution_from_serialized_coinbase_outputs,
};
use mmm_capture::child_payout::{PoolIdentityLookup, params_for_source_code};
use mmm_capture::pool_resolver::PoolResolver;

use super::{ReclassifyPoolsConfig, ReclassifyPoolsStats};

#[derive(Debug)]
struct CandidateRow {
    id: i64,
    btc_parent_header_hash: Vec<u8>,
    source_code: String,
    parent_attribution: ExistingSourceAttribution,
    child_attribution: ExistingSourceAttribution,
    child_payout_attributions: ExistingAttributionSet,
    btc_parent_coinbase_script: Option<Vec<u8>>,
    btc_parent_coinbase_outputs: Option<Vec<u8>>,
    btc_parent_coinbase_outputs_text: Option<String>,
    child_coinbase_script: Option<Vec<u8>>,
    child_coinbase_outputs: Option<Vec<u8>>,
    event_confirmed_at: i64,
}

/// Source-scoped attribution state for one side of an event (`row_count` 0, 1, or >1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ExistingSourceAttribution {
    row_count: i64,
    namespace: Option<String>,
    match_kind: Option<String>,
    matched_value: Option<String>,
    pool_id: Option<i64>,
    pool_identity_id: Option<i64>,
    confidence: Option<String>,
    details: Option<Value>,
}

impl ExistingSourceAttribution {
    fn matches(&self, attribution: &EventPoolAttribution) -> bool {
        self.row_count == 1
            && self.namespace.as_deref() == Some(attribution.namespace)
            && self.match_kind.as_deref() == Some(attribution.match_kind)
            && self.matched_value.as_deref() == Some(attribution.matched_value.as_str())
            && self.pool_id == attribution.pool_id
            && self.pool_identity_id == attribution.pool_identity_id
            && self.confidence.as_deref() == Some(attribution.confidence.as_db_str())
            && self.details.as_ref() == Some(&attribution.details)
    }
}

#[derive(Debug)]
pub(super) struct PlannedUpdate {
    pub(super) event_id: i64,
    pub(super) parent_pool_changed: bool,
    pub(super) child_pool_changed: bool,
    pub(super) child_payout_changed: bool,
    pub(super) attributions: Vec<EventPoolAttribution>,
    /// Upserted without stale cleanup so sibling payout addresses survive.
    pub(super) child_payout_attributions: Vec<EventPoolAttribution>,
    pub(super) observed_at: i64,
}

#[derive(Debug)]
pub(super) struct ParentUpdateGroup {
    pub(super) parent_hash: Vec<u8>,
    pub(super) updates: Vec<PlannedUpdate>,
    pub(super) parent_rollup_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ParentResolution {
    pub(super) attribution: Option<EventPoolAttribution>,
    pub(super) corrupt_outputs: bool,
}

/// A stored parent coinbase, as the main and observation phases resolve its pool.
pub(super) struct StoredParentCoinbase<'a> {
    /// The event or observation id, for the corrupt-outputs warning.
    pub(super) id: i64,
    pub(super) script: Option<&'a [u8]>,
    pub(super) outputs: Option<&'a [u8]>,
    pub(super) outputs_text: Option<&'a str>,
}

fn resolve_parent_pool(
    row: &CandidateRow,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> ParentResolution {
    resolve_stored_parent_pool(
        &StoredParentCoinbase {
            id: row.id,
            script: row.btc_parent_coinbase_script.as_deref(),
            outputs: row.btc_parent_coinbase_outputs.as_deref(),
            outputs_text: row.btc_parent_coinbase_outputs_text.as_deref(),
        },
        resolver,
        pool_ids_by_slug,
    )
}

pub(super) fn resolve_stored_parent_pool(
    coinbase: &StoredParentCoinbase<'_>,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> ParentResolution {
    let script = coinbase.script.unwrap_or_default();
    match resolve_parent_pool_attribution_from_serialized_coinbase_outputs(
        script,
        coinbase.outputs,
        resolver,
        pool_ids_by_slug,
    ) {
        Ok(Some(attribution)) => ParentResolution {
            attribution: Some(attribution),
            corrupt_outputs: false,
        },
        Ok(None) => ParentResolution {
            attribution: resolve_parent_pool_from_published_text(
                coinbase.outputs_text,
                script,
                resolver,
                pool_ids_by_slug,
            ),
            corrupt_outputs: false,
        },
        Err(err) => {
            warn!(
                id = coinbase.id,
                error = %err,
                "btc_parent_coinbase_outputs is corrupt; \
                 trying preserved publication text"
            );
            ParentResolution {
                attribution: resolve_parent_pool_from_published_text(
                    coinbase.outputs_text,
                    script,
                    resolver,
                    pool_ids_by_slug,
                ),
                corrupt_outputs: true,
            }
        }
    }
}

fn resolve_parent_pool_from_published_text(
    outputs_text: Option<&str>,
    script: &[u8],
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> Option<EventPoolAttribution> {
    let addresses = outputs_text
        .map(published_parent_coinbase_output_addresses)
        .unwrap_or_default();
    resolve_parent_pool_attribution_from_coinbase(script, &addresses, resolver, pool_ids_by_slug)
}

struct ChildPayoutResolution {
    attributions: Vec<EventPoolAttribution>,
    changed: bool,
    corrupt_outputs: bool,
}

fn resolve_child_payouts(
    row: &CandidateRow,
    identities: &PoolIdentityLookup,
    overwrite: bool,
) -> ChildPayoutResolution {
    let Some(attributions) = replay_child_payouts(
        row.id,
        &row.source_code,
        row.child_coinbase_outputs.as_deref(),
        identities,
    ) else {
        return ChildPayoutResolution {
            attributions: Vec::new(),
            changed: false,
            corrupt_outputs: true,
        };
    };
    let attributions = attributions
        .into_iter()
        .filter(|attribution| {
            row.child_payout_attributions
                .should_write(attribution, WritePolicy::ChildPayout { overwrite })
        })
        .collect::<Vec<_>>();
    ChildPayoutResolution {
        changed: !attributions.is_empty(),
        attributions,
        corrupt_outputs: false,
    }
}

/// The payout attributions a stored child coinbase's outputs resolve to: empty for a source
/// with no payout format or no stored outputs, and `None` (warned) for corrupt outputs.
/// `id` is the event or observation id, for the warning.
pub(super) fn replay_child_payouts(
    id: i64,
    source_code: &str,
    outputs: Option<&[u8]>,
    identities: &PoolIdentityLookup,
) -> Option<Vec<EventPoolAttribution>> {
    let (Some(params), Some(outputs)) = (params_for_source_code(source_code), outputs) else {
        return Some(Vec::new());
    };
    match deserialize::<Vec<TxOut>>(outputs) {
        Ok(outputs) => Some(resolve_child_payout_attributions(
            &outputs,
            params,
            Some(identities),
        )),
        Err(err) => {
            warn!(
                id,
                source = source_code,
                error = %err,
                "skipping corrupt child_coinbase_outputs for child payout replay"
            );
            None
        }
    }
}

fn should_write_attribution(
    current: &ExistingSourceAttribution,
    resolved: Option<&EventPoolAttribution>,
    overwrite: bool,
) -> bool {
    let Some(resolved) = resolved else {
        return false;
    };
    if current.row_count == 0 {
        return true;
    }
    overwrite && !current.matches(resolved)
}

pub(super) fn plan_batch_updates(
    rows: &[tokio_postgres::Row],
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
    child_payout_identities: &PoolIdentityLookup,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Vec<ParentUpdateGroup> {
    let mut groups: Vec<ParentUpdateGroup> = Vec::new();
    let mut group_index: HashMap<Vec<u8>, usize> = HashMap::new();

    for row in rows {
        let candidate = CandidateRow {
            id: row.get(0),
            btc_parent_header_hash: row.get(1),
            source_code: row.get(2),
            parent_attribution: attribution_state_from_row(row, 3),
            child_attribution: attribution_state_from_row(row, 11),
            child_payout_attributions: ExistingAttributionSet::from_json(&row.get(19)),
            btc_parent_coinbase_script: row.get(20),
            btc_parent_coinbase_outputs: row.get(21),
            btc_parent_coinbase_outputs_text: row.get(22),
            child_coinbase_script: row.get(23),
            child_coinbase_outputs: row.get(24),
            event_confirmed_at: row.get(25),
        };

        let resolved_parent = resolve_parent_pool(&candidate, resolver, pool_ids_by_slug);
        if resolved_parent.corrupt_outputs {
            stats.corrupt_parent_outputs += 1;
        }
        let resolved_child = resolve_child_pool_attribution_from_script(
            candidate.child_coinbase_script.as_deref(),
            resolver,
            pool_ids_by_slug,
        );
        let resolved_child_payout =
            resolve_child_payouts(&candidate, child_payout_identities, config.overwrite);
        if resolved_child_payout.corrupt_outputs {
            stats.corrupt_child_outputs_skipped += 1;
        }

        let parent_source_changed = should_write_attribution(
            &candidate.parent_attribution,
            resolved_parent.attribution.as_ref(),
            config.overwrite,
        );
        let child_source_changed = should_write_attribution(
            &candidate.child_attribution,
            resolved_child.as_ref(),
            config.overwrite,
        );
        let child_payout_changed = resolved_child_payout.changed;
        let mut attributions = Vec::new();
        if parent_source_changed && let Some(attribution) = resolved_parent.attribution {
            attributions.push(attribution);
        }
        if child_source_changed && let Some(attribution) = resolved_child {
            attributions.push(attribution);
        }
        let child_payout_attributions = if child_payout_changed {
            resolved_child_payout.attributions
        } else {
            Vec::new()
        };

        if !parent_source_changed && !child_source_changed && !child_payout_changed {
            continue;
        }

        let idx = *group_index
            .entry(candidate.btc_parent_header_hash.clone())
            .or_insert_with(|| {
                groups.push(ParentUpdateGroup {
                    parent_hash: candidate.btc_parent_header_hash.clone(),
                    updates: Vec::new(),
                    parent_rollup_changed: false,
                });
                groups.len() - 1
            });
        let group = &mut groups[idx];
        group.updates.push(PlannedUpdate {
            event_id: candidate.id,
            parent_pool_changed: parent_source_changed,
            child_pool_changed: child_source_changed,
            child_payout_changed,
            attributions,
            child_payout_attributions,
            observed_at: candidate.event_confirmed_at,
        });
        group.parent_rollup_changed |= parent_source_changed;
    }

    groups
}

/// Decode `ExistingSourceAttribution` from eight consecutive columns starting at `offset`.
fn attribution_state_from_row(
    row: &tokio_postgres::Row,
    offset: usize,
) -> ExistingSourceAttribution {
    ExistingSourceAttribution {
        row_count: row.get(offset),
        namespace: row.get(offset + 1),
        match_kind: row.get(offset + 2),
        matched_value: row.get(offset + 3),
        pool_id: row.get(offset + 4),
        pool_identity_id: row.get(offset + 5),
        confidence: row.get(offset + 6),
        details: row.get(offset + 7),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mmm_capture::capture::{
        BTC_POOL_SNAPSHOT_SOURCE, PoolAttributionConfidence, PoolAttributionSide,
    };
    use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;

    fn test_attribution(pool_id: i64) -> EventPoolAttribution {
        EventPoolAttribution {
            side: PoolAttributionSide::BtcParent,
            namespace: "btc_coinbase_tag",
            match_kind: "coinbase_tag",
            matched_value: "/SpiderPool/".to_string(),
            pool_id: Some(pool_id),
            pool_identity_id: None,
            source: BTC_POOL_SNAPSHOT_SOURCE,
            confidence: PoolAttributionConfidence::High,
            details: serde_json::json!({}),
        }
    }

    fn existing_from(attribution: &EventPoolAttribution) -> ExistingSourceAttribution {
        ExistingSourceAttribution {
            row_count: 1,
            namespace: Some(attribution.namespace.to_string()),
            match_kind: Some(attribution.match_kind.to_string()),
            matched_value: Some(attribution.matched_value.clone()),
            pool_id: attribution.pool_id,
            pool_identity_id: attribution.pool_identity_id,
            confidence: Some(attribution.confidence.as_db_str().to_string()),
            details: Some(attribution.details.clone()),
        }
    }

    #[test]
    fn fill_null_only_sets_when_currently_null() {
        let resolved = test_attribution(5);
        assert!(should_write_attribution(
            &ExistingSourceAttribution::default(),
            Some(&resolved),
            false
        ));
    }

    #[test]
    fn fill_null_only_does_not_touch_existing() {
        let current = existing_from(&test_attribution(3));
        let resolved = test_attribution(5);
        assert!(!should_write_attribution(&current, Some(&resolved), false));
    }

    #[test]
    fn never_writes_null_over_existing() {
        let current = existing_from(&test_attribution(3));
        assert!(!should_write_attribution(&current, None, false));
        assert!(!should_write_attribution(&current, None, true));
    }

    #[test]
    fn overwrite_replaces_with_new_resolution() {
        let current = existing_from(&test_attribution(3));
        let resolved = test_attribution(5);
        assert!(should_write_attribution(&current, Some(&resolved), true));
    }

    #[test]
    fn overwrite_refreshes_same_pool_provenance() {
        let mut current = existing_from(&test_attribution(5));
        current.namespace = Some("btc_payout_address".to_string());
        current.match_kind = Some("payout_address".to_string());
        current.matched_value = Some("stale-address".to_string());
        let resolved = test_attribution(5);
        assert!(should_write_attribution(&current, Some(&resolved), true));
    }

    #[test]
    fn overwrite_is_noop_when_resolution_unchanged() {
        let resolved = test_attribution(5);
        let current = existing_from(&resolved);
        assert!(!should_write_attribution(&current, Some(&resolved), true));
    }

    #[test]
    fn overwrite_rewrites_duplicate_current_source_rows() {
        let resolved = test_attribution(5);
        let mut current = existing_from(&resolved);
        current.row_count = 2;
        assert!(should_write_attribution(&current, Some(&resolved), true));
    }

    #[test]
    fn null_stays_null_when_unresolved() {
        let current = ExistingSourceAttribution::default();
        assert!(!should_write_attribution(&current, None, false));
        assert!(!should_write_attribution(&current, None, true));
    }

    fn candidate_with_outputs(script: Option<Vec<u8>>, outputs: Option<Vec<u8>>) -> CandidateRow {
        CandidateRow {
            id: 42,
            btc_parent_header_hash: vec![0u8; 32],
            source_code: NAMECOIN_SOURCE_CODE.to_owned(),
            parent_attribution: ExistingSourceAttribution::default(),
            child_attribution: ExistingSourceAttribution::default(),
            child_payout_attributions: ExistingAttributionSet::default(),
            btc_parent_coinbase_script: script,
            btc_parent_coinbase_outputs: outputs,
            btc_parent_coinbase_outputs_text: None,
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            event_confirmed_at: 1_000,
        }
    }

    #[test]
    fn corrupt_parent_outputs_blob_is_skipped_not_aborting() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let pool_ids_by_slug: HashMap<String, i64> = HashMap::new();
        let corrupt_outputs = vec![0xffu8, 0xff, 0xff];
        let row = candidate_with_outputs(
            Some(b"\x03\xde\xad/no-such-pool-tag/".to_vec()),
            Some(corrupt_outputs),
        );

        let resolution = resolve_parent_pool(&row, &resolver, &pool_ids_by_slug);
        assert_eq!(resolution.attribution, None);
        assert!(resolution.corrupt_outputs);
    }

    #[test]
    fn payout_only_published_text_resolves_without_a_coinbase_script() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let mut pool_ids_by_slug = HashMap::new();
        pool_ids_by_slug.insert("1hash".to_owned(), 17);
        let mut row = candidate_with_outputs(None, None);
        row.btc_parent_coinbase_outputs_text =
            Some("1F1xcRt8H8Wa623KqmkEontwAAVqDSAWCV:5000000000".to_owned());

        let resolution = resolve_parent_pool(&row, &resolver, &pool_ids_by_slug);

        assert_eq!(
            resolution
                .attribution
                .as_ref()
                .and_then(|attribution| attribution.pool_id),
            Some(17)
        );
        assert!(!resolution.corrupt_outputs);
    }

    #[test]
    fn well_formed_unmatched_outputs_are_not_flagged_corrupt() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let pool_ids_by_slug: HashMap<String, i64> = HashMap::new();
        let empty_outputs = bitcoin::consensus::serialize(&Vec::<TxOut>::new());
        let row = candidate_with_outputs(
            Some(b"\x03\xde\xad/no-such-pool-tag/".to_vec()),
            Some(empty_outputs),
        );

        let resolution = resolve_parent_pool(&row, &resolver, &pool_ids_by_slug);
        assert_eq!(resolution.attribution, None);
        assert!(!resolution.corrupt_outputs);
    }

    #[test]
    fn missing_parent_script_still_validates_independent_outputs() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let pool_ids_by_slug: HashMap<String, i64> = HashMap::new();
        let row = candidate_with_outputs(None, Some(vec![0xffu8, 0xff, 0xff]));
        let resolution = resolve_parent_pool(&row, &resolver, &pool_ids_by_slug);
        assert_eq!(resolution.attribution, None);
        assert!(resolution.corrupt_outputs);
    }
}
