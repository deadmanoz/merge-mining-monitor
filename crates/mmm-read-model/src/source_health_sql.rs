//! `source_health` and `read_model_invariant` derived writers.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, GenericClient, Transaction};

/// Two-int advisory-lock key (`0x5048`, `'PH'`) for serializing a full rebuild
/// against incremental maintenance. Disjoint from the one-int `bigint` per-block
/// hash lock space.
const SOURCE_HEALTH_LOCK_CLASS: i32 = 0x5048; // 'PH' - per-source Health
const SOURCE_HEALTH_LOCK_OBJ: i32 = 0;

#[derive(Debug, Clone)]
struct ParentSourceContribution {
    active_events: i64,
    current_kind: String,
    orphan_class: Option<String>,
    /// The latest capture at this parent: evidence `confirmed_at`, or a near parent's latest
    /// observation `recorded_at`.
    max_confirmed_at: i64,
}

fn orphan_bucket(contribution: &ParentSourceContribution) -> (i64, i64) {
    if contribution.current_kind != "unknown" {
        return (0, 0);
    }
    match contribution.orphan_class.as_deref() {
        Some("strict_btc_orphan") => (1, 0),
        Some("weak_btc_orphan") => (0, 1),
        _ => (0, 0),
    }
}

/// One BTC parent's contribution to `source_health` and `read_model_invariant`.
#[derive(Debug, Clone, Default)]
pub(crate) struct ParentContribution {
    per_source: HashMap<i64, ParentSourceContribution>,
    invalid_unknown: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct KindDeltas {
    near: i64,
    unknown: i64,
    canonical: i64,
    stale: i64,
    error_block: i64,
}

/// Aggregate change to one source across one or more parent mutations, combined
/// before updating `source_health` in globally sorted `source_id` order.
#[derive(Debug, Default, PartialEq, Eq)]
struct SourceHealthDelta {
    events: i64,
    kinds: KindDeltas,
    strict_orphans: i64,
    weak_orphans: i64,
    max_after_confirmed_at: Option<i64>,
    recompute_last_event_seen: bool,
}

impl SourceHealthDelta {
    fn add_parent_diff(
        &mut self,
        before: Option<&ParentSourceContribution>,
        after: Option<&ParentSourceContribution>,
    ) -> Result<()> {
        self.events += after.map(|c| c.active_events).unwrap_or(0)
            - before.map(|c| c.active_events).unwrap_or(0);

        if let Some(before) = before {
            self.kinds.add(&before.current_kind, -1)?;
        }
        if let Some(after) = after {
            self.kinds.add(&after.current_kind, 1)?;
            self.max_after_confirmed_at = Some(
                self.max_after_confirmed_at
                    .map_or(after.max_confirmed_at, |current| {
                        current.max(after.max_confirmed_at)
                    }),
            );
        }

        let (before_strict, before_weak) = before.map(orphan_bucket).unwrap_or((0, 0));
        let (after_strict, after_weak) = after.map(orphan_bucket).unwrap_or((0, 0));
        self.strict_orphans += after_strict - before_strict;
        self.weak_orphans += after_weak - before_weak;

        self.recompute_last_event_seen |= match (after, before) {
            (_, None) => false,
            (None, Some(_)) => true,
            (Some(after), Some(before)) => after.max_confirmed_at < before.max_confirmed_at,
        };
        Ok(())
    }
}

fn aggregate_source_health_diffs<'a>(
    diffs: impl IntoIterator<Item = (&'a ParentContribution, &'a ParentContribution)>,
) -> Result<(HashMap<i64, SourceHealthDelta>, i64)> {
    let mut per_source = HashMap::<i64, SourceHealthDelta>::new();
    let mut invalid_unknown_delta = 0_i64;

    for (before, after) in diffs {
        let mut source_ids: Vec<i64> = before
            .per_source
            .keys()
            .chain(after.per_source.keys())
            .copied()
            .collect();
        source_ids.sort_unstable();
        source_ids.dedup();

        for source_id in source_ids {
            per_source.entry(source_id).or_default().add_parent_diff(
                before.per_source.get(&source_id),
                after.per_source.get(&source_id),
            )?;
        }
        invalid_unknown_delta += (after.invalid_unknown as i64) - (before.invalid_unknown as i64);
    }

    Ok((per_source, invalid_unknown_delta))
}

