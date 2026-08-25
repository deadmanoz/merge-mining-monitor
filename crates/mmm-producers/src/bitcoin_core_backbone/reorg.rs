//! Bounded near-tip Bitcoin Core reorg detection and repair.
//!
//! Follow mode captures one active-chain view by walking backwards from a
//! pinned Core tip. If the local canonical rows contain a competing hash, this
//! module finds the last complete matching anchor before the first divergence,
//! fetches the whole replacement suffix before writing anything, and hands the
//! suffix to the read model for one atomic chain switch. Ordinary gaps without
//! a competing hash keep using the existing missing-only repair path.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use serde_json::{Value, json};
use tokio_postgres::Client;

#[cfg(any(test, feature = "db-integration"))]
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;
use mmm_read_model::{
    CoreCanonicalReplacement, ExpectedCoreCanonicalRow, drain_core_reconcile_queue,
    replace_core_canonical_suffix_validated,
};
#[cfg(any(test, feature = "db-integration"))]
use mmm_store::get_source_id;

use super::{
    BackboneIntegrityError, BackboneIntegrityFailure, BitcoinCoreBackboneSource,
    BitcoinCoreBackboneTip, BitcoinCoreSyncStats, REORG_REPAIR_ERROR_CODE, integrity_error,
    live_backbone_window_start_height, load_or_init_sync_state, repair_near_tip_gaps_to_target,
    update_sync_error,
};

/// Finite test seam for exercising follow-mode near-tip repair without
/// starting the process-lifetime live loop.
#[cfg(any(test, feature = "db-integration"))]
pub async fn repair_near_tip_backbone_for_test<S>(
    client: &mut Client,
    source: &S,
    target: BitcoinCoreBackboneTip,
    delay: Duration,
    window_heights: i32,
) -> Result<BitcoinCoreSyncStats>
where
    S: BitcoinCoreBackboneSource,
{
    let source_id = get_source_id(client, BITCOIN_SOURCE_CODE).await?;
    repair_near_tip_backbone_to_target(
        client,
        source,
        source_id,
        target,
        delay,
        window_heights,
        &ConfiguredParentClassifier::Disabled,
    )
    .await
}

