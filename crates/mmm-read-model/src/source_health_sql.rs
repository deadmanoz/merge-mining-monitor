//! source_health / read_model_invariant SQL: reconciler-owned derived state.
//!
//! These writers live in the read-model crate (not mmm-store) because
//! source_health and read_model_invariant are derived tables like block and
//! attestation_proof: producers never write them directly, and the mutation
//! module owns the before/after bracket built on the pub(crate) primitives.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio_postgres::{Client, GenericClient};

// ---------------------------------------------------------------------------
// source_health: precomputed per-source rollup counters.
//
// `/api/v1/sources` reads these rows instead of re-aggregating millions of
// `merge_mining_event` rows per request. They are maintained incrementally by
// the reconciler via the parent-contribution before/after diff, and rebuilt
// authoritatively from base tables by `rebuild_source_health`.
// ---------------------------------------------------------------------------

/// Two-int advisory-lock key for serializing a full rebuild against incremental
/// maintenance. The two-int `pg_advisory_xact_lock(int4, int4)` space is
/// DISJOINT from the one-int `pg_advisory_xact_lock(bigint)` space the per-block
/// hash locks use, so these can never collide with a block lock.
const SOURCE_HEALTH_LOCK_CLASS: i32 = 0x5048; // 'PH' - per-source Health
/// The single object id within `SOURCE_HEALTH_LOCK_CLASS`: there is exactly one
/// global source_health rollup, so a fixed `0` serializes all maintenance against
/// the one rebuild.
const SOURCE_HEALTH_LOCK_OBJ: i32 = 0;

/// One source's contribution to a single BTC parent: how many ACTIVE events that
/// source has on the parent, the parent-level `current_kind`, the parent-level
/// `btc_orphan_class` (only meaningful while `current_kind = 'unknown'`), and the
/// max `confirmed_at` over that source's active events on the parent.
#[derive(Debug, Clone)]
struct ParentSourceContribution {
    active_events: i64,
    current_kind: String,
    orphan_class: Option<String>,
    max_confirmed_at: i64,
}

/// The strict/weak orphan buckets a contribution falls into: a parent counts as
/// a strict (resp. weak) orphan for a source only while its `current_kind` is
/// `unknown` AND its `block.btc_orphan_class` is `strict_btc_orphan` (resp.
/// `weak_btc_orphan`). Returns `(strict, weak)` as 0/1 deltas.
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

/// A BTC parent's full contribution to `source_health`, derived from base
/// tables. Each member source contributes exactly one (source -> current_kind)
/// entry, so diffing two snapshots of the same parent maintains the
/// distinct-parent counts without double-counting.
#[derive(Debug, Clone, Default)]
pub(crate) struct ParentContribution {
    per_source: HashMap<i64, ParentSourceContribution>,
    invalid_unknown: bool,
}

/// Per-kind distinct-parent count deltas for one source, accumulated as the
/// before snapshot subtracts (-1 on its `current_kind`) and the after snapshot
/// adds (+1 on its `current_kind`). A parent contributes to exactly one kind
/// bucket per snapshot, so the four fields stay mutually exclusive across a
/// diff.
#[derive(Default)]
struct KindDeltas {
    near: i64,
    unknown: i64,
    canonical: i64,
    stale: i64,
}

impl KindDeltas {
    /// Add `delta` (always -1 for the before kind, +1 for the after kind) to the
    /// matching bucket. Errors on any `current_kind` outside the four known kinds
    /// so an unexpected enum value fails the reconcile loudly rather than silently
    /// dropping a count.
    fn add(&mut self, kind: &str, delta: i64) -> Result<()> {
        match kind {
            "near" => self.near += delta,
            "unknown" => self.unknown += delta,
            "canonical" => self.canonical += delta,
            "stale" => self.stale += delta,
            other => anyhow::bail!("unexpected current_kind {other:?} in source_health diff"),
        }
        Ok(())
    }
}

