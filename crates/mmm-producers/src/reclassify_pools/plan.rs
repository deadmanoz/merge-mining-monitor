//! Pure planning layer for `reclassify-pools`: decode one keyset page of
//! candidate rows and resolve, per event, which source-scoped attribution rows
//! should be written under fill-missing-only / overwrite rules. No DB writes
//! happen here. The driver in the parent module pairs each parent group this
//! produces with that parent's read-model reconcile inside one transaction.

use std::collections::HashMap;

use bitcoin::consensus::deserialize;
use serde_json::Value;
use tracing::warn;

use mmm_capture::attribution_policy::{ExistingAttributionSet, WritePolicy};
use mmm_capture::auxpow::TxOut;
use mmm_capture::capture::{
    EventPoolAttribution, resolve_child_payout_attributions,
    resolve_parent_pool_attribution_from_serialized_coinbase_outputs,
};
use mmm_capture::child_payout::{PoolIdentityLookup, params_for_source_code};
use mmm_capture::pool_resolver::PoolResolver;

use super::{ReclassifyPoolsConfig, ReclassifyPoolsStats};

/// One scanned event decoded from a candidate-batch row: the stored raw
/// coinbase evidence plus the current source-scoped attribution state for each
/// side. Resolution is pure over these fields; nothing here triggers a DB write.
#[derive(Debug)]
struct CandidateRow {
    id: i64,
    /// Parent BTC header hash bytes (wire/internal order, as stored). Used as
    /// the grouping key so all an event's writes share their parent's reconcile.
    btc_parent_header_hash: Vec<u8>,
    source_code: String,
    /// Current BTC-parent attribution row state for the snapshot source.
    parent_attribution: ExistingSourceAttribution,
    /// Current child-side attribution row state for the legacy child-script source.
    child_attribution: ExistingSourceAttribution,
    /// Existing child payout attribution rows (chain-native payout/reward
    /// namespaces), used by `should_write` to honor fill-missing-only vs overwrite.
    child_payout_attributions: ExistingAttributionSet,
    btc_parent_coinbase_script: Option<Vec<u8>>,
    btc_parent_coinbase_outputs: Option<Vec<u8>>,
    child_coinbase_script: Option<Vec<u8>>,
    child_coinbase_outputs: Option<Vec<u8>>,
    /// Event confirmation timestamp, reused as the `observed_at` of any rows
    /// this pass writes so provenance timestamps stay tied to the evidence.
    event_confirmed_at: i64,
}

/// Snapshot of the single source-scoped attribution row currently present for
/// one side of one event. `row_count` distinguishes 0 (absent), 1 (the unique
/// row these `Option` fields describe), and >1 (a duplicate that overwrite
/// always rewrites). When `row_count != 1` the descriptive fields are NULL.
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
    /// True only when exactly one row exists and its full provenance tuple
    /// (namespace, match kind, matched value, pool id, identity id, confidence,
    /// details) equals the freshly resolved attribution. Compares the whole
    /// tuple, not just `pool_id`, so a provenance-only change still counts as a
    /// difference and lets overwrite refresh it.
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

/// One planned attribution update for an event. The `*_changed` flags drive the
/// transactional delete-then-upsert in `apply_parent_group`: each side is only
/// touched when its flag is set, so an unchanged side is never rewritten.
#[derive(Debug)]
pub(super) struct PlannedUpdate {
    pub(super) event_id: i64,
    /// Parent BTC pool attribution differs and should be (re)written; this also
    /// gates whether the parent's read model is reconciled.
    pub(super) parent_pool_changed: bool,
    /// Legacy child-script pool attribution differs and should be (re)written.
    pub(super) child_pool_changed: bool,
    /// Chain-native child payout attribution rows differ and should be written.
    pub(super) child_payout_changed: bool,
    /// Parent + legacy-child-script attributions to upsert (with stale cleanup).
    pub(super) attributions: Vec<EventPoolAttribution>,
    /// Child payout attributions to upsert WITHOUT stale cleanup (multiple
    /// payout addresses per event are legitimate, so siblings must survive).
    pub(super) child_payout_attributions: Vec<EventPoolAttribution>,
    /// Provenance timestamp for written rows, sourced from the event's
    /// `confirmed_at`.
    pub(super) observed_at: i64,
}

