//! Source health projection for `/api/v1/sources`.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio_postgres::Client;

use super::ProjectionError;
use sync_status::{
    SourceBackboneProgress, SourceCursorProgress, source_status, sync_for_source_code,
};

mod sync_status;

/// `/api/v1/sources` success payload (fixture: fixtures/api/sources.json). Wire
/// contract: `sources` is ordered by `source.id` ascending, one record per row
/// in the `source` table. Source ids are permanent and may contain retired gaps.
/// Re-exported and consumed by the integration-test host.
#[derive(Debug, Clone, Serialize)]
pub struct SourcesPayload {
    pub sources: Vec<SourceEndpointRecord>,
}

/// One source row in the `/api/v1/sources` payload (fixture: sources.json). Every
/// field name is the locked JSON wire contract. `status` is the freshness verdict
/// from the derived `last_seen_at`; `sync` is the per-class capture-progress
/// status; `counts` carries the precomputed source_health tallies including
/// strict/weak BTC-orphan sub-counts.
#[derive(Debug, Clone, Serialize)]
pub struct SourceEndpointRecord {
    pub id: i64,
    pub code: String,
    pub kind: String,
    pub chain: Option<String>,
    pub instance: Option<String>,
    pub created_at: i64,
    pub last_seen_at: Option<i64>,
    pub status: &'static str,
    pub sync: SourceSyncStatus,
    pub counts: SourceCounts,
}

/// Per-source capture-progress status nested under each `/api/v1/sources` record
/// (fixture: sources.json). `mode` names the capture class (live,
/// bitcoin-core-backbone, historical, partial, surveyed, catalogued, unknown)
/// and `state` its
/// current phase (not_started, catching_up, live, stale, error, historical,
/// partial, surveyed, catalogued). All fields are the locked
/// wire contract; `&'static str` mode/state values are part of that contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceSyncStatus {
    pub mode: &'static str,
    pub state: &'static str,
    pub progress_height: Option<i32>,
    pub progress_updated_at: Option<i64>,
    pub target_height: Option<i32>,
    pub latest_evidence_at: Option<i64>,
    pub error_code: Option<String>,
    pub error_height: Option<i32>,
}

/// Per-source evidence tallies nested under each `/api/v1/sources` record
/// (fixture: sources.json). Read straight from the precomputed `source_health`
/// table, never re-aggregated at request time. `strict_orphan`/`weak_orphan` are
/// the BTC-orphan sub-counts; field names are the locked wire contract.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceCounts {
    pub events: usize,
    pub near: usize,
    pub unknown: usize,
    pub canonical: usize,
    pub stale: usize,
    pub strict_orphan: usize,
    pub weak_orphan: usize,
}

/// Decoded `source` row joined with its poll_cursor / bitcoin_core_sync_state
/// progress, before merge with source_health counts. Private staging type for
/// `load_source_endpoint_rows`; not serialized.
#[derive(Debug, Clone)]
struct SourceEndpointBaseRow {
    id: i64,
    code: String,
    kind: String,
    chain: Option<String>,
    instance: Option<String>,
    created_at: i64,
    cursor: Option<SourceCursorProgress>,
    backbone: Option<SourceBackboneProgress>,
}

/// One `source_health` row (events + last_event_seen + the SourceCounts tallies)
/// keyed by `source_id`, merged into the endpoint records by `sources`. Private
/// staging type for `load_source_count_aggregates`.
#[derive(Debug, Clone)]
struct SourceCountAggregateRow {
    source_id: i64,
    events: usize,
    last_event_seen: Option<i64>,
    counts: SourceCounts,
}

