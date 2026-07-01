//! Offline historical pool re-resolution: the `reclassify-pools` subcommand.
//!
//! Replays stored BYTEA coinbase columns
//! (`btc_parent_coinbase_script`, `btc_parent_coinbase_outputs`,
//! `child_coinbase_script`) against the embedded, expanded `PoolResolver` and
//! materializes source-scoped `event_pool_attribution` rows. No RPC, no
//! `BITCOIN_RPC_URL`: read-model reconciliation runs with a disabled classifier
//! exactly like the missing-only startup repair.
//!
//! Self-contained: it seeds the expanded `pool` snapshot, RSK-only pool slugs,
//! and child identity registries itself, so `pool_ids_by_slug` is populated
//! before scanning. It works on a fresh schema or in CI without a prior producer
//! run.
//!
//! Safety rules (RSK-safe):
//! - Only rows whose relevant evidence column is present are considered for an
//!   attribution family; RSK miner-address rows are handled by the RSK sidecar
//!   tail rather than the BTC/child-coinbase scan.
//! - BTC-derived matching only writes resolved attributions. Child payout replay
//!   also writes unresolved observed payout addresses with NULL pool IDs.
//! - Default is fill-missing-only. `--overwrite` additionally replaces existing
//!   source-scoped BTC-coinbase-derived attribution rows while never erasing a
//!   row because the current resolver cannot match it.
//! - Bounded and idempotent: a second run with no registry change is a no-op.
//!
//! Transactional per parent (mirrors `mmm_read_model::capture_in_txn`): a
//! parent's attribution-row writes and that parent's read-model reconcile commit
//! together in ONE transaction when parent attribution changes. A mid-pass
//! failure (cascade budget, transient DB error) is therefore atomic per parent:
//! either the parent's events plus its reconcile both commit, or neither does.
//! So an interrupted run self-heals on re-run, and no parent is ever left with
//! updated parent attribution but a stale read model.
//! Dependent stale-competitor cascades run after each parent's commit, exactly
//! as `capture_in_txn` does.
//!
//! Child-pool attribution includes legacy child script tags, Namecoin/Syscoin/
//! Fractal chain-native payout/reward addresses decoded from stored child
//! coinbase outputs, and Hathor reward addresses decoded from stored
//! `hathor_merge_mining_evidence.funds_graph` bytes. Older rows without the
//! relevant raw bytes still require raw-block/RFC fixture replay.
//!
//! The pure planning layer (candidate decoding, pool resolution, and the
//! per-event write decisions) lives in [`plan`]; this module keeps the SQL/IO
//! (`candidate_batch_sql`, `load_candidate_batch`, `apply_parent_group`) and
//! the `run_reclassify_pools` driver that pairs each planned parent group with
//! that parent's read-model reconcile.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::Client;
use tracing::{debug, info};

use crate::chains::child_payout_registry::seed_all_child_payout_identities;
use crate::chains::elastos::identity::{
    upsert_elastos_minerinfo_pool_identities, upsert_elastos_reward_address_pool_identities,
};
use crate::chains::elastos::identity_reresolve::reresolve_elastos_identity_attributions;
use crate::chains::hathor::identity::upsert_hathor_reward_pool_identities;
use crate::chains::hathor::reward::HATHOR_REWARD_ADDRESS_NAMESPACE;
use crate::chains::hathor::reward_replay::replay_hathor_reward_attributions;
use crate::chains::rsk::identity_reresolve::reresolve_rsk_miner_identities;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::capture::{
    BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE, BTC_POOL_SNAPSHOT_SOURCE,
    CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE, PoolAttributionSide,
};
use mmm_capture::child_payout::{
    FRACTAL_REWARD_ADDRESS_NAMESPACE, NAMECOIN_PAYOUT_ADDRESS_NAMESPACE, PoolIdentityLookup,
    SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
};
use mmm_capture::pool_resolver::{PoolIdentityRegistry, PoolResolver};
use mmm_capture::source_registry::{
    FRACTAL_SOURCE_CODE, NAMECOIN_SOURCE_CODE, SYSCOIN_SOURCE_CODE,
};
use mmm_read_model::{drive_args, require_positive, update_parent_events};
use mmm_store::{
    delete_event_pool_attributions_for_source, get_source_id, load_pool_identities_by_namespace,
    upsert_event_pool_attributions, upsert_event_pool_attributions_without_stale_cleanup,
    upsert_pool_snapshot, upsert_rsk_only_pools,
};

