//! Bitcoin Core backbone coverage for the tree and stale-navigation projections.
//!
//! Loads the canonical block coverage for a set of height windows and answers two
//! questions over it: is a window a clean single-block-per-height complete-coinbase
//! run (`window_is_complete`, used to pick a renderable navigation window), and
//! does a requested tree window have a synced, conflict-free backbone
//! (`ensure_window`, which raises the `backbone_unsynced` / `backbone_conflict`
//! errors). The max-complete-canonical-height ceiling lives here too.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::json;
use tokio_postgres::Client;

use super::display_hash;
use super::split_height_windows;
use crate::error::ApiError;
use crate::projection::ProjectionError;

/// Max canonical Bitcoin height whose coinbase is fully captured
/// (btc_coinbase_status='complete'). The tip ceiling for tree/branch windows:
/// rows above it are not yet backbone-complete, so windows are clamped to it.
pub(crate) async fn load_max_complete_canonical_height(client: &Client) -> Result<Option<i32>> {
    let row = client
        .query_opt(
            "SELECT max(btc_height) \
             FROM block \
             WHERE kind = 'canonical' \
               AND btc_coinbase_status = 'complete' \
               AND btc_height IS NOT NULL",
            &[],
        )
        .await
        .context("load max complete canonical height")?;
    Ok(row.and_then(|row| row.get::<_, Option<i32>>(0)))
}

/// Loaded canonical-backbone coverage for one or more height windows, keyed by
/// height. Backs the backbone-sync gate: every requested height must have
/// exactly one complete canonical row, link-consistent to its predecessor, or
/// the tree/branch query fails with a BackboneUnsynced/BackboneConflict error
/// (run sync-bitcoin-core).
#[derive(Debug, Clone)]
pub(crate) struct BackboneWindowCoverage {
    by_height: BTreeMap<i32, Vec<BackboneCoverageRow>>,
}

/// One canonical block row within a coverage window (hash/prev_hash wire
/// bytes, coinbase_status). Private to the coverage check; prev_hash links are
/// verified against the prior height's hash to detect backbone forks.
#[derive(Debug, Clone)]
struct BackboneCoverageRow {
    hash: Vec<u8>,
    prev_hash: Vec<u8>,
    coinbase_status: String,
}

const BACKBONE_WINDOW_COVERAGE_SQL: &str = "\
WITH requested(from_height, to_height) AS ( \
    SELECT * FROM unnest($1::int[], $2::int[]) \
) \
SELECT b.btc_height, b.btc_header_hash, b.btc_prev_header_hash, b.btc_coinbase_status \
FROM requested r \
JOIN LATERAL ( \
    SELECT b.btc_height, b.btc_header_hash, b.btc_prev_header_hash, b.btc_coinbase_status \
    FROM block b \
    WHERE b.kind = 'canonical' \
      AND b.btc_height BETWEEN r.from_height AND r.to_height \
    OFFSET 0 \
) b ON TRUE \
ORDER BY b.btc_height, b.btc_header_hash";

/// Load backbone coverage for a single [from,to] height window. Thin wrapper
/// over the multi-window form; used by the single-window tree path
/// (ensure_backbone_window_coverage).
async fn load_backbone_window_coverage(
    client: &Client,
    from_height: i32,
    to_height: i32,
) -> Result<BackboneWindowCoverage> {
    load_backbone_window_coverage_for_windows(client, &[(from_height, to_height)]).await
}

/// Load backbone coverage for N height windows in one round trip (unnest +
/// lateral range lookup; see BACKBONE_WINDOW_COVERAGE_SQL for the OFFSET 0
/// optimization fence). Empty windows short-circuit to empty coverage. Used by
/// the multi-window stale-branch navigator (branches.rs).
pub(crate) async fn load_backbone_window_coverage_for_windows(
    client: &Client,
    windows: &[(i32, i32)],
) -> Result<BackboneWindowCoverage> {
    if windows.is_empty() {
        return Ok(BackboneWindowCoverage {
            by_height: BTreeMap::new(),
        });
    }
    let (from_heights, to_heights) = split_height_windows(windows);
    let rows = client
        .query(
            // The OFFSET 0 is intentional and covered by a SQL-contract test:
            // it prevents Postgres from flattening this parameterized lateral
            // range lookup into a seq scan over the million-row canonical block
            // population when many navigation windows are requested.
            BACKBONE_WINDOW_COVERAGE_SQL,
            &[&from_heights, &to_heights],
        )
        .await
        .context("load tree backbone coverage rows")?;

    let mut by_height = BTreeMap::<i32, Vec<BackboneCoverageRow>>::new();
    for row in rows {
        let height: Option<i32> = row.get(0);
        let Some(height) = height else {
            continue;
        };
        by_height
            .entry(height)
            .or_default()
            .push(BackboneCoverageRow {
                hash: row.get(1),
                prev_hash: row.get(2),
                coinbase_status: row.get(3),
            });
    }
    Ok(BackboneWindowCoverage { by_height })
}