/// Build the `/api/v1/sources` payload: O(sources) join of the `source` table
/// against precomputed poll_cursor / bitcoin_core_sync_state progress and the
/// `source_health` count table, ordered by `source.id`. Fails closed (Err) until
/// `source_health_ready`, so un-rebuilt zero counts are never served. `reference_now`
/// (epoch secs) anchors the freshness verdicts. For auxpow sources, the derived
/// `last_seen_at` is the latest event seen and seeds `sync.latest_evidence_at`.
pub async fn sources(
    client: &Client,
    reference_now: u64,
) -> Result<SourcesPayload, ProjectionError> {
    let source_rows = load_source_endpoint_rows(client).await?;
    let count_rows = load_source_count_aggregates(client).await?;
    let mut count_rows = count_rows
        .into_iter()
        .map(|row| (row.source_id, row))
        .collect::<HashMap<_, _>>();

    let reference_now = reference_now as i64;
    Ok(SourcesPayload {
        sources: source_rows
            .into_iter()
            .map(|row| {
                let aggregate = count_rows.remove(&row.id);
                let mut counts = aggregate
                    .as_ref()
                    .map(|aggregate| aggregate.counts.clone())
                    .unwrap_or_default();
                counts.events = aggregate
                    .as_ref()
                    .map(|aggregate| aggregate.events)
                    .unwrap_or_default();
                let last_seen_at = if row.kind == "auxpow" {
                    aggregate
                        .as_ref()
                        .and_then(|aggregate| aggregate.last_event_seen)
                } else {
                    // Bitcoin Core freshness is carried by bitcoin_core_sync_state,
                    // not by event-derived source_health counts.
                    None
                };
                let mut sync =
                    sync_for_source_code(&row.code, row.cursor, row.backbone, reference_now);
                if row.kind == "auxpow" && sync.mode == "live" {
                    sync.latest_evidence_at = last_seen_at;
                }
                SourceEndpointRecord {
                    id: row.id,
                    sync,
                    code: row.code,
                    kind: row.kind,
                    chain: row.chain,
                    instance: row.instance,
                    created_at: row.created_at,
                    last_seen_at,
                    status: source_status(last_seen_at, reference_now),
                    counts,
                }
            })
            .collect(),
    })
}

/// Load every `source` row joined LEFT to its poll_cursor progress and (for the
/// live-chaintip backbone) its contiguous bitcoin_core_sync_state, ordered by
/// `source.id` to fix the wire output order. Bails if a join yields a partial
/// cursor (height/updated-at split) or a null contiguous-height/updated-at.
async fn load_source_endpoint_rows(client: &Client) -> Result<Vec<SourceEndpointBaseRow>> {
    let rows = client
        .query(
            "SELECT source.id, source.code, source.kind, source.chain, source.instance, \
                    source.created_at, \
                    pc.cursor_height, EXTRACT(EPOCH FROM pc.updated_at)::BIGINT, \
                    pc.target_height, \
                    bcs.source_id, bcs.target_tip_height, bcs.contiguous_complete_height, \
                    bcs.last_error_code, bcs.last_error_height, bcs.updated_at \
             FROM source \
             LEFT JOIN poll_cursor pc ON pc.source_id = source.id \
             LEFT JOIN bitcoin_core_sync_state bcs \
                ON bcs.source_id = source.id AND bcs.sync_mode = 'contiguous' \
             ORDER BY source.id",
            &[],
        )
        .await
        .context("load source endpoint rows")?;
    rows.into_iter()
        .map(|row| {
            let cursor_height: Option<i32> = row.get(6);
            let cursor_updated_at: Option<i64> = row.get(7);
            let cursor = match (cursor_height, cursor_updated_at) {
                (Some(height), Some(updated_at_epoch)) => Some(SourceCursorProgress {
                    height,
                    target_height: row.get(8),
                    updated_at_epoch,
                }),
                (None, None) => None,
                _ => bail!("poll_cursor join returned partial cursor fields"),
            };
            let backbone_source_id: Option<i64> = row.get(9);
            let backbone = match backbone_source_id {
                Some(_) => {
                    let contiguous_complete_height: Option<i32> = row.get(11);
                    let updated_at_epoch: Option<i64> = row.get(14);
                    Some(SourceBackboneProgress {
                        target_tip_height: row.get(10),
                        contiguous_complete_height: contiguous_complete_height.context(
                            "bitcoin_core_sync_state join returned null contiguous height",
                        )?,
                        last_error_code: row.get(12),
                        last_error_height: row.get(13),
                        updated_at_epoch: updated_at_epoch
                            .context("bitcoin_core_sync_state join returned null updated_at")?,
                    })
                }
                None => None,
            };
            Ok(SourceEndpointBaseRow {
                id: row.get(0),
                code: row.get(1),
                kind: row.get(2),
                chain: row.get(3),
                instance: row.get(4),
                created_at: row.get(5),
                cursor,
                backbone,
            })
        })
        .collect()
}