mod plan;
use plan::{ParentUpdateGroup, plan_batch_updates};

const DEFAULT_BATCH_SIZE: i64 = 500;
const EVENT_POOL_ATTRIBUTION_JSON_ROWS_SELECT: &str = r"
   SELECT COALESCE(
       jsonb_agg(
           jsonb_build_object(
               'source', a.source,
               'namespace', a.namespace,
               'match_kind', a.match_kind,
               'matched_value', a.matched_value,
               'pool_id', a.pool_id,
               'pool_identity_id', a.pool_identity_id,
               'confidence', a.confidence,
               'details', a.details
           )
           ORDER BY a.namespace, a.matched_value
       ),
       '[]'::jsonb
   ) AS rows
   FROM event_pool_attribution a";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclassifyPoolsConfig {
    /// When true, also replace an existing BTC-coinbase-derived attribution with
    /// a newly resolved one. Default false (fill-missing-only).
    pub overwrite: bool,
    /// Keyset page size: rows loaded and planned per `(id)`-ordered batch.
    /// Bounds peak memory; must be positive.
    pub batch_size: i64,
    /// Phase gates, all `true` by default. `--only <phase>` runs exactly one;
    /// `--skip-<phase>` drops one (the two are mutually exclusive). Seeding always
    /// runs regardless; these gate only the four work phases.
    pub run_rsk: bool,
    pub run_main: bool,
    pub run_hathor: bool,
    pub run_elastos: bool,
    /// Optional `--source <code>` filter for the main candidate scan only (the
    /// RSK/Hathor/Elastos tails are already source-scoped). `None` = all sources.
    pub source: Option<String>,
}

impl Default for ReclassifyPoolsConfig {
    fn default() -> Self {
        Self {
            overwrite: false,
            batch_size: DEFAULT_BATCH_SIZE,
            run_rsk: true,
            run_main: true,
            run_hathor: true,
            run_elastos: true,
            source: None,
        }
    }
}

impl ReclassifyPoolsConfig {
    /// Parse argv. `--batch-size` keeps bespoke error wording that predates the
    /// shared `cli_args` helpers (pinned by the `from_args` tests); the value
    /// must be positive.
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut config = Self::default();
        let mut only_phase: Option<String> = None;
        let mut any_skip = false;
        drive_args("reclassify-pools", args, |flag, cur| {
            Ok(match flag {
                "--overwrite" => {
                    config.overwrite = true;
                    true
                }
                "--batch-size" => {
                    // Bespoke wording predating the shared helpers; pinned by
                    // the from_args tests below.
                    let value = cur
                        .raw_value()
                        .with_context(|| "--batch-size needs a value".to_owned())?;
                    config.batch_size = value
                        .parse()
                        .with_context(|| format!("--batch-size {value:?} must be an integer"))?;
                    true
                }
                "--source" => {
                    config.source = Some(cur.value("--source")?.to_owned());
                    true
                }
                "--only" => {
                    only_phase = Some(cur.value("--only")?.to_owned());
                    true
                }
                "--skip-rsk" => {
                    config.run_rsk = false;
                    any_skip = true;
                    true
                }
                "--skip-main" => {
                    config.run_main = false;
                    any_skip = true;
                    true
                }
                "--skip-hathor" => {
                    config.run_hathor = false;
                    any_skip = true;
                    true
                }
                "--skip-elastos" => {
                    config.run_elastos = false;
                    any_skip = true;
                    true
                }
                _ => false,
            })
        })?;
        require_positive(config.batch_size, "--batch-size")?;