/// All planned updates for one parent header hash in a batch, plus whether any
/// of them changed parent-side attribution. A changed parent-side pool match
/// means the pool snapshot may also change the read model's Bitcoin-coinbase
/// miner resolution for this parent, so the parent is reconciled again.
#[derive(Debug)]
pub(super) struct ParentUpdateGroup {
    pub(super) parent_hash: Vec<u8>,
    pub(super) updates: Vec<PlannedUpdate>,
    pub(super) parent_rollup_changed: bool,
}

/// Outcome of parent-pool re-resolution for one row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParentResolution {
    attribution: Option<EventPoolAttribution>,
    /// True when the stored `btc_parent_coinbase_outputs` blob failed to
    /// deserialize and the address fallback was skipped for this row.
    corrupt_outputs: bool,
}

/// Resolve the parent BTC pool attribution for one row from its stored coinbase
/// script (tags) and, failing that, its stored coinbase outputs (payout
/// addresses).
/// Address derivation reuses the live parser's `output_addresses` so historical
/// and capture-time attribution agree.
///
/// Best-effort: a corrupt or schema-legacy `btc_parent_coinbase_outputs` blob
/// does NOT abort the whole offline pass. The address fallback is skipped for
/// that row (it stays unresolved, which replay handles safely by never writing
/// NULL) and is flagged so the caller can count/log it.
fn resolve_parent_pool(
    row: &CandidateRow,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> ParentResolution {
    let Some(script) = &row.btc_parent_coinbase_script else {
        return ParentResolution::default();
    };

    match resolve_parent_pool_attribution_from_serialized_coinbase_outputs(
        script,
        row.btc_parent_coinbase_outputs.as_deref(),
        resolver,
        pool_ids_by_slug,
    ) {
        Ok(attribution) => ParentResolution {
            attribution,
            corrupt_outputs: false,
        },
        Err(err) => {
            warn!(
                event_id = row.id,
                error = %err,
                "skipping corrupt btc_parent_coinbase_outputs for address fallback; \
                 leaving pool attribution unresolved for this row"
            );
            ParentResolution {
                attribution: None,
                corrupt_outputs: true,
            }
        }
    }
}

/// Resolve the child pool attribution for one row from its stored child
/// coinbase script (tags only). In this replay, child attribution is
/// coinbase-tag-only for Namecoin-family chains; chain-native payout/reward
/// addresses are handled separately by `resolve_child_payouts`. Yields an
/// attribution only when the matched pool slug maps to a seeded `pool_id`.
fn resolve_child_pool(
    row: &CandidateRow,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> Option<EventPoolAttribution> {
    let script = row.child_coinbase_script.as_ref()?;
    let pool_match = resolver.resolve_coinbase_script(script)?;
    let pool_id = pool_ids_by_slug.get(&pool_match.pool.slug).copied();
    pool_id.map(|id| EventPoolAttribution::from_legacy_child_pool_match(&pool_match, id))
}

/// Result of payout re-resolution for one event. `attributions` holds only the
/// rows the write policy says to write (may include unresolved NULL-pool
/// observations); `changed` is true iff that vector is non-empty; `corrupt_outputs`
/// flags a blob that failed consensus decode so the caller can count it.
struct ChildPayoutResolution {
    attributions: Vec<EventPoolAttribution>,
    changed: bool,
    corrupt_outputs: bool,
}

/// Re-resolve child payout attributions from stored coinbase outputs. Returns
/// unchanged when the source has no payout params or the row has no outputs
/// bytes. A corrupt blob is flagged and skipped, not fatal.
fn resolve_child_payouts(
    row: &CandidateRow,
    identities: &PoolIdentityLookup,
    overwrite: bool,
) -> ChildPayoutResolution {
    let Some(params) = params_for_source_code(&row.source_code) else {
        return unchanged_child_payout();
    };
    let Some(outputs_bytes) = row.child_coinbase_outputs.as_deref() else {
        return unchanged_child_payout();
    };

    let outputs: Vec<TxOut> = match deserialize(outputs_bytes) {
        Ok(outputs) => outputs,
        Err(err) => {
            warn!(
                event_id = row.id,
                source = row.source_code,
                error = %err,
                "skipping corrupt child_coinbase_outputs for child payout replay; \
                 leaving child payout attribution unchanged for this row"
            );
            return ChildPayoutResolution {
                attributions: Vec::new(),
                changed: false,
                corrupt_outputs: true,
            };
        }
    };

    let attributions = resolve_child_payout_attributions(&outputs, params, Some(identities))
        .into_iter()
        .filter(|attribution| {
            row.child_payout_attributions
                .should_write(attribution, WritePolicy::ChildPayout { overwrite })
        })
        .collect::<Vec<_>>();
    let changed = !attributions.is_empty();

    ChildPayoutResolution {
        attributions,
        changed,
        corrupt_outputs: false,
    }
}

fn unchanged_child_payout() -> ChildPayoutResolution {
    ChildPayoutResolution {
        attributions: Vec::new(),
        changed: false,
        corrupt_outputs: false,
    }
}

/// Decide whether a source-scoped attribution row should be written under
/// fill-missing-only / overwrite rules. The decision compares the full
/// provenance tuple, not just `pool_id`, because the API exposes namespace,
/// match kind, matched value, source, confidence, and identity linkage.
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

/// Plan one batch's updates, grouped by parent header hash in first-seen
/// order. Resolution is pure (no DB writes); a corrupt blob only adjusts the
/// counter, and nothing commits until each parent's transaction.
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
            child_coinbase_script: row.get(22),
            child_coinbase_outputs: row.get(23),
            event_confirmed_at: row.get(24),
        };

        let resolved_parent = resolve_parent_pool(&candidate, resolver, pool_ids_by_slug);
        if resolved_parent.corrupt_outputs {
            stats.corrupt_outputs_skipped += 1;
        }
        let resolved_child = resolve_child_pool(&candidate, resolver, pool_ids_by_slug);
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

/// Decode an `ExistingSourceAttribution` from the eight consecutive columns
/// starting at `offset`. The offsets (3 for parent, 11 for child) must stay in
/// lockstep with the candidate-batch projection order.
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
        // Unresolved coinbase: leave the existing ID untouched.
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
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            event_confirmed_at: 1_000,
        }
    }

    #[test]
    fn corrupt_parent_outputs_blob_is_skipped_not_aborting() {
        // A non-matching coinbase script forces the address fallback, and a
        // corrupt outputs blob (a varint claiming outputs with no data) fails
        // consensus deserialize. Best-effort: the row resolves to None and is
        // flagged so the caller counts it; the pass must NOT abort.
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
    fn well_formed_unmatched_outputs_are_not_flagged_corrupt() {
        // A valid (empty) outputs blob that resolves to no pool is unresolved
        // but NOT corrupt, so it must not inflate `corrupt_outputs_skipped`.
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
    fn missing_parent_script_resolves_to_none_without_touching_outputs() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let pool_ids_by_slug: HashMap<String, i64> = HashMap::new();
        // No coinbase script at all: nothing to resolve, outputs are ignored.
        let row = candidate_with_outputs(None, Some(vec![0xffu8, 0xff, 0xff]));
        let resolution = resolve_parent_pool(&row, &resolver, &pool_ids_by_slug);
        assert_eq!(resolution, ParentResolution::default());
    }
}