/// Snapshot one BTC parent's `source_health` contribution from base tables.
///
/// Mirrors the read-path CTE's `parent_current` derivation for a single parent:
/// `current_kind` is `block.kind` if a block row exists, else
/// `unknown`-if-any-active-unknown-event else `near`. A parent with no active
/// events contributes nothing (empty map, not invalid_unknown). The caller MUST
/// already hold the parent advisory lock.
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
             ), pstate AS ( \
                 SELECT \
                     COALESCE( \
                         (SELECT b.kind FROM block b WHERE b.btc_header_hash = $1), \
                         CASE WHEN bool_or(btc_parent_kind = 'unknown') THEN 'unknown' ELSE 'near' END \
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
             GROUP BY pe.source_id, ps.current_kind, ps.orphan_class, ps.has_failed_target",
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
        // Parent-level flag, identical on every row.
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

/// Apply one source's before/after contribution delta to its `source_health`
/// row: the kind-bucket counters, the strict/weak orphan sub-counters, and the
/// `last_event_seen` maintenance. Extracted from `apply_source_health_diff` so
/// that function stays within the structural line budget.
async fn apply_one_source_delta<C: GenericClient>(
    client: &C,
    source_id: i64,
    b: Option<&ParentSourceContribution>,
    a: Option<&ParentSourceContribution>,
    now: i64,
) -> Result<()> {
    let before_events = b.map(|c| c.active_events).unwrap_or(0);
    let after_events = a.map(|c| c.active_events).unwrap_or(0);
    let events_delta = after_events - before_events;

    let mut deltas = KindDeltas::default();
    if let Some(b) = b {
        deltas.add(&b.current_kind, -1)?;
    }
    if let Some(a) = a {
        deltas.add(&a.current_kind, 1)?;
    }

    // Strict/weak orphan deltas: a refinement WITHIN the unknown bucket, so they
    // move on orphan-class transitions (NULL->strict/weak, strict<->weak) even
    // when current_kind stays 'unknown'. The before/after snapshots read
    // block.btc_orphan_class around the same reconcile that writes it.
    let (b_strict, b_weak) = b.map(orphan_bucket).unwrap_or((0, 0));
    let (a_strict, a_weak) = a.map(orphan_bucket).unwrap_or((0, 0));
    let strict_delta = a_strict - b_strict;
    let weak_delta = a_weak - b_weak;

    client
        .execute(
            "INSERT INTO source_health ( \
                 source_id, events, near_parents, unknown_parents, \
                 canonical_parents, stale_parents, strict_orphan_parents, \
                 weak_orphan_parents, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (source_id) DO UPDATE SET \
                 events = source_health.events + EXCLUDED.events, \
                 near_parents = source_health.near_parents + EXCLUDED.near_parents, \
                 unknown_parents = source_health.unknown_parents + EXCLUDED.unknown_parents, \
                 canonical_parents = source_health.canonical_parents + EXCLUDED.canonical_parents, \
                 stale_parents = source_health.stale_parents + EXCLUDED.stale_parents, \
                 strict_orphan_parents = source_health.strict_orphan_parents + EXCLUDED.strict_orphan_parents, \
                 weak_orphan_parents = source_health.weak_orphan_parents + EXCLUDED.weak_orphan_parents, \
                 updated_at = EXCLUDED.updated_at",
            &[
                &source_id,
                &events_delta,
                &deltas.near,
                &deltas.unknown,
                &deltas.canonical,
                &deltas.stale,
                &strict_delta,
                &weak_delta,
                &now,
            ],
        )
        .await
        .context("apply source_health counter delta")?;

    // last_event_seen = max(confirmed_at) over ACTIVE events. A gain/refresh
    // bumps via GREATEST (O(1)). The stored value can only need LOWERING when
    // this source's max confirmed_at ON THIS PARENT drops (membership lost, or an
    // event swapped for a lower-confirmed one even at equal count); in that case
    // the global max may have moved, so recompute from base tables. A count drop
    // that removes a NON-max event leaves this source's parent-max unchanged, so
    // the GREATEST bump below is sufficient and no recompute is needed.
    let max_dropped_or_lost = match (a, b) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some(a), Some(b)) => a.max_confirmed_at < b.max_confirmed_at,
    };
    if max_dropped_or_lost {
        client
            .execute(
                "UPDATE source_health \
                    SET last_event_seen = ( \
                            SELECT max(confirmed_at) FROM merge_mining_event \
                            WHERE source_id = $1 AND revoked_at IS NULL \
                        ), updated_at = $2 \
                  WHERE source_id = $1",
                &[&source_id, &now],
            )
            .await
            .context("recompute source_health last_event_seen")?;
    } else if let Some(a) = a {
        client
            .execute(
                "UPDATE source_health \
                    SET last_event_seen = GREATEST(COALESCE(last_event_seen, $2), $2), \
                        updated_at = $3 \
                  WHERE source_id = $1",
                &[&source_id, &a.max_confirmed_at, &now],
            )
            .await
            .context("bump source_health last_event_seen")?;
    }
    Ok(())
}