        // `--only` names the single phase to run; `--skip-<phase>` drops phases.
        // Mixing the two is ambiguous, so reject it.
        if let Some(phase) = &only_phase {
            if any_skip {
                bail!("reclassify-pools: --only and --skip-<phase> are mutually exclusive");
            }
            config.run_rsk = false;
            config.run_main = false;
            config.run_hathor = false;
            config.run_elastos = false;
            match phase.as_str() {
                "rsk" => config.run_rsk = true,
                "main" => config.run_main = true,
                "hathor" => config.run_hathor = true,
                "elastos" => config.run_elastos = true,
                other => bail!(
                    "reclassify-pools: unknown --only phase {other:?} \
                     (expected one of: rsk, main, hathor, elastos)"
                ),
            }
        }

        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReclassifyPoolsStats {
    /// Events whose parent-side attribution was set/changed.
    pub parent_pool_updates: usize,
    /// Events whose child-side attribution was set/changed.
    pub child_pool_updates: usize,
    /// Distinct parent header hashes whose read model was reconciled.
    pub parents_reconciled: usize,
    /// Rows whose stored `btc_parent_coinbase_outputs` blob failed to
    /// deserialize and were skipped for the address fallback (best-effort: the
    /// row is treated as unresolved, never erased).
    pub corrupt_outputs_skipped: usize,
    /// Rows whose stored `child_coinbase_outputs` blob failed to deserialize
    /// and were skipped for child payout replay.
    pub corrupt_child_outputs_skipped: usize,
    /// Hathor events whose reward-address attribution rows were set or upgraded
    /// from persisted sidecar `funds_graph` bytes.
    pub hathor_reward_updates: usize,
    /// Hathor sidecar rows whose parsed reward audit JSON was set or refreshed.
    pub hathor_reward_audit_updates: usize,
    /// Rows whose stored Hathor `funds_graph` could not be parsed and were
    /// skipped for reward-address replay.
    pub corrupt_hathor_funds_graph_skipped: usize,
    /// Elastos child identity attributions promoted from unmapped to a registry
    /// pool by the no-RPC re-resolution tail (their `matched_value` matched a
    /// newly-seeded `pool_identity`).
    pub elastos_identity_updates: usize,
    /// RSK sidecar rows scanned by the no-RPC miner-address reclassification
    /// tail.
    pub rsk_miner_rows_scanned: usize,
    /// Scanned RSK miner rows that matched the embedded miner registry.
    pub rsk_miner_registry_resolved_rows: usize,
    /// Scanned RSK miner rows that remain unmapped but retain observed
    /// `rsk_rpc_miner` attribution.
    pub rsk_miner_unresolved_rows: usize,
    /// RSK child-side miner attribution rows set, promoted, or overwritten.
    pub rsk_miner_attribution_updates: usize,
    /// RSK sidecar `pool_identity_id` values late-filled from NULL.
    pub rsk_miner_sidecar_late_fills: usize,
}

/// Apply one parent group atomically through the read-model mutation module:
/// write every event's attribution rows and (when parent attribution changed)
/// reconcile the parent's read model IN THE SAME transaction, commit, then
/// cascade dependents. The module owns lock ordering, source-health ownership,
/// and the bounded lock-set-change retry, so this path can no longer diverge
/// from the capture/revoke/restore paths.
async fn apply_parent_group(client: &mut Client, group: &ParentUpdateGroup) -> Result<()> {
    // Reconcile the parent's read model from any of its now-updated events (all
    // share `btc_parent_header_hash`, so any one anchors the rebuild). Disabled
    // classifier: offline, preserves persisted classification. A pool-only
    // group with no parent-pool change skips the reconcile (and the cascade).
    let reconcile_anchor = if group.parent_rollup_changed {
        Some(
            group
                .updates
                .iter()
                .find(|update| update.parent_pool_changed)
                .map(|update| update.event_id)
                .expect("parent_rollup_changed implies a parent-side update"),
        )
    } else {
        None
    };

    update_parent_events(
        client,
        &ConfiguredParentClassifier::Disabled,
        &group.parent_hash,
        async |txn| {
            for update in &group.updates {
                if update.parent_pool_changed {
                    delete_event_pool_attributions_for_source(
                        txn,
                        update.event_id,
                        PoolAttributionSide::BtcParent,
                        BTC_POOL_SNAPSHOT_SOURCE,
                    )
                    .await?;
                }
                if update.child_pool_changed {
                    delete_event_pool_attributions_for_source(
                        txn,
                        update.event_id,
                        PoolAttributionSide::ChildBlock,
                        BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE,
                    )
                    .await?;
                }
                upsert_event_pool_attributions(
                    txn,
                    update.event_id,
                    &update.attributions,
                    update.observed_at,
                )
                .await?;
                upsert_event_pool_attributions_without_stale_cleanup(
                    txn,
                    update.event_id,
                    &update.child_payout_attributions,
                    update.observed_at,
                )
                .await?;
            }
            Ok(())
        },
        reconcile_anchor,
        "reclassify-pools parent",
    )
    .await
}

/// Keyset-paged candidate scan. Column order is a contract with
/// `plan::plan_batch_updates`/`attribution_state_from_row` (positional
/// `row.get`). The two parent/child LATERAL subqueries roll each side's
/// source-scoped attribution up to a single row (NULL fields when not exactly
/// one); the third aggregates child payout rows to JSON. Params: `$1` keyset
/// cursor (id), `$2` limit, `$3` overwrite (false = fill-missing-only, which
/// adds the `NOT EXISTS` provenance-gap filters), `$4`/`$5` snapshot/legacy-child
/// sources, `$6`/`$7` child payout namespaces/sources, `$8` payout-eligible
/// source codes, `$9` optional `source_id` filter (NULL = all sources). Scoped
/// to non-revoked events with usable evidence, ordered by `e.id` for resumable
/// paging.
fn candidate_batch_sql() -> String {
    let parent_attr = source_scoped_attribution_lateral("parent_attr", "btc_parent", "$4");
    let child_attr = source_scoped_attribution_lateral("child_attr", "child_block", "$5");

    format!(
        r"
SELECT e.id, e.btc_parent_header_hash, s.code AS source_code,
       parent_attr.row_count AS parent_attribution_row_count,
       parent_attr.namespace AS parent_attribution_namespace,
       parent_attr.match_kind AS parent_attribution_match_kind,
       parent_attr.matched_value AS parent_attribution_matched_value,
       parent_attr.pool_id AS parent_attribution_pool_id,
       parent_attr.pool_identity_id AS parent_attribution_pool_identity_id,
       parent_attr.confidence AS parent_attribution_confidence,
       parent_attr.details AS parent_attribution_details,
       child_attr.row_count AS child_attribution_row_count,
       child_attr.namespace AS child_attribution_namespace,
       child_attr.match_kind AS child_attribution_match_kind,
       child_attr.matched_value AS child_attribution_matched_value,
       child_attr.pool_id AS child_attribution_pool_id,
       child_attr.pool_identity_id AS child_attribution_pool_identity_id,
       child_attr.confidence AS child_attribution_confidence,
       child_attr.details AS child_attribution_details,
       child_payout_attr.rows AS child_payout_attribution_rows,
       btc_parent_coinbase_script, btc_parent_coinbase_outputs,
       child_coinbase_script, child_coinbase_outputs, confirmed_at
FROM merge_mining_event e
JOIN source s ON s.id = e.source_id
{parent_attr}
{child_attr}
LEFT JOIN LATERAL ({EVENT_POOL_ATTRIBUTION_JSON_ROWS_SELECT}
   WHERE a.event_id = e.id
     AND a.side = 'child_block'
     AND a.namespace = ANY($6::text[])
     AND a.source = ANY($7::text[])
) child_payout_attr ON true
WHERE e.revoked_at IS NULL
  AND (e.btc_parent_coinbase_script IS NOT NULL
       OR e.child_coinbase_script IS NOT NULL
       OR (s.code = ANY($8::text[]) AND e.child_coinbase_outputs IS NOT NULL))
  AND ($3
       OR (e.btc_parent_coinbase_script IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM event_pool_attribution a
               WHERE a.event_id = e.id
                 AND a.side = 'btc_parent'
                 AND a.source = $4
           ))
       OR (e.child_coinbase_script IS NOT NULL
           AND NOT EXISTS (
               SELECT 1 FROM event_pool_attribution a
               WHERE a.event_id = e.id
                 AND a.side = 'child_block'
                 AND a.source = $5
           ))
       OR (s.code = ANY($8::text[]) AND e.child_coinbase_outputs IS NOT NULL))
  AND ($1::bigint IS NULL OR e.id > $1)
  AND ($9::bigint IS NULL OR e.source_id = $9)
ORDER BY e.id
LIMIT $2"
    )
}