#[derive(Debug, Clone)]
struct CoreViewRow {
    height: i32,
    hash: Vec<u8>,
    header: Header,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LocalCanonicalRow {
    height: i32,
    hash: Vec<u8>,
    prev_hash: Vec<u8>,
    coinbase_status: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepairPlan {
    NoHashConflict,
    ReplaceFrom { first_replacement_index: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepairPlanError {
    NoMatchingAnchor {
        first_divergence_height: i32,
        first_conflict_height: i32,
        view_start: i32,
        view_end: i32,
    },
}

/// Repair the captured near-tip target without allowing the ordinary
/// contiguous guard to encounter a known fork first. A conflicting suffix is
/// switched atomically; a window containing only gaps or incomplete matching
/// rows uses the existing missing-only fill so fresh and sparse databases keep
/// their established behavior.
pub(crate) async fn repair_near_tip_backbone_to_target<S>(
    client: &mut Client,
    source: &S,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
    delay: Duration,
    window_heights: i32,
    classifier: &ConfiguredParentClassifier,
) -> Result<BitcoinCoreSyncStats>
where
    S: BitcoinCoreBackboneSource,
{
    let mut may_replan_after_gap_conflict = true;
    loop {
        let state = load_or_init_sync_state(client, source_id).await?;
        validate_target_bounds(
            client,
            source_id,
            target,
            state.contiguous_complete_height,
            state.target_tip_height,
        )
        .await?;

        let core_view = capture_core_view(source, target, window_heights).await?;
        verify_target_stable(client, source, source_id, target).await?;
        let view_start = core_view
            .first()
            .expect("captured Core view always contains its target")
            .height;
        verify_cursor_before_view(
            client,
            source,
            source_id,
            target,
            state.contiguous_complete_height,
            view_start,
        )
        .await?;
        let local_rows = load_local_canonical_rows(client, view_start, target.height).await?;

        let plan = match plan_repair(&core_view, &local_rows) {
            Ok(plan) => plan,
            Err(err) => {
                return record_plan_error(client, source_id, target, err).await;
            }
        };
        let RepairPlan::ReplaceFrom {
            first_replacement_index,
        } = plan
        else {
            let gap_result =
                repair_near_tip_gaps_to_target(client, source, target, delay, window_heights).await;
            if may_replan_after_gap_conflict && is_height_conflict(&gap_result) {
                // The gap writer takes the exclusive canonical-view barrier. A
                // classifier that was still committing during the unlocked scan
                // can therefore become visible only after this plan was made.
                // Reload every planning input once so the committed conflict is
                // routed through the bounded suffix replacement path.
                may_replan_after_gap_conflict = false;
                continue;
            }
            return gap_result;
        };

        let common_ancestor_height = core_view[first_replacement_index - 1].height;
        let replacement_view = &core_view[first_replacement_index..];
        let replacements = fetch_replacements(source, replacement_view, delay).await?;

        // Coinbase acquisition can take long enough for another fork to occur.
        // Confirm the captured target is still active immediately before the first
        // database mutation.
        verify_target_stable(client, source, source_id, target).await?;
        let expected_local = expected_local_rows(&local_rows);
        let replace_result = replace_core_canonical_suffix_validated(
            client,
            source_id,
            state.contiguous_complete_height,
            common_ancestor_height,
            &expected_local,
            &replacements,
            (
                async |_txn| {
                    if let Some(failure) = target_stability_failure(source, target).await? {
                        return Err(failure.into_error());
                    }
                    Ok(())
                },
                async |_txn| {
                    if let Some(failure) = target_stability_failure(source, target).await? {
                        return Err(failure.into_error());
                    }
                    Ok(())
                },
            ),
        )
        .await;
        if let Err(err) = &replace_result
            && let Some(failure) = err.downcast_ref::<BackboneIntegrityFailure>()
        {
            failure.persist(client, source_id).await?;
        }
        replace_result.context("atomically replace Bitcoin Core canonical near-tip suffix")?;

        // The suffix switch persists its changed-parent seeds before committing.
        // Drain immediately for the normal path; follow startup and every later
        // tick also replay this queue after a crash or transient cascade failure.
        drain_core_reconcile_queue(client, source_id, classifier)
            .await
            .context("reconcile dependents after Bitcoin Core canonical suffix replacement")?;

        let replacement_stats = BitcoinCoreSyncStats {
            attempted: replacements.len(),
            completed: replacements.len(),
            skipped_complete: 0,
            coinbase_failed: 0,
        };
        let gap_stats =
            repair_near_tip_gaps_to_target(client, source, target, delay, window_heights)
                .await
                .context("fill gaps outside the replaced Bitcoin Core suffix")?;
        return Ok(add_stats(replacement_stats, gap_stats));
    }
}

fn is_height_conflict(result: &Result<BitcoinCoreSyncStats>) -> bool {
    result
        .as_ref()
        .err()
        .and_then(|err| err.downcast_ref::<BackboneIntegrityError>())
        == Some(&BackboneIntegrityError::HeightConflict)
}

fn expected_local_rows(rows: &[LocalCanonicalRow]) -> Vec<ExpectedCoreCanonicalRow> {
    rows.iter()
        .map(|row| ExpectedCoreCanonicalRow {
            height: row.height,
            hash: row.hash.clone(),
            prev_hash: row.prev_hash.clone(),
        })
        .collect()
}

fn add_stats(left: BitcoinCoreSyncStats, right: BitcoinCoreSyncStats) -> BitcoinCoreSyncStats {
    BitcoinCoreSyncStats {
        attempted: left.attempted + right.attempted,
        completed: left.completed + right.completed,
        skipped_complete: left.skipped_complete + right.skipped_complete,
        coinbase_failed: left.coinbase_failed + right.coinbase_failed,
    }
}

/// A fork whose stale contiguous cursor has already fallen below the captured
/// repair view must not be mistaken for an ordinary empty near-tip window.
/// Compare that proven local cursor directly with Core before any gap fill can
/// mutate newer rows, and report an explicit out-of-window repair failure when
/// the hashes differ.
async fn verify_cursor_before_view<S>(
    client: &Client,
    source: &S,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
    contiguous_complete_height: i32,
    view_start: i32,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    if contiguous_complete_height < 0 || contiguous_complete_height >= view_start {
        return Ok(());
    }

    let rows = load_local_canonical_rows(
        client,
        contiguous_complete_height,
        contiguous_complete_height,
    )
    .await?;
    let [local] = rows.as_slice() else {
        return reorg_integrity_failure(
            client,
            source_id,
            contiguous_complete_height,
            format!(
                "Bitcoin Core contiguous cursor {} has {} canonical rows",
                contiguous_complete_height,
                rows.len()
            ),
            json!({
                "reason": "contiguous_cursor_row_invalid",
                "contiguous_complete_height": contiguous_complete_height,
                "canonical_row_count": rows.len(),
                "captured_view_start": view_start,
                "target_tip_height": target.height,
                "target_tip_hash": target.hash.to_string(),
            }),
        )
        .await;
    };
    if local.coinbase_status != "complete" {
        return reorg_integrity_failure(
            client,
            source_id,
            contiguous_complete_height,
            format!(
                "Bitcoin Core contiguous cursor {} is not coinbase-complete",
                contiguous_complete_height
            ),
            json!({
                "reason": "contiguous_cursor_row_incomplete",
                "contiguous_complete_height": contiguous_complete_height,
                "coinbase_status": local.coinbase_status,
                "captured_view_start": view_start,
                "target_tip_height": target.height,
                "target_tip_hash": target.hash.to_string(),
            }),
        )
        .await;
    }

    let active_hash = source
        .block_hash(contiguous_complete_height)
        .await
        .with_context(|| {
            format!(
                "fetch Bitcoin Core active hash at out-of-window contiguous cursor {}",
                contiguous_complete_height
            )
        })?;
    if local.hash == active_hash.to_byte_array() {
        return Ok(());
    }

    reorg_integrity_failure(
        client,
        source_id,
        contiguous_complete_height,
        format!(
            "Bitcoin Core contiguous cursor divergence at height {} lies below bounded view start {}",
            contiguous_complete_height, view_start
        ),
        json!({
            "reason": "common_ancestor_outside_window",
            "first_conflict_height": contiguous_complete_height,
            "local_cursor_hash": hex::encode(&local.hash),
            "current_core_hash": active_hash.to_string(),
            "captured_view_start": view_start,
            "target_tip_height": target.height,
            "target_tip_hash": target.hash.to_string(),
            "operator_action": "stop serving, back up PostgreSQL, and run the documented deep-reorg recovery workflow",
        }),
    )
    .await
}

async fn validate_target_bounds(
    client: &Client,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
    contiguous_complete_height: i32,
    persisted_target_height: Option<i32>,
) -> Result<()> {
    if target.height < contiguous_complete_height {
        return reorg_integrity_failure(
            client,
            source_id,
            target.height,
            format!(
                "Bitcoin Core tip {} is below the proven contiguous cursor {}",
                target.height, contiguous_complete_height
            ),
            json!({
                "reason": "target_below_contiguous_cursor",
                "current_tip_height": target.height,
                "current_tip_hash": target.hash.to_string(),
                "contiguous_complete_height": contiguous_complete_height,
            }),
        )
        .await;
    }
    if persisted_target_height.is_some_and(|previous| previous > target.height) {
        return reorg_integrity_failure(
            client,
            source_id,
            target.height,
            format!(
                "Bitcoin Core tip regressed below the persisted target: current={} persisted={}",
                target.height,
                persisted_target_height.expect("checked as present")
            ),
            json!({
                "reason": "target_height_regressed",
                "current_tip_height": target.height,
                "persisted_target_height": persisted_target_height,
                "current_tip_hash": target.hash.to_string(),
            }),
        )
        .await;
    }
    Ok(())
}

async fn fetch_replacements<S>(
    source: &S,
    replacement_view: &[CoreViewRow],
    delay: Duration,
) -> Result<Vec<CoreCanonicalReplacement>>
where
    S: BitcoinCoreBackboneSource,
{
    let mut replacements = Vec::with_capacity(replacement_view.len());
    for (index, row) in replacement_view.iter().enumerate() {
        let coinbase = source
            .block_coinbase(row.header.block_hash())
            .await
            .with_context(|| {
                format!(
                    "fetch Bitcoin Core replacement coinbase at height {} ({})",
                    row.height,
                    row.header.block_hash()
                )
            })?;
        replacements.push(CoreCanonicalReplacement {
            height: row.height,
            header: row.header,
            coinbase,
        });
        if index + 1 < replacement_view.len() && !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
    Ok(replacements)
}

/// Capture `window_heights` repairable heights plus one anchor by starting at
/// the pinned target hash and following each header's `prev_blockhash`. This
/// avoids mixing independent per-height active-chain lookups into one view.
async fn capture_core_view<S>(
    source: &S,
    target: BitcoinCoreBackboneTip,
    window_heights: i32,
) -> Result<Vec<CoreViewRow>>
where
    S: BitcoinCoreBackboneSource,
{
    let view_start = core_view_start_height(target.height, window_heights);
    let mut descending = Vec::with_capacity((target.height - view_start + 1) as usize);
    let mut requested_hash = target.hash;
    for height in (view_start..=target.height).rev() {
        let header = source.block_header(requested_hash).await.with_context(|| {
            format!("fetch Bitcoin Core active-chain header at height {height} ({requested_hash})")
        })?;
        let actual_hash = header.block_hash();
        if actual_hash != requested_hash {
            anyhow::bail!(
                "Bitcoin Core returned header {} while capturing requested hash {} at height {}",
                actual_hash,
                requested_hash,
                height
            );
        }
        descending.push(CoreViewRow {
            height,
            hash: actual_hash.to_byte_array().to_vec(),
            header,
        });
        requested_hash = header.prev_blockhash;
    }
    descending.reverse();
    Ok(descending)
}

fn core_view_start_height(tip_height: i32, window_heights: i32) -> i32 {
    live_backbone_window_start_height(tip_height, window_heights)
        .saturating_sub(1)
        .max(0)
}

/// Confirm the captured target remains on Core's active chain. Advancing beyond
/// it is harmless, but regressing below it or changing its active hash means the
/// captured view cannot be applied safely.
async fn verify_target_stable<S>(
    client: &Client,
    source: &S,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
) -> Result<()>
where
    S: BitcoinCoreBackboneSource,
{
    let Some(failure) = target_stability_failure(source, target).await? else {
        return Ok(());
    };
    failure.persist(client, source_id).await?;
    Err(failure.into_error())
}

pub(super) async fn target_stability_failure<S>(
    source: &S,
    target: BitcoinCoreBackboneTip,
) -> Result<Option<BackboneIntegrityFailure>>
where
    S: BitcoinCoreBackboneSource,
{
    let current_tip = source
        .tip()
        .await
        .context("recheck Bitcoin Core tip during near-tip reorg repair")?;
    if current_tip.height < target.height {
        return Ok(Some(BackboneIntegrityFailure::new(
            BackboneIntegrityError::LiveWindowInvariant,
            REORG_REPAIR_ERROR_CODE,
            target.height,
            format!(
                "Bitcoin Core tip regressed during near-tip repair: captured={} current={}",
                target.height, current_tip.height
            ),
            json!({
                "reason": "target_height_regressed_during_capture",
                "captured_tip_height": target.height,
                "captured_tip_hash": target.hash.to_string(),
                "current_tip_height": current_tip.height,
                "current_tip_hash": current_tip.hash.to_string(),
            }),
        )));
    }
    let current_target_hash = if current_tip.height == target.height {
        current_tip.hash
    } else {
        source.block_hash(target.height).await.with_context(|| {
            format!(
                "recheck Bitcoin Core active hash at captured height {}",
                target.height
            )
        })?
    };
    if current_target_hash == target.hash {
        return Ok(None);
    }
    Ok(Some(BackboneIntegrityFailure::new(
        BackboneIntegrityError::LiveWindowInvariant,
        REORG_REPAIR_ERROR_CODE,
        target.height,
        format!(
            "Bitcoin Core target moved during near-tip repair at height {}: captured={} current={}",
            target.height, target.hash, current_target_hash
        ),
        json!({
            "reason": "target_hash_moved_during_capture",
            "height": target.height,
            "captured_tip_hash": target.hash.to_string(),
            "current_core_hash": current_target_hash.to_string(),
        }),
    )))
}

async fn load_local_canonical_rows(
    client: &Client,
    from_height: i32,
    to_height: i32,
) -> Result<Vec<LocalCanonicalRow>> {
    let rows = client
        .query(
            "SELECT btc_height, btc_header_hash, btc_prev_header_hash, btc_coinbase_status \
             FROM block \
             WHERE kind = 'canonical' \
               AND btc_height BETWEEN $1 AND $2 \
             ORDER BY btc_height, btc_header_hash",
            &[&from_height, &to_height],
        )
        .await
        .with_context(|| {
            format!("load local canonical rows for Core view {from_height}..={to_height}")
        })?;
    Ok(rows
        .into_iter()
        .map(|row| LocalCanonicalRow {
            height: row
                .get::<_, Option<i32>>(0)
                .expect("canonical rows have heights"),
            hash: row.get(1),
            prev_hash: row.get(2),
            coinbase_status: row.get(3),
        })
        .collect())
}

fn plan_repair(
    core_view: &[CoreViewRow],
    local_rows: &[LocalCanonicalRow],
) -> std::result::Result<RepairPlan, RepairPlanError> {
    let mut local_by_height: BTreeMap<i32, Vec<&LocalCanonicalRow>> = BTreeMap::new();
    for row in local_rows {
        local_by_height.entry(row.height).or_default().push(row);
    }

    // A non-Core canonical row is a repairable conflict even when another row
    // at the same height already matches Core. That transient duplicate is the
    // natural ordering when an AuxPoW reconcile promotes the new active parent
    // before the exclusive suffix writer demotes the displaced parent.
    let first_conflict_index = core_view.iter().position(|core| {
        local_by_height
            .get(&core.height)
            .is_some_and(|rows| rows.iter().any(|row| row.hash != core.hash))
    });
    let Some(first_conflict_index) = first_conflict_index else {
        return Ok(RepairPlan::NoHashConflict);
    };
    let first_conflict_height = core_view[first_conflict_index].height;

    // Missing or incomplete rows below the conflict do not prove divergence.
    // Walk back from the first conflicting hash to the nearest unique, complete
    // local row that still matches the pinned Core view. This is the actual
    // bounded common ancestor; every later row remains in the replacement
    // suffix even if its hash happens to match again (the production 963854
    // topology). A duplicate height cannot serve as the immutable anchor because
    // one of its rows still needs to be demoted by the replacement transaction.
    let matching_anchor_index = (0..first_conflict_index).rev().find(|index| {
        let core = &core_view[*index];
        local_by_height.get(&core.height).is_some_and(|rows| {
            rows.len() == 1 && rows[0].hash == core.hash && rows[0].coinbase_status == "complete"
        })
    });
    let Some(matching_anchor_index) = matching_anchor_index else {
        let first_divergence_height = core_view
            .iter()
            .take(first_conflict_index + 1)
            .find(|core| {
                !local_by_height.get(&core.height).is_some_and(|rows| {
                    rows.len() == 1
                        && rows[0].hash == core.hash
                        && rows[0].coinbase_status == "complete"
                })
            })
            .map_or(core_view[0].height, |core| core.height);
        return Err(RepairPlanError::NoMatchingAnchor {
            first_divergence_height,
            first_conflict_height,
            view_start: core_view[0].height,
            view_end: core_view.last().expect("non-empty Core view").height,
        });
    };
    let first_replacement_index = matching_anchor_index + 1;
    debug_assert!(
        core_view[first_replacement_index..]
            .iter()
            .any(|core| core.height == first_conflict_height)
    );
    Ok(RepairPlan::ReplaceFrom {
        first_replacement_index,
    })
}

async fn record_plan_error<T>(
    client: &Client,
    source_id: i64,
    target: BitcoinCoreBackboneTip,
    err: RepairPlanError,
) -> Result<T> {
    match err {
        RepairPlanError::NoMatchingAnchor {
            first_divergence_height,
            first_conflict_height,
            view_start,
            view_end,
        } => {
            reorg_integrity_failure(
                client,
                source_id,
                first_conflict_height,
                format!(
                    "Bitcoin Core canonical divergence at height {first_conflict_height} has no complete matching anchor in bounded view {view_start}..={view_end}"
                ),
                json!({
                    "reason": "common_ancestor_outside_window",
                    "first_divergence_height": first_divergence_height,
                    "first_conflict_height": first_conflict_height,
                    "view_start": view_start,
                    "view_end": view_end,
                    "target_tip_hash": target.hash.to_string(),
                    "operator_action": "stop serving, back up PostgreSQL, and run the documented deep-reorg recovery workflow",
                }),
            )
            .await
        }
    }
}

async fn reorg_integrity_failure<T>(
    client: &Client,
    source_id: i64,
    height: i32,
    message: String,
    details: Value,
) -> Result<T> {
    update_sync_error(
        client,
        source_id,
        height,
        REORG_REPAIR_ERROR_CODE,
        &message,
        details,
    )
    .await?;
    Err(integrity_error(
        BackboneIntegrityError::LiveWindowInvariant,
        message,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::block::Version;
    use bitcoin::hash_types::TxMerkleNode;
    use bitcoin::{BlockHash, CompactTarget};

    fn hash(byte: u8) -> Vec<u8> {
        vec![byte; 32]
    }

    fn core(height: i32, byte: u8) -> CoreViewRow {
        CoreViewRow {
            height,
            hash: hash(byte),
            header: Header {
                version: Version::ONE,
                prev_blockhash: BlockHash::from_byte_array([byte.saturating_sub(1); 32]),
                merkle_root: TxMerkleNode::all_zeros(),
                time: byte as u32,
                bits: CompactTarget::from_consensus(0x1d00ffff),
                nonce: byte as u32,
            },
        }
    }

    fn local(height: i32, byte: u8) -> LocalCanonicalRow {
        LocalCanonicalRow {
            height,
            hash: hash(byte),
            prev_hash: hash(byte.saturating_sub(1)),
            coinbase_status: "complete".to_owned(),
        }
    }

    #[test]
    fn plans_depth_one_suffix_replacement() {
        let core = vec![core(100, 1), core(101, 2), core(102, 3), core(103, 4)];
        let local = vec![local(100, 1), local(101, 2), local(102, 3), local(103, 9)];
        assert_eq!(
            plan_repair(&core, &local),
            Ok(RepairPlan::ReplaceFrom {
                first_replacement_index: 3
            })
        );
    }

    #[test]
    fn plans_multi_block_suffix_replacement() {
        let core = vec![core(200, 1), core(201, 2), core(202, 3), core(203, 4)];
        let local = vec![local(200, 1), local(201, 8), local(202, 9), local(203, 10)];
        assert_eq!(
            plan_repair(&core, &local),
            Ok(RepairPlan::ReplaceFrom {
                first_replacement_index: 1
            })
        );
    }

    #[test]
    fn accepts_window_depth_and_rejects_window_plus_one() {
        // Four repairable heights plus the extra anchor at index zero.
        let core = vec![
            core(300, 1),
            core(301, 2),
            core(302, 3),
            core(303, 4),
            core(304, 5),
        ];
        let depth_four = vec![
            local(300, 1),
            local(301, 7),
            local(302, 8),
            local(303, 9),
            local(304, 10),
        ];
        assert_eq!(
            plan_repair(&core, &depth_four),
            Ok(RepairPlan::ReplaceFrom {
                first_replacement_index: 1
            })
        );

        let depth_five = vec![
            local(300, 6),
            local(301, 7),
            local(302, 8),
            local(303, 9),
            local(304, 10),
        ];
        assert!(matches!(
            plan_repair(&core, &depth_five),
            Err(RepairPlanError::NoMatchingAnchor { .. })
        ));
    }

    #[test]
    fn later_matching_hash_remains_in_replacement_suffix() {
        let core = vec![core(963_852, 1), core(963_853, 2), core(963_854, 3)];
        let local = vec![local(963_852, 1), local(963_853, 9), local(963_854, 3)];
        assert_eq!(
            plan_repair(&core, &local),
            Ok(RepairPlan::ReplaceFrom {
                first_replacement_index: 1
            }),
            "the later matching 963854 row is still part of the 963853..=963854 replacement"
        );
    }

    #[test]
    fn uses_later_matching_anchor_when_extra_anchor_is_missing() {
        let core = vec![
            core(400, 1),
            core(401, 2),
            core(402, 3),
            core(403, 4),
            core(404, 5),
        ];
        let local = vec![local(401, 2), local(402, 3), local(403, 9)];
        assert_eq!(
            plan_repair(&core, &local),
            Ok(RepairPlan::ReplaceFrom {
                first_replacement_index: 3
            }),
            "the complete matching row at 402 anchors the conflicting suffix"
        );
    }

    #[test]
    fn capture_view_anchor_never_falls_below_genesis() {
        assert_eq!(core_view_start_height(2, 4), 0);
        assert_eq!(core_view_start_height(10, 4), 6);
    }
}