/// Apply the difference between two contributions of the SAME parent to
/// `source_health` + `read_model_invariant`.
///
/// SINGLE OWNER of the shared rebuild advisory lock: its first action takes
/// `pg_advisory_xact_lock_shared`, so every maintenance path inherits the
/// rebuild-serialization guarantee without per-path wiring. The caller is
/// expected to already hold the PARENT advisory lock (path-specific). Per-source
/// upserts run in sorted `source_id` order so concurrent multi-source diffs
/// acquire row locks in one global order and cannot deadlock.
pub(crate) async fn apply_source_health_diff<C: GenericClient>(
    client: &C,
    before: &ParentContribution,
    after: &ParentContribution,
) -> Result<()> {
    client
        .execute(
            "SELECT pg_advisory_xact_lock_shared($1, $2)",
            &[&SOURCE_HEALTH_LOCK_CLASS, &SOURCE_HEALTH_LOCK_OBJ],
        )
        .await
        .context("acquire shared source_health advisory lock")?;

    let mut source_ids: Vec<i64> = before
        .per_source
        .keys()
        .chain(after.per_source.keys())
        .copied()
        .collect();
    source_ids.sort_unstable();
    source_ids.dedup();

    let now = mmm_capture::capture::now_epoch_seconds()?;
    for source_id in source_ids {
        let b = before.per_source.get(&source_id);
        let a = after.per_source.get(&source_id);
        apply_one_source_delta(client, source_id, b, a, now).await?;
    }

    let iu_delta = (after.invalid_unknown as i64) - (before.invalid_unknown as i64);
    if iu_delta != 0 {
        client
            .execute(
                "UPDATE read_model_invariant \
                    SET invalid_unknown_parents = invalid_unknown_parents + $1, updated_at = $2 \
                  WHERE id = TRUE",
                &[&iu_delta, &now],
            )
            .await
            .context("apply read_model_invariant invalid_unknown delta")?;
    }
    Ok(())
}

/// One `source_health` row recomputed from base tables: the
/// per-source event count, `last_event_seen`, and the distinct-parent counts
/// per `current_kind` plus the strict/weak orphan refinements. Produced by
/// `compute_source_health_from_base`; written verbatim by
/// `rebuild_source_health` and used as the maintained-vs-recompute test oracle.
#[derive(Debug, Clone)]
pub struct ComputedSourceHealthRow {
    pub source_id: i64,
    pub events: i64,
    pub last_event_seen: Option<i64>,
    pub near_parents: i64,
    pub unknown_parents: i64,
    pub canonical_parents: i64,
    pub stale_parents: i64,
    pub strict_orphan_parents: i64,
    pub weak_orphan_parents: i64,
}

/// The full recompute of `source_health` + the global guard
/// scalar, derived from base tables WITHOUT writing anything. This is the test
/// oracle and the basis for `rebuild_source_health`.
#[derive(Debug, Clone)]
pub struct ComputedSourceHealth {
    pub rows: Vec<ComputedSourceHealthRow>,
    pub invalid_unknown_parents: i64,
}