fn source_scoped_attribution_lateral(alias: &str, side: &str, source_param: &str) -> String {
    format!(
        r"LEFT JOIN LATERAL (
   SELECT count(*)::bigint AS row_count,
          CASE WHEN count(*) = 1 THEN min(a.namespace) ELSE NULL END AS namespace,
          CASE WHEN count(*) = 1 THEN min(a.match_kind) ELSE NULL END AS match_kind,
          CASE WHEN count(*) = 1 THEN min(a.matched_value) ELSE NULL END AS matched_value,
          CASE WHEN count(*) = 1 THEN min(a.pool_id) ELSE NULL END AS pool_id,
          CASE WHEN count(*) = 1 THEN min(a.pool_identity_id) ELSE NULL END AS pool_identity_id,
          CASE WHEN count(*) = 1 THEN min(a.confidence) ELSE NULL END AS confidence,
          CASE WHEN count(*) = 1 THEN (array_agg(a.details ORDER BY a.id))[1] ELSE NULL END AS details
   FROM event_pool_attribution a
   WHERE a.event_id = e.id
     AND a.side = '{side}'
     AND a.source = {source_param}
) {alias} ON true"
    )
}

/// Load one keyset page of candidate rows after `cursor` (exclusive). Binds the
/// fixed child payout namespace/source/source-code sets, the mode flags from
/// `config`, and the optional `source_filter` (`$9`, `None` = all sources), then
/// runs `candidate_batch_sql`. Returns raw rows; decoding and planning happen in
/// `plan::plan_batch_updates`.
async fn load_candidate_batch(
    client: &Client,
    cursor: Option<i64>,
    config: &ReclassifyPoolsConfig,
    source_filter: Option<i64>,
) -> Result<Vec<tokio_postgres::Row>> {
    // In fill-missing-only mode ($3 = false), scan rows whose source-scoped
    // attribution provenance has not been materialized yet. `--overwrite` scans
    // all rows with usable evidence because it may re-attribute an already-set
    // row and should refresh provenance at the same time.
    let child_payout_namespaces = vec![
        NAMECOIN_PAYOUT_ADDRESS_NAMESPACE.to_owned(),
        SYSCOIN_PAYOUT_ADDRESS_NAMESPACE.to_owned(),
        FRACTAL_REWARD_ADDRESS_NAMESPACE.to_owned(),
    ];
    let child_payout_sources = vec![
        CHILD_COINBASE_OUTPUT_SOURCE.to_owned(),
        CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
    ];
    let child_payout_source_codes = vec![
        NAMECOIN_SOURCE_CODE.to_owned(),
        SYSCOIN_SOURCE_CODE.to_owned(),
        FRACTAL_SOURCE_CODE.to_owned(),
    ];
    let sql = candidate_batch_sql();
    client
        .query(
            &sql,
            &[
                &cursor,
                &config.batch_size,
                &config.overwrite,
                &BTC_POOL_SNAPSHOT_SOURCE,
                &BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE,
                &child_payout_namespaces,
                &child_payout_sources,
                &child_payout_source_codes,
                &source_filter,
            ],
        )
        .await
        .context("load reclassify-pools candidate batch")
}