async fn load_source_count_aggregates(client: &Client) -> Result<Vec<SourceCountAggregateRow>> {
    // O(sources): read the precomputed source_health table + the single-row
    // read_model_invariant guard instead of re-aggregating merge_mining_event.
    // The counters are maintained incrementally by the reconciler and rebuilt by
    // `reconcile-read-model --rebuild-source-health`.
    //
    // The guard scalars and the rows are read in ONE statement (one snapshot): a
    // maintenance transaction committing between two separate reads could expose
    // `invalid_unknown_parents = 0` from before the commit alongside rows from
    // after it, bypassing the fail-closed guard. The LEFT JOIN against the
    // always-present invariant row also keeps the not-ready / empty-table case
    // observable (one row with NULL source_health columns).
    let rows = client
        .query(
            "SELECT inv.source_health_ready, inv.invalid_unknown_parents, \
                    sh.source_id, sh.events, sh.last_event_seen, sh.near_parents, \
                    sh.unknown_parents, sh.canonical_parents, sh.stale_parents, \
                    sh.strict_orphan_parents, sh.weak_orphan_parents \
             FROM read_model_invariant inv \
             LEFT JOIN source_health sh ON TRUE \
             WHERE inv.id = TRUE \
             ORDER BY sh.source_id",
            &[],
        )
        .await
        .context("load source_health with invariant")?;

    let Some(first) = rows.first() else {
        bail!("read_model_invariant row missing; apply database migrations");
    };
    let ready: bool = first.get(0);
    let invalid_unknown_parents: i64 = first.get(1);

    // Fail closed: an un-rebuilt (empty) source_health must never be served as
    // legitimate zeros. Once the first rebuild sets ready = TRUE this is satisfied.
    if !ready {
        bail!("source_health not initialized; run reconcile-read-model --rebuild-source-health");
    }
    // Preserve the prior invalid-unknown guard exactly (same error string).
    if invalid_unknown_parents > 0 {
        bail!("active unknown event fails Bitcoin target");
    }

    rows.iter()
        .filter_map(|row| {
            // NULL source_id => the LEFT JOIN placeholder for an empty table.
            let source_id: Option<i64> = row.get(2);
            source_id.map(|source_id| {
                Ok(SourceCountAggregateRow {
                    source_id,
                    events: i64_to_usize(row.get(3), "source event count")?,
                    last_event_seen: row.get(4),
                    counts: SourceCounts {
                        events: 0,
                        near: i64_to_usize(row.get(5), "source near count")?,
                        unknown: i64_to_usize(row.get(6), "source unknown count")?,
                        canonical: i64_to_usize(row.get(7), "source canonical count")?,
                        stale: i64_to_usize(row.get(8), "source stale count")?,
                        strict_orphan: i64_to_usize(row.get(9), "source strict orphan count")?,
                        weak_orphan: i64_to_usize(row.get(10), "source weak orphan count")?,
                    },
                })
            })
        })
        .collect()
}

/// Convert a Postgres BIGINT count to usize, attaching the column `label` on the
/// (should-be-impossible) negative-count overflow so a corrupt source_health row
/// surfaces a named error rather than a panic.
fn i64_to_usize(value: i64, label: &str) -> Result<usize> {
    value
        .try_into()
        .with_context(|| format!("{label} does not fit usize"))
}