impl KindDeltas {
    fn add(&mut self, kind: &str, delta: i64) -> Result<()> {
        match kind {
            "near" => self.near += delta,
            "unknown" => self.unknown += delta,
            "canonical" => self.canonical += delta,
            "stale" => self.stale += delta,
            "error_block" => self.error_block += delta,
            other => anyhow::bail!("unexpected current_kind {other:?} in source_health diff"),
        }
        Ok(())
    }
}

/// Snapshot one BTC parent's `source_health` contribution from base tables.
/// Caller must hold the parent advisory lock.
pub(crate) async fn snapshot_parent_contribution<C: GenericClient>(
    client: &C,
    parent_hash: &[u8],
) -> Result<ParentContribution> {
    let rows = client
        .query(
            "WITH parent_events AS ( \
                 SELECT source_id, btc_parent_kind, pow_validates_btc_target, confirmed_at \
                 FROM merge_mining_event \
                 WHERE btc_parent_header_hash = $1 AND revoked_at IS NULL \
             ), near_sources AS ( \
                 SELECT source_id, max(recorded_at) AS last_recorded_at \
                 FROM child_block_observation \
                 WHERE btc_parent_header_hash = $1 AND observation_kind = 'near' \
                 GROUP BY source_id \
             ), pstate AS ( \
                 SELECT \
                     COALESCE( \
                         (SELECT b.kind FROM block b WHERE b.btc_header_hash = $1), 'unknown' \
                     ) AS current_kind, \
                     (SELECT b.btc_orphan_class FROM block b WHERE b.btc_header_hash = $1) AS orphan_class, \
                     bool_or(NOT pow_validates_btc_target) AS has_failed_target \
                 FROM parent_events \
             ) \
             SELECT pe.source_id, \
                    count(*)::bigint AS active_events, \
                    max(pe.confirmed_at) AS max_confirmed_at, \
                    ps.current_kind, \
                    ps.orphan_class, \
                    (ps.current_kind = 'unknown' AND COALESCE(ps.has_failed_target, FALSE)) AS invalid_unknown \
             FROM parent_events pe CROSS JOIN pstate ps \
             GROUP BY pe.source_id, ps.current_kind, ps.orphan_class, ps.has_failed_target \
             UNION ALL \
             SELECT ns.source_id, 0::bigint, ns.last_recorded_at, 'near', NULL::text, FALSE \
             FROM near_sources ns \
             WHERE NOT EXISTS (SELECT 1 FROM parent_events pe WHERE pe.source_id = ns.source_id)",
            &[&parent_hash],
        )
        .await
        .context("snapshot parent source_health contribution")?;

    let mut contribution = ParentContribution::default();
    for row in rows {
        let source_id: i64 = row.get(0);
        let active_events: i64 = row.get(1);
        let max_confirmed_at: i64 = row.get(2);
        let current_kind: String = row.get(3);
        let orphan_class: Option<String> = row.get(4);
        let invalid_unknown: bool = row.get(5);
        contribution.invalid_unknown = contribution.invalid_unknown || invalid_unknown;
        contribution.per_source.insert(
            source_id,
            ParentSourceContribution {
                active_events,
                current_kind,
                orphan_class,
                max_confirmed_at,
            },
        );
    }
    Ok(contribution)
}