/// Self-seed the embedded resolver, RSK-only pool slugs, and the Hathor reward,
/// Elastos minerinfo, Elastos reward-address, and Namecoin/Syscoin/Fractal
/// child-payout identity registries, then load the child payout identity lookup.
/// All idempotent `ON CONFLICT` upserts, safe alongside live producers. Returns the
/// resolver, registry, slug map, and identity lookup the phases read.
async fn seed_reclassify_pools(
    client: &mut Client,
) -> Result<(
    PoolResolver,
    PoolIdentityRegistry,
    HashMap<String, i64>,
    PoolIdentityLookup,
)> {
    let resolver = PoolResolver::from_default_snapshot()
        .context("load embedded pool snapshot for reclassify-pools")?;
    let registry = PoolIdentityRegistry::from_default_rsk_registry()
        .context("load embedded RSK miner registry for reclassify-pools")?;
    let mut pool_ids_by_slug = upsert_pool_snapshot(client, resolver.snapshot())
        .await
        .context("seed expanded pool snapshot")?;
    upsert_rsk_only_pools(client, &registry, &mut pool_ids_by_slug)
        .await
        .context("seed RSK-only pool slugs")?;
    upsert_hathor_reward_pool_identities(client, &mut pool_ids_by_slug)
        .await
        .context("seed Hathor reward pool identities")?;
    upsert_elastos_minerinfo_pool_identities(client, &mut pool_ids_by_slug)
        .await
        .context("seed Elastos minerinfo pool identities")?;
    upsert_elastos_reward_address_pool_identities(client, &mut pool_ids_by_slug)
        .await
        .context("seed Elastos reward-address pool identities")?;
    seed_all_child_payout_identities(client, &mut pool_ids_by_slug)
        .await
        .context("seed Namecoin/Syscoin/Fractal child-payout pool identities")?;
    let child_payout_identities = load_pool_identities_by_namespace(
        client,
        &[
            NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
            SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
            FRACTAL_REWARD_ADDRESS_NAMESPACE,
            HATHOR_REWARD_ADDRESS_NAMESPACE,
        ],
    )
    .await
    .context("load child payout pool identities")?;
    Ok((
        resolver,
        registry,
        pool_ids_by_slug,
        child_payout_identities,
    ))
}