/// Recompute `source_health` rows + the global invalid-unknown scalar from base
/// tables. NON-mutating: this is the same aggregation the old `/sources` CTE
/// performed, returned rather than written. Used as the rebuild input
/// and as the test oracle (kept separate so the maintained-vs-recompute test
/// cannot be tautological).
pub async fn compute_source_health_from_base<C: GenericClient>(
    client: &C,
) -> Result<ComputedSourceHealth> {
    let rows = client
        .query(
            "WITH parent_rollup AS ( \
                 SELECT e.btc_parent_header_hash, \
                        (array_agg(b.kind) FILTER (WHERE b.kind IS NOT NULL))[1] AS block_kind, \
                        (array_agg(b.btc_orphan_class) FILTER (WHERE b.btc_orphan_class IS NOT NULL))[1] AS block_orphan_class, \
                        bool_or(e.btc_parent_kind = 'unknown') AS has_unknown_event \
                 FROM merge_mining_event e \
                 LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                 WHERE e.revoked_at IS NULL \
                 GROUP BY e.btc_parent_header_hash \
             ), parent_current AS ( \
                 SELECT btc_parent_header_hash, \
                        COALESCE(block_kind, CASE WHEN has_unknown_event THEN 'unknown' ELSE 'near' END) AS current_kind, \
                        block_orphan_class AS orphan_class \
                 FROM parent_rollup \
             ), event_counts AS ( \
                 SELECT source_id, count(*)::bigint AS events, max(confirmed_at) AS last_event_seen \
                 FROM merge_mining_event WHERE revoked_at IS NULL GROUP BY source_id \
             ), parent_counts AS ( \
                 SELECT e.source_id, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'near'))::bigint AS near_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'unknown'))::bigint AS unknown_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'canonical'))::bigint AS canonical_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'stale'))::bigint AS stale_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'unknown' AND pc.orphan_class = 'strict_btc_orphan'))::bigint AS strict_orphan_count, \
                        (count(DISTINCT e.btc_parent_header_hash) FILTER (WHERE pc.current_kind = 'unknown' AND pc.orphan_class = 'weak_btc_orphan'))::bigint AS weak_orphan_count \
                 FROM merge_mining_event e \
                 JOIN parent_current pc ON pc.btc_parent_header_hash = e.btc_parent_header_hash \
                 WHERE e.revoked_at IS NULL GROUP BY e.source_id \
             ) \
             SELECT COALESCE(ec.source_id, pc.source_id) AS source_id, \
                    COALESCE(ec.events, 0), ec.last_event_seen, \
                    COALESCE(pc.near_count, 0), COALESCE(pc.unknown_count, 0), \
                    COALESCE(pc.canonical_count, 0), COALESCE(pc.stale_count, 0), \
                    COALESCE(pc.strict_orphan_count, 0), COALESCE(pc.weak_orphan_count, 0) \
             FROM event_counts ec \
             FULL OUTER JOIN parent_counts pc ON pc.source_id = ec.source_id \
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
            strict_orphan_parents: row.get(7),
            weak_orphan_parents: row.get(8),
        })
        .collect();

    let invalid_unknown_parents: i64 = client
        .query_one(
            "WITH parent_rollup AS ( \
                 SELECT e.btc_parent_header_hash, \
                        (array_agg(b.kind) FILTER (WHERE b.kind IS NOT NULL))[1] AS block_kind, \
                        bool_or(e.btc_parent_kind = 'unknown') AS has_unknown_event, \
                        bool_or(NOT e.pow_validates_btc_target) AS has_failed_target \
                 FROM merge_mining_event e \
                 LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                 WHERE e.revoked_at IS NULL \
                 GROUP BY e.btc_parent_header_hash \
             ), parent_current AS ( \
                 SELECT CASE WHEN COALESCE(block_kind, CASE WHEN has_unknown_event THEN 'unknown' ELSE 'near' END) = 'unknown' \
                             THEN has_failed_target ELSE FALSE END AS invalid_unknown \
                 FROM parent_rollup \
             ) \
             SELECT count(*)::bigint FROM parent_current WHERE invalid_unknown",
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

/// Full rebuild of `source_health` from base tables, inside one
/// transaction holding the EXCLUSIVE rebuild advisory lock (so it can never race
/// an in-flight `apply_source_health_diff`, which holds the shared side). Sets
/// `source_health_ready = TRUE` atomically with the counts. Idempotent: a rebuild
/// of a correctly-maintained table reproduces the same counters.
pub async fn rebuild_source_health(client: &mut Client) -> Result<()> {
    let txn = client
        .transaction()
        .await
        .context("begin source_health rebuild transaction")?;
    txn.execute(
        "SELECT pg_advisory_xact_lock($1, $2)",
        &[&SOURCE_HEALTH_LOCK_CLASS, &SOURCE_HEALTH_LOCK_OBJ],
    )
    .await
    .context("acquire exclusive source_health advisory lock")?;

    let computed = compute_source_health_from_base(&txn).await?;
    let now = mmm_capture::capture::now_epoch_seconds()?;

    txn.execute("DELETE FROM source_health", &[])
        .await
        .context("clear source_health for rebuild")?;
    for row in &computed.rows {
        txn.execute(
            "INSERT INTO source_health ( \
                 source_id, events, last_event_seen, near_parents, \
                 unknown_parents, canonical_parents, stale_parents, \
                 strict_orphan_parents, weak_orphan_parents, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
            &[
                &row.source_id,
                &row.events,
                &row.last_event_seen,
                &row.near_parents,
                &row.unknown_parents,
                &row.canonical_parents,
                &row.stale_parents,
                &row.strict_orphan_parents,
                &row.weak_orphan_parents,
                &now,
            ],
        )
        .await
        .context("insert rebuilt source_health row")?;
    }

    txn.execute(
        "UPDATE read_model_invariant \
            SET invalid_unknown_parents = $1, source_health_ready = TRUE, updated_at = $2 \
          WHERE id = TRUE",
        &[&computed.invalid_unknown_parents, &now],
    )
    .await
    .context("set read_model_invariant on rebuild")?;

    txn.commit().await.context("commit source_health rebuild")?;
    Ok(())
}