async fn apply_one_source_delta<C: GenericClient>(
    client: &C,
    source_id: i64,
    delta: &SourceHealthDelta,
    now: i64,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO source_health ( \
                 source_id, events, near_parents, unknown_parents, \
                 canonical_parents, stale_parents, error_block_parents, \
                 strict_orphan_parents, weak_orphan_parents, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (source_id) DO UPDATE SET \
                 events = source_health.events + EXCLUDED.events, \
                 near_parents = source_health.near_parents + EXCLUDED.near_parents, \
                 unknown_parents = source_health.unknown_parents + EXCLUDED.unknown_parents, \
                 canonical_parents = source_health.canonical_parents + EXCLUDED.canonical_parents, \
                 stale_parents = source_health.stale_parents + EXCLUDED.stale_parents, \
                 error_block_parents = source_health.error_block_parents + EXCLUDED.error_block_parents, \
                 strict_orphan_parents = source_health.strict_orphan_parents + EXCLUDED.strict_orphan_parents, \
                 weak_orphan_parents = source_health.weak_orphan_parents + EXCLUDED.weak_orphan_parents, \
                 updated_at = EXCLUDED.updated_at",
            &[
                &source_id,
                &delta.events,
                &delta.kinds.near,
                &delta.kinds.unknown,
                &delta.kinds.canonical,
                &delta.kinds.stale,
                &delta.kinds.error_block,
                &delta.strict_orphans,
                &delta.weak_orphans,
                &now,
            ],
        )
        .await
        .context("apply source_health counter delta")?;

    if delta.recompute_last_event_seen {
        client
            .execute(
                "UPDATE source_health \
                    SET last_event_seen = GREATEST( \
                            (SELECT max(confirmed_at) FROM merge_mining_event \
                              WHERE source_id = $1 AND revoked_at IS NULL), \
                            (SELECT max(recorded_at) FROM child_block_observation \
                              WHERE source_id = $1 AND observation_kind = 'near') \
                        ), updated_at = $2 \
                  WHERE source_id = $1",
                &[&source_id, &now],
            )
            .await
            .context("recompute source_health last_event_seen")?;
    } else if let Some(max_after_confirmed_at) = delta.max_after_confirmed_at {
        client
            .execute(
                "UPDATE source_health \
                    SET last_event_seen = GREATEST(COALESCE(last_event_seen, $2), $2), \
                        updated_at = $3 \
                  WHERE source_id = $1",
                &[&source_id, &max_after_confirmed_at, &now],
            )
            .await
            .context("bump source_health last_event_seen")?;
    }
    Ok(())
}

/// Apply aggregated source-health deltas under the shared rebuild lock, touching
/// `source_health` rows in globally sorted `source_id` order.
async fn apply_aggregated_source_health_diff<C: GenericClient>(
    client: &C,
    per_source: HashMap<i64, SourceHealthDelta>,
    invalid_unknown_delta: i64,
) -> Result<()> {
    if per_source.is_empty() && invalid_unknown_delta == 0 {
        return Ok(());
    }

    client
        .execute(
            "SELECT pg_advisory_xact_lock_shared($1, $2)",
            &[&SOURCE_HEALTH_LOCK_CLASS, &SOURCE_HEALTH_LOCK_OBJ],
        )
        .await
        .context("acquire shared source_health advisory lock")?;

    let mut source_ids: Vec<i64> = per_source.keys().copied().collect();
    source_ids.sort_unstable();

    let now = mmm_capture::capture::now_epoch_seconds()?;
    for source_id in source_ids {
        let delta = per_source
            .get(&source_id)
            .expect("source id was collected from the aggregate delta map");
        apply_one_source_delta(client, source_id, delta, now).await?;
    }

    if invalid_unknown_delta != 0 {
        client
            .execute(
                "UPDATE read_model_invariant \
                    SET invalid_unknown_parents = invalid_unknown_parents + $1, updated_at = $2 \
                  WHERE id = TRUE",
                &[&invalid_unknown_delta, &now],
            )
            .await
            .context("apply read_model_invariant invalid_unknown delta")?;
    }
    Ok(())
}

/// Apply a single parent's contribution diff under the shared `source_health`
/// advisory lock. Caller must hold the parent advisory lock.
pub(crate) async fn apply_source_health_diff<C: GenericClient>(
    client: &C,
    before: &ParentContribution,
    after: &ParentContribution,
) -> Result<()> {
    let (per_source, invalid_unknown_delta) =
        aggregate_source_health_diffs(std::iter::once((before, after)))?;
    apply_aggregated_source_health_diff(client, per_source, invalid_unknown_delta).await
}

/// Before/after `source_health` bracket across multiple parent hashes in one
/// transaction. Caller must hold all parent advisory locks before calling `open`.
pub(crate) struct MultiParentSourceHealthBracket {
    before: Vec<(Vec<u8>, ParentContribution)>,
}

