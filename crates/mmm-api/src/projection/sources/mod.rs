//! Source health projection for `/api/v1/sources` (`fixtures/api/sources.json`).

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio_postgres::Client;

use super::ProjectionError;
use sync_status::{
    SourceBackboneProgress, SourceCursorProgress, source_status, sync_for_source_code,
};

mod runtime;
mod sync_status;
pub use runtime::SourceRuntime;

#[derive(Debug, Clone, Serialize)]
pub struct SourcesPayload {
    pub sources: Vec<SourceEndpointRecord>,
}

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
    pub runtime: Option<SourceRuntime>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct SourceCounts {
    pub events: usize,
    pub near: usize,
    pub unknown: usize,
    pub canonical: usize,
    pub stale: usize,
    pub error_block: usize,
    pub strict_orphan: usize,
    pub weak_orphan: usize,
}

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
    capture_error_height: Option<i32>,
    runtime: Option<SourceRuntime>,
}

#[derive(Debug, Clone)]
struct SourceCountAggregateRow {
    source_id: i64,
    events: usize,
    last_event_seen: Option<i64>,
    counts: SourceCounts,
}

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
                    None
                };
                let mut sync = sync_for_source_code(
                    &row.code,
                    row.cursor,
                    row.backbone,
                    row.capture_error_height,
                    reference_now,
                );
                runtime::apply_runtime(&mut sync, row.runtime, reference_now);
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

/// Load `source` rows joined with `poll_cursor`, contiguous `bitcoin_core_sync_state`, and
/// the lowest unresolved `capture_error` height (`ORDER BY source.id`).
async fn load_source_endpoint_rows(client: &Client) -> Result<Vec<SourceEndpointBaseRow>> {
    let rows = client
        .query(
            "SELECT source.id, source.code, source.kind, source.chain, source.instance, \
                    source.created_at, \
                    pc.cursor_height, EXTRACT(EPOCH FROM pc.updated_at)::BIGINT, \
                    pc.target_height, \
                    bcs.source_id, bcs.target_tip_height, bcs.contiguous_complete_height, \
                    bcs.last_error_code, bcs.last_error_height, bcs.updated_at, \
                    ce.height, to_jsonb(prs) \
             FROM source \
             LEFT JOIN producer_runtime_state prs ON prs.source_id = source.id \
             LEFT JOIN poll_cursor pc ON pc.source_id = source.id \
             LEFT JOIN bitcoin_core_sync_state bcs \
                ON bcs.source_id = source.id AND bcs.sync_mode = 'contiguous' \
             LEFT JOIN LATERAL ( \
                SELECT capture_error.height FROM capture_error \
                WHERE capture_error.source_id = source.id \
                ORDER BY capture_error.height \
                LIMIT 1 \
             ) ce ON TRUE \
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
                capture_error_height: row.get(15),
                runtime: row
                    .get::<_, Option<serde_json::Value>>(16)
                    .map(serde_json::from_value)
                    .transpose()?,
            })
        })
        .collect()
}

async fn load_source_count_aggregates(client: &Client) -> Result<Vec<SourceCountAggregateRow>> {
    // Read `read_model_invariant` and `source_health` in a single statement so a concurrent
    // maintenance commit cannot split the guard scalars from the rows.
    let rows = client
        .query(
            "SELECT inv.source_health_ready, inv.invalid_unknown_parents, \
                    sh.source_id, sh.events, sh.last_event_seen, sh.near_parents, \
                    sh.unknown_parents, sh.canonical_parents, sh.stale_parents, \
                    sh.error_block_parents, sh.strict_orphan_parents, sh.weak_orphan_parents \
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

    if !ready {
        bail!("source_health not initialized; run reconcile-read-model --rebuild-source-health");
    }
    if invalid_unknown_parents > 0 {
        bail!("active unknown event fails Bitcoin target");
    }

    rows.iter()
        .filter_map(|row| {
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
                        error_block: i64_to_usize(row.get(9), "source error-block count")?,
                        strict_orphan: i64_to_usize(row.get(10), "source strict orphan count")?,
                        weak_orphan: i64_to_usize(row.get(11), "source weak orphan count")?,
                    },
                })
            })
        })
        .collect()
}

fn i64_to_usize(value: i64, label: &str) -> Result<usize> {
    value
        .try_into()
        .with_context(|| format!("{label} does not fit usize"))
}