/// Load + assert backbone coverage for a single window, returning the locked
/// BackboneUnsynced/BackboneConflict ApiError if any height is missing,
/// partial, duplicated, or link-broken. The tree-window backbone gate.
pub(crate) async fn ensure_backbone_window_coverage(
    client: &Client,
    from_height: i32,
    to_height: i32,
) -> Result<(), ProjectionError> {
    let coverage = load_backbone_window_coverage(client, from_height, to_height).await?;
    coverage.ensure_window(from_height, to_height)
}

impl BackboneWindowCoverage {
    /// Boolean form of the coverage check: true iff every height has exactly one
    /// complete, link-consistent canonical row. Used by the stale-branch
    /// readiness probe (branches.rs) where a missing window is a soft skip, not an
    /// error.
    pub(crate) fn window_is_complete(&self, from_height: i32, to_height: i32) -> bool {
        for height in from_height..=to_height {
            match self.by_height.get(&height) {
                Some(rows) if rows.len() == 1 && rows[0].coinbase_status == "complete" => {}
                _ => return false,
            }
        }

        for height in (from_height + 1)..=to_height {
            let Some(previous) = self
                .by_height
                .get(&(height - 1))
                .and_then(|rows| rows.first())
            else {
                return false;
            };
            let Some(current) = self.by_height.get(&height).and_then(|rows| rows.first()) else {
                return false;
            };
            if current.prev_hash != previous.hash {
                return false;
            }
        }

        true
    }

    /// Erroring form of the coverage check: Ok iff the window is fully
    /// backbone-complete and link-consistent, else a BackboneUnsynced (missing/
    /// partial) or BackboneConflict (duplicate/link-mismatch) ApiError carrying
    /// the diagnostic details the locked error envelope serializes.
    fn ensure_window(&self, from_height: i32, to_height: i32) -> Result<(), ProjectionError> {
        let mut first_missing_height = None;
        let mut missing_count = 0u64;
        let mut partial_count = 0u64;
        for height in from_height..=to_height {
            match self.by_height.get(&height) {
                None => {
                    missing_count += 1;
                    first_missing_height.get_or_insert(height);
                }
                Some(rows) if rows.len() != 1 => {
                    return backbone_conflict_error(
                        from_height,
                        to_height,
                        height,
                        "duplicate_canonical_height",
                        rows,
                    );
                }
                Some(rows) if rows[0].coinbase_status != "complete" => {
                    partial_count += 1;
                    first_missing_height.get_or_insert(height);
                }
                Some(_) => {}
            }
        }

        if missing_count > 0 || partial_count > 0 {
            return Err(ProjectionError::Api(ApiError::BackboneUnsynced {
                details: json!({
                    "from_height": from_height,
                    "to_height": to_height,
                    "first_missing_height": first_missing_height,
                    "missing_count": missing_count,
                    "partial_count": partial_count,
                    "conflict_count": 0u64,
                    "action": "run sync-bitcoin-core",
                }),
            }));
        }

        for height in (from_height + 1)..=to_height {
            let Some(previous) = self
                .by_height
                .get(&(height - 1))
                .and_then(|rows| rows.first())
            else {
                continue;
            };
            let Some(current) = self.by_height.get(&height).and_then(|rows| rows.first()) else {
                continue;
            };
            if current.prev_hash != previous.hash {
                return backbone_conflict_error(
                    from_height,
                    to_height,
                    height,
                    "link_mismatch",
                    std::slice::from_ref(current),
                );
            }
        }

        Ok(())
    }
}

/// Build the BackboneConflict ApiError for a duplicate-height or link-mismatch
/// at `height`, embedding the offending hashes (explorer-hex). The conflict
/// details JSON format is part of the locked error envelope.
fn backbone_conflict_error(
    from_height: i32,
    to_height: i32,
    height: i32,
    reason: &'static str,
    rows: &[BackboneCoverageRow],
) -> Result<(), ProjectionError> {
    let hashes = rows
        .iter()
        .map(|row| display_hash(&row.hash))
        .collect::<Result<Vec<_>>>()?;
    Err(ProjectionError::Api(ApiError::BackboneConflict {
        details: json!({
            "from_height": from_height,
            "to_height": to_height,
            "first_missing_height": null,
            "missing_count": 0u64,
            "partial_count": 0u64,
            "conflict_count": 1u64,
            "conflict_height": height,
            "conflict_reason": reason,
            "hashes": hashes,
            "action": "run sync-bitcoin-core",
        }),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backbone_window_coverage_sql_keeps_lateral_optimization_fence() {
        let normalized = BACKBONE_WINDOW_COVERAGE_SQL.to_ascii_lowercase();
        assert!(
            normalized.contains("join lateral"),
            "coverage query must remain a lateral range lookup"
        );
        assert!(
            normalized.contains("offset 0"),
            "OFFSET 0 is the non-flattening fence for the canonical coverage lookup"
        );
    }
}