impl MultiParentSourceHealthBracket {
    pub(crate) async fn open<C, I, H>(client: &C, hashes: I) -> Result<Self>
    where
        C: GenericClient,
        I: IntoIterator<Item = H>,
        H: AsRef<[u8]>,
    {
        let mut hashes: Vec<Vec<u8>> = hashes
            .into_iter()
            .map(|hash| hash.as_ref().to_vec())
            .collect();
        hashes.sort_unstable();
        hashes.dedup();

        let mut before = Vec::with_capacity(hashes.len());
        for hash in hashes {
            let contribution = snapshot_parent_contribution(client, &hash).await?;
            before.push((hash, contribution));
        }
        Ok(Self { before })
    }

    pub(crate) async fn close<C: GenericClient>(self, client: &C) -> Result<()> {
        let mut after = Vec::with_capacity(self.before.len());
        for (hash, _) in &self.before {
            after.push(snapshot_parent_contribution(client, hash).await?);
        }

        let diffs = self
            .before
            .iter()
            .zip(after.iter())
            .map(|((_, before), after)| (before, after));
        let (per_source, invalid_unknown_delta) = aggregate_source_health_diffs(diffs)?;
        apply_aggregated_source_health_diff(client, per_source, invalid_unknown_delta).await
    }
}

#[derive(Debug, Clone)]
pub struct ComputedSourceHealthRow {
    pub source_id: i64,
    pub events: i64,
    pub last_event_seen: Option<i64>,
    pub near_parents: i64,
    pub unknown_parents: i64,
    pub canonical_parents: i64,
    pub stale_parents: i64,
    pub error_block_parents: i64,
    pub strict_orphan_parents: i64,
    pub weak_orphan_parents: i64,
}

#[derive(Debug, Clone)]
pub struct ComputedSourceHealth {
    pub rows: Vec<ComputedSourceHealthRow>,
    pub invalid_unknown_parents: i64,
}

/// Recompute `source_health` rows and the global `invalid_unknown_parents` scalar
/// from base tables without mutating state.
pub async fn compute_source_health_from_base<C: GenericClient>(
    client: &C,
) -> Result<ComputedSourceHealth> {
    let rows = client
        .query(
            "WITH parent_rollup AS ( \
                 SELECT e.btc_parent_header_hash, \
                        (array_agg(b.kind) FILTER (WHERE b.kind IS NOT NULL))[1] AS block_kind, \
                        (array_agg(b.btc_orphan_class) FILTER (WHERE b.btc_orphan_class IS NOT NULL))[1] AS block_orphan_class \
                 FROM merge_mining_event e \
                 LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                 WHERE e.revoked_at IS NULL \
                 GROUP BY e.btc_parent_header_hash \
             ), parent_current AS ( \
                 SELECT btc_parent_header_hash, \
                        COALESCE(block_kind, 'unknown') AS current_kind, \
                        block_orphan_class AS orphan_class \
                 FROM parent_rollup \
             ), near_counts AS ( \
                 SELECT source_id, count(DISTINCT btc_parent_header_hash)::bigint AS near_count, \
                        max(recorded_at) AS last_near_seen \
                 FROM child_block_observation WHERE observation_kind = 'near' GROUP BY source_id \
             ), parent_counts AS ( \
                 SELECT e.source_id, count(*)::bigint AS events, \
                        max(e.confirmed_at) AS last_event_seen, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'unknown'))::bigint AS unknown_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'canonical'))::bigint AS canonical_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'stale'))::bigint AS stale_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'error_block'))::bigint AS error_block_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'unknown' AND pc.orphan_class = 'strict_btc_orphan'))::bigint AS strict_orphan_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'unknown' AND pc.orphan_class = 'weak_btc_orphan'))::bigint AS weak_orphan_count \
                 FROM merge_mining_event e \
                 JOIN parent_current pc ON pc.btc_parent_header_hash = e.btc_parent_header_hash \
                 WHERE e.revoked_at IS NULL GROUP BY e.source_id \
             ) \
             SELECT COALESCE(pc.source_id, nc.source_id) AS source_id, \
                    COALESCE(pc.events, 0), GREATEST(pc.last_event_seen, nc.last_near_seen), \
                    COALESCE(nc.near_count, 0), \
                    COALESCE(pc.unknown_count, 0), \
                    COALESCE(pc.canonical_count, 0), COALESCE(pc.stale_count, 0), \
                    COALESCE(pc.error_block_count, 0), COALESCE(pc.strict_orphan_count, 0), \
                    COALESCE(pc.weak_orphan_count, 0) \
             FROM parent_counts pc \
             FULL OUTER JOIN near_counts nc ON nc.source_id = pc.source_id \
             ORDER BY source_id",
            &[],
        )
        .await
        .context("recompute source_health rows from base")?;

    let rows = rows
        .into_iter()
        .map(|row| ComputedSourceHealthRow {
            source_id: row.get(0),
            events: row.get(1),
            last_event_seen: row.get(2),
            near_parents: row.get(3),
            unknown_parents: row.get(4),
            canonical_parents: row.get(5),
            stale_parents: row.get(6),
            error_block_parents: row.get(7),
            strict_orphan_parents: row.get(8),
            weak_orphan_parents: row.get(9),
        })
        .collect();

    let invalid_unknown_parents: i64 = client
        .query_one(
            "WITH parent_rollup AS ( \
                 SELECT e.btc_parent_header_hash, \
                        (array_agg(b.kind) FILTER (WHERE b.kind IS NOT NULL))[1] AS block_kind, \
                        bool_or(NOT e.pow_validates_btc_target) AS has_failed_target \
                 FROM merge_mining_event e \
                 LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                 WHERE e.revoked_at IS NULL \
                 GROUP BY e.btc_parent_header_hash \
             ) \
             SELECT count(*)::bigint FROM parent_rollup \
              WHERE COALESCE(block_kind, 'unknown') = 'unknown' AND has_failed_target",
            &[],
        )
        .await
        .context("recompute invalid_unknown_parents from base")?
        .get(0);

    Ok(ComputedSourceHealth {
        rows,
        invalid_unknown_parents,
    })
}