/// Run the main keyset candidate scan: optionally `--source`-scoped, paged by
/// `(id)` ascending, applying each parent group transactionally through the
/// read-model mutation module. Extracted from `run_reclassify_pools` so the
/// driver stays a thin phase orchestrator.
async fn run_main_candidate_scan(
    client: &mut Client,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
    child_payout_identities: &PoolIdentityLookup,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Result<()> {
    // Optional `--source` filter resolves to a `source_id` and bounds the
    // candidate scan to that one source; the tails are already source-scoped.
    let source_filter = match &config.source {
        Some(code) => Some(
            get_source_id(client, code)
                .await
                .with_context(|| format!("resolve --source {code:?}"))?,
        ),
        None => None,
    };

    // Page by (id) ascending. Within each batch, the planned updates are grouped
    // by parent header hash and applied + reconciled in one transaction per
    // parent, so peak memory stays bounded by `batch_size`. A parent split across
    // two batches gets one atomic transaction per batch-portion (each pairing its
    // events with a reconcile); read-model reconciliation is idempotent, so the
    // second reconcile is harmless.
    info!(
        phase = "main_scan",
        source = ?config.source,
        "reclassify-pools: starting main candidate scan"
    );
    let mut cursor: Option<i64> = None;
    let mut batch_index: u64 = 0;

    loop {
        let rows = load_candidate_batch(client, cursor, config, source_filter).await?;
        if rows.is_empty() {
            break;
        }
        cursor = Some(rows.last().expect("non-empty batch").get(0));
        batch_index += 1;
        if batch_index.is_multiple_of(50) {
            debug!(
                phase = "main_scan",
                batch_index,
                cursor = ?cursor,
                parent_pool_updates = stats.parent_pool_updates,
                child_pool_updates = stats.child_pool_updates,
                "reclassify-pools: main candidate scan progress"
            );
        }

        let groups = plan_batch_updates(
            &rows,
            resolver,
            pool_ids_by_slug,
            child_payout_identities,
            config,
            stats,
        );

        // Apply each parent group transactionally through the read-model mutation
        // module: parent attribution writes and that parent's read-model reconcile
        // commit together, exactly like producer capture, and the module cascades
        // dependent stale competitors after the commit. Stale competition miner
        // detail is derived from block rows, so a Bitcoin miner change does not
        // require a competition-cache refresh. A failure rolls the whole group
        // back so an interrupted run self-heals on re-run.
        for group in &groups {
            apply_parent_group(client, group).await?;
            for update in &group.updates {
                if update.parent_pool_changed {
                    stats.parent_pool_updates += 1;
                }
                if update.child_pool_changed || update.child_payout_changed {
                    stats.child_pool_updates += 1;
                }
            }
            if group.parent_rollup_changed {
                stats.parents_reconciled += 1;
            }
        }
    }

    info!(
        phase = "main_scan",
        batches = batch_index,
        parent_pool_updates = stats.parent_pool_updates,
        child_pool_updates = stats.child_pool_updates,
        parents_reconciled = stats.parents_reconciled,
        "reclassify-pools: main candidate scan complete"
    );
    Ok(())
}

/// Run the offline historical pool re-resolution. Self-seeds the embedded pool
/// snapshot, RSK-only slugs, and the Hathor reward + Elastos minerinfo identities
/// so slug lookups resolve. It first runs the RSK miner-address sidecar tail
/// (from persisted `rsk_miner`) so registry remap conflicts fail before any
/// parent/child coinbase updates, then keyset-pages candidates, planning and
/// applying one atomic transaction per parent header hash, and finishes with
/// the no-RPC Hathor reward-address replay (re-derives from stored `funds_graph`)
/// plus Elastos identity promotion (promotes registry-matchable existing
/// attributions by their stored `matched_value`, since Elastos stores no child
/// coinbase outputs to re-derive from). Idempotent and resumable: a re-run with
/// no registry change is a no-op.
pub async fn run_reclassify_pools(
    client: &mut Client,
    config: ReclassifyPoolsConfig,
) -> Result<ReclassifyPoolsStats> {
    // Self-seed the embedded resolver + registries so slug/identity lookups
    // resolve before any phase scans. Idempotent, safe alongside live producers.
    let (resolver, registry, pool_ids_by_slug, child_payout_identities) =
        seed_reclassify_pools(client).await?;

    info!(
        phase = "seed",
        pools = pool_ids_by_slug.len(),
        "reclassify-pools: seeded pool snapshot and identity registries"
    );

    let mut stats = ReclassifyPoolsStats::default();

    if config.run_rsk {
        info!(
            phase = "rsk",
            "reclassify-pools: starting RSK miner-identity pass"
        );
        reresolve_rsk_miner_identities(client, &registry, &pool_ids_by_slug, &config, &mut stats)
            .await?;
        info!(
            phase = "rsk",
            rows_scanned = stats.rsk_miner_rows_scanned,
            registry_resolved = stats.rsk_miner_registry_resolved_rows,
            attribution_updates = stats.rsk_miner_attribution_updates,
            sidecar_late_fills = stats.rsk_miner_sidecar_late_fills,
            "reclassify-pools: RSK miner-identity pass complete"
        );
    } else {
        info!(
            phase = "rsk",
            "reclassify-pools: skipping RSK miner-identity pass (gated by --only/--skip)"
        );
    }

    if config.run_main {
        run_main_candidate_scan(
            client,
            &resolver,
            &pool_ids_by_slug,
            &child_payout_identities,
            &config,
            &mut stats,
        )
        .await?;
    } else {
        info!(
            phase = "main_scan",
            "reclassify-pools: skipping main candidate scan (gated by --only/--skip)"
        );
    }

    if config.run_hathor {
        info!(
            phase = "hathor",
            "reclassify-pools: starting Hathor reward replay"
        );
        replay_hathor_reward_attributions(client, &child_payout_identities, &config, &mut stats)
            .await?;
        info!(
            phase = "hathor",
            reward_updates = stats.hathor_reward_updates,
            audit_updates = stats.hathor_reward_audit_updates,
            "reclassify-pools: Hathor reward replay complete"
        );
    } else {
        info!(
            phase = "hathor",
            "reclassify-pools: skipping Hathor reward replay (gated by --only/--skip)"
        );
    }

    // Elastos stores no child coinbase outputs, so its identity is not
    // re-derivable from the candidate scan above; instead promote
    // registry-matchable existing Elastos attributions by their stored
    // matched_value (no RPC).
    if config.run_elastos {
        info!(
            phase = "elastos",
            "reclassify-pools: starting Elastos identity re-resolution"
        );
        reresolve_elastos_identity_attributions(client, &config, &mut stats).await?;
        info!(
            phase = "elastos",
            identity_updates = stats.elastos_identity_updates,
            "reclassify-pools: Elastos identity re-resolution complete"
        );
    } else {
        info!(
            phase = "elastos",
            "reclassify-pools: skipping Elastos identity re-resolution (gated by --only/--skip)"
        );
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_args_parses_overwrite_and_batch_size() {
        let config =
            ReclassifyPoolsConfig::from_args(["--overwrite", "--batch-size", "250"]).unwrap();
        assert!(config.overwrite);
        assert_eq!(config.batch_size, 250);
    }

    #[test]
    fn from_args_defaults_to_fill_null_only() {
        let config = ReclassifyPoolsConfig::from_args(Vec::<String>::new()).unwrap();
        assert!(!config.overwrite);
        assert_eq!(config.batch_size, DEFAULT_BATCH_SIZE);
    }

    #[test]
    fn from_args_rejects_unknown_and_bad_batch_size() {
        assert!(ReclassifyPoolsConfig::from_args(["--nope"]).is_err());
        assert!(ReclassifyPoolsConfig::from_args(["--batch-size", "0"]).is_err());
        assert!(ReclassifyPoolsConfig::from_args(["--batch-size", "x"]).is_err());
    }

    #[test]
    fn from_args_pins_bespoke_batch_size_error_text() {
        let missing = ReclassifyPoolsConfig::from_args(["--batch-size"]).unwrap_err();
        assert_eq!(missing.to_string(), "--batch-size needs a value");
        let non_integer = ReclassifyPoolsConfig::from_args(["--batch-size", "x"]).unwrap_err();
        assert_eq!(
            non_integer.to_string(),
            "--batch-size \"x\" must be an integer"
        );
    }

    #[test]
    fn from_args_defaults_run_all_phases_with_no_source() {
        let config = ReclassifyPoolsConfig::from_args(Vec::<String>::new()).unwrap();
        assert!(config.run_rsk && config.run_main && config.run_hathor && config.run_elastos);
        assert_eq!(config.source, None);
    }

    #[test]
    fn from_args_only_runs_exactly_one_phase() {
        let config = ReclassifyPoolsConfig::from_args(["--only", "main"]).unwrap();
        assert!(!config.run_rsk);
        assert!(config.run_main);
        assert!(!config.run_hathor);
        assert!(!config.run_elastos);
    }

    #[test]
    fn from_args_skip_drops_named_phases_only() {
        let config = ReclassifyPoolsConfig::from_args(["--skip-rsk", "--skip-hathor"]).unwrap();
        assert!(!config.run_rsk);
        assert!(config.run_main);
        assert!(!config.run_hathor);
        assert!(config.run_elastos);
    }

    #[test]
    fn from_args_parses_source_filter() {
        let config = ReclassifyPoolsConfig::from_args(["--source", "namecoin"]).unwrap();
        assert_eq!(config.source.as_deref(), Some("namecoin"));
    }

    #[test]
    fn from_args_rejects_only_combined_with_skip() {
        let err = ReclassifyPoolsConfig::from_args(["--only", "main", "--skip-rsk"]).unwrap_err();
        assert!(format!("{err:#}").contains("mutually exclusive"));
    }

    #[test]
    fn from_args_rejects_unknown_only_phase() {
        let err = ReclassifyPoolsConfig::from_args(["--only", "bogus"]).unwrap_err();
        assert!(format!("{err:#}").contains("unknown --only phase"));
    }

    #[test]
    fn from_args_requires_only_and_source_values() {
        assert!(ReclassifyPoolsConfig::from_args(["--only"]).is_err());
        assert!(ReclassifyPoolsConfig::from_args(["--source"]).is_err());
    }
}