/// Rebuild `source_health` from base tables under the exclusive rebuild advisory lock.
pub async fn rebuild_source_health(client: &mut Client) -> Result<()> {
    let txn = client
        .transaction()
        .await
        .context("begin source_health rebuild transaction")?;
    rebuild_source_health_in_transaction(&txn).await?;
    txn.commit().await.context("commit source_health rebuild")?;
    Ok(())
}

/// Take the exclusive `source_health` lock and mark `source_health_ready = FALSE`
/// before committing a historical import transaction.
pub async fn invalidate_source_health_in_transaction(client: &Transaction<'_>) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&SOURCE_HEALTH_LOCK_CLASS, &SOURCE_HEALTH_LOCK_OBJ],
        )
        .await
        .context("acquire source_health lock for historical invalidation")?;
    let now = mmm_capture::capture::now_epoch_seconds()?;
    client
        .execute(
            "UPDATE read_model_invariant \
             SET source_health_ready = FALSE, updated_at = $1 \
             WHERE id = TRUE",
            &[&now],
        )
        .await
        .context("invalidate source_health before historical import")?;
    Ok(())
}

pub(crate) async fn rebuild_source_health_in_transaction<C: GenericClient>(
    client: &C,
) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock($1, $2)",
            &[&SOURCE_HEALTH_LOCK_CLASS, &SOURCE_HEALTH_LOCK_OBJ],
        )
        .await
        .context("acquire exclusive source_health advisory lock")?;

    let historical_work_pending: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM historical_reconcile_queue)",
            &[],
        )
        .await
        .context("check pending historical reconciliation before source_health rebuild")?
        .get(0);
    if historical_work_pending {
        bail!("cannot rebuild source_health while historical reconciliation is pending");
    }

    let computed = compute_source_health_from_base(client).await?;
    let now = mmm_capture::capture::now_epoch_seconds()?;

    client
        .execute("DELETE FROM source_health", &[])
        .await
        .context("clear source_health for rebuild")?;
    for row in &computed.rows {
        client
            .execute(
                "INSERT INTO source_health ( \
                 source_id, events, last_event_seen, near_parents, \
                 unknown_parents, canonical_parents, stale_parents, \
                 error_block_parents, strict_orphan_parents, weak_orphan_parents, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
                &[
                    &row.source_id,
                    &row.events,
                    &row.last_event_seen,
                    &row.near_parents,
                    &row.unknown_parents,
                    &row.canonical_parents,
                    &row.stale_parents,
                    &row.error_block_parents,
                    &row.strict_orphan_parents,
                    &row.weak_orphan_parents,
                    &now,
                ],
            )
            .await
            .context("insert rebuilt source_health row")?;
    }

    client
        .execute(
            "UPDATE read_model_invariant \
            SET invalid_unknown_parents = $1, source_health_ready = TRUE, updated_at = $2 \
          WHERE id = TRUE",
            &[&computed.invalid_unknown_parents, &now],
        )
        .await
        .context("set read_model_invariant on rebuild")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(
        active_events: i64,
        current_kind: &str,
        orphan_class: Option<&str>,
        max_confirmed_at: i64,
    ) -> ParentSourceContribution {
        ParentSourceContribution {
            active_events,
            current_kind: current_kind.to_owned(),
            orphan_class: orphan_class.map(str::to_owned),
            max_confirmed_at,
        }
    }

    fn parent(
        entries: impl IntoIterator<Item = (i64, ParentSourceContribution)>,
        invalid_unknown: bool,
    ) -> ParentContribution {
        ParentContribution {
            per_source: entries.into_iter().collect(),
            invalid_unknown,
        }
    }

    #[test]
    fn aggregate_combines_parent_transitions_per_source() {
        let before_a = parent(
            [
                (7, source(2, "canonical", None, 100)),
                (9, source(1, "unknown", Some("strict_btc_orphan"), 80)),
            ],
            true,
        );
        let after_a = parent([(7, source(2, "stale", None, 100))], false);
        let before_b = parent([(7, source(1, "near", None, 90))], false);
        let after_b = parent(
            [
                (7, source(3, "canonical", None, 110)),
                (9, source(1, "unknown", Some("weak_btc_orphan"), 70)),
            ],
            true,
        );

        let (deltas, invalid_unknown_delta) =
            aggregate_source_health_diffs([(&before_a, &after_a), (&before_b, &after_b)])
                .expect("valid kinds aggregate");

        assert_eq!(invalid_unknown_delta, 0);
        assert_eq!(
            deltas.get(&7),
            Some(&SourceHealthDelta {
                events: 2,
                kinds: KindDeltas {
                    near: -1,
                    canonical: 0,
                    stale: 1,
                    ..KindDeltas::default()
                },
                max_after_confirmed_at: Some(110),
                ..SourceHealthDelta::default()
            })
        );
        assert_eq!(
            deltas.get(&9),
            Some(&SourceHealthDelta {
                strict_orphans: -1,
                weak_orphans: 1,
                max_after_confirmed_at: Some(70),
                recompute_last_event_seen: true,
                ..SourceHealthDelta::default()
            })
        );
    }

    #[test]
    fn aggregate_recomputes_last_seen_if_any_parent_max_drops() {
        let before_a = parent([(7, source(1, "canonical", None, 200))], false);
        let after_a = parent([(7, source(1, "canonical", None, 150))], false);
        let before_b = ParentContribution::default();
        let after_b = parent([(7, source(1, "canonical", None, 300))], false);

        let (deltas, _) =
            aggregate_source_health_diffs([(&before_a, &after_a), (&before_b, &after_b)])
                .expect("valid kinds aggregate");
        let delta = deltas.get(&7).expect("source delta");

        assert!(delta.recompute_last_event_seen);
        assert_eq!(delta.max_after_confirmed_at, Some(300));
        assert_eq!(delta.events, 1);
        assert_eq!(delta.kinds.canonical, 1);
    }

    #[test]
    fn aggregate_rejects_unknown_current_kind() {
        let before = parent([(7, source(1, "surprise", None, 100))], false);
        let after = ParentContribution::default();

        let error = aggregate_source_health_diffs([(&before, &after)])
            .expect_err("unexpected kind must fail loudly");

        assert!(error.to_string().contains("unexpected current_kind"));
    }
}
