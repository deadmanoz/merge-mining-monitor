//! Offline historical pool re-resolution (`reclassify-pools`).
//!
//! Seeds the embedded `PoolResolver`, RSK-only slugs, and child identity
//! registries, then runs five phase-gated passes: RSK miner-address
//! re-resolution, keyset-paged coinbase/payout replay (`plan`), Hathor
//! `funds_graph` reward replay, Elastos identity promotion, and the pools of
//! observations no event backs (`observations`).
//! Parent-group attribution updates and their read-model reconciliation commit
//! atomically per parent via `mmm_read_model::update_parent_events` (classifier
//! disabled), with fill-missing-only by default and `--overwrite` to refresh
//! resolved attributions without erasing unresolved rows.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::Client;
use tracing::info;

use crate::chains::child_payout_registry::{
    apply_child_payout_confidence, seed_all_child_payout_identities,
};
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
    QBIT_PAYOUT_ADDRESS_NAMESPACE, SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
};
use mmm_capture::pool_resolver::{PoolIdentityRegistry, PoolResolver};
use mmm_capture::progress::ProgressReporter;
use mmm_capture::source_registry::{
    FRACTAL_SOURCE_CODE, NAMECOIN_SOURCE_CODE, QBIT_SOURCE_CODE, SYSCOIN_SOURCE_CODE,
};
use mmm_read_model::{drive_args, require_positive, update_parent_events};
use mmm_store::{
    delete_event_pool_attributions_for_source, get_source_id, load_pool_identities_by_namespace,
    sync_event_observation_attribution, upsert_event_pool_attributions,
    upsert_event_pool_attributions_without_stale_cleanup, upsert_pool_snapshot,
    upsert_rsk_only_pools, upsert_rsk_pool_identities_with_policy,
};

mod observations;
mod plan;
use observations::reresolve_observation_pools;
use plan::{ParentUpdateGroup, plan_batch_updates};

const DEFAULT_BATCH_SIZE: i64 = 500;
const CHILD_PAYOUT_SOURCE_CODES: &[&str] = &[
    NAMECOIN_SOURCE_CODE,
    SYSCOIN_SOURCE_CODE,
    FRACTAL_SOURCE_CODE,
    QBIT_SOURCE_CODE,
];
// Shared by the initial count and every page so progress measures the same
// eligible events the scan processes. Params: overwrite, parent source,
// child-tag source, payout source codes, optional source ID.
const CANDIDATE_FILTER_SQL: &str = r"e.revoked_at IS NULL
      AND e.btc_parent_kind <> 'near'
      AND (e.btc_parent_coinbase_script IS NOT NULL
           OR e.btc_parent_coinbase_outputs IS NOT NULL
           OR e.btc_parent_coinbase_outputs_text IS NOT NULL
           OR e.child_coinbase_script IS NOT NULL
           OR (s.code = ANY($4::text[]) AND e.child_coinbase_outputs IS NOT NULL))
      AND ($1
           OR (s.code = ANY($4::text[]) AND e.child_coinbase_outputs IS NOT NULL)
           OR ((e.btc_parent_coinbase_script IS NOT NULL
                OR e.btc_parent_coinbase_outputs IS NOT NULL
                OR e.btc_parent_coinbase_outputs_text IS NOT NULL)
               AND NOT EXISTS (
                   SELECT 1 FROM event_pool_attribution a
                   WHERE a.event_id = e.id
                     AND a.side = 'btc_parent'
                     AND a.source = $2
               ))
           OR (e.child_coinbase_script IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM event_pool_attribution a
                   WHERE a.event_id = e.id
                     AND a.side = 'child_block'
                     AND a.source = $3
               )))
      AND ($5::bigint IS NULL OR e.source_id = $5)";
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
    pub overwrite: bool,
    pub batch_size: i64,
    pub run_rsk: bool,
    pub run_main: bool,
    pub run_hathor: bool,
    pub run_elastos: bool,
    pub run_observations: bool,
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
            run_observations: true,
            source: None,
        }
    }
}

impl ReclassifyPoolsConfig {
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
                "--skip-observations" => {
                    config.run_observations = false;
                    any_skip = true;
                    true
                }
                _ => false,
            })
        })?;
        require_positive(config.batch_size, "--batch-size")?;

        if let Some(phase) = &only_phase {
            if any_skip {
                bail!("reclassify-pools: --only and --skip-<phase> are mutually exclusive");
            }
            config.run_rsk = false;
            config.run_main = false;
            config.run_hathor = false;
            config.run_elastos = false;
            config.run_observations = false;
            match phase.as_str() {
                "rsk" => config.run_rsk = true,
                "main" => config.run_main = true,
                "hathor" => config.run_hathor = true,
                "elastos" => config.run_elastos = true,
                "observations" => config.run_observations = true,
                other => bail!(
                    "reclassify-pools: unknown --only phase {other:?} \
                     (expected one of: rsk, main, hathor, elastos, observations)"
                ),
            }
        }

        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReclassifyPoolsStats {
    pub parent_pool_updates: usize,
    pub child_pool_updates: usize,
    pub parents_reconciled: usize,
    pub corrupt_parent_outputs: usize,
    pub corrupt_child_outputs_skipped: usize,
    pub hathor_reward_updates: usize,
    pub hathor_reward_audit_updates: usize,
    pub corrupt_hathor_funds_graph_skipped: usize,
    pub elastos_identity_updates: usize,
    pub rsk_miner_rows_scanned: usize,
    pub rsk_miner_registry_resolved_rows: usize,
    pub rsk_miner_unresolved_rows: usize,
    pub rsk_miner_attribution_updates: usize,
    pub rsk_miner_sidecar_late_fills: usize,
    pub observations_scanned: usize,
    pub observation_pool_updates: usize,
}

async fn apply_parent_group(client: &mut Client, group: &ParentUpdateGroup) -> Result<()> {
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
                sync_event_observation_attribution(txn, update.event_id).await?;
            }
            Ok(())
        },
        reconcile_anchor,
        "reclassify-pools parent",
    )
    .await
}

/// Keyset-paged candidate scan; column order must match `plan::plan_batch_updates`.
/// Materialize the page before loading attribution details so source-scoped scans
/// do not evaluate lateral aggregates for every remaining event.
fn candidate_batch_sql() -> String {
    let parent_attr = source_scoped_attribution_lateral("parent_attr", "btc_parent", "$2");
    let child_attr = source_scoped_attribution_lateral("child_attr", "child_block", "$3");

    format!(
        r"
WITH candidates AS MATERIALIZED (
    SELECT e.id, e.btc_parent_header_hash, s.code AS source_code,
           e.btc_parent_coinbase_script, e.btc_parent_coinbase_outputs,
           e.btc_parent_coinbase_outputs_text, e.child_coinbase_script,
           e.child_coinbase_outputs, e.confirmed_at
    FROM merge_mining_event e
    JOIN source s ON s.id = e.source_id
    WHERE {CANDIDATE_FILTER_SQL}
      AND ($6::bigint IS NULL OR e.id > $6)
    ORDER BY e.id
    LIMIT $7
)
SELECT e.id, e.btc_parent_header_hash, e.source_code,
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
       btc_parent_coinbase_outputs_text,
       child_coinbase_script, child_coinbase_outputs, confirmed_at
FROM candidates e
{parent_attr}
{child_attr}
LEFT JOIN LATERAL ({EVENT_POOL_ATTRIBUTION_JSON_ROWS_SELECT}
   WHERE a.event_id = e.id
     AND a.side = 'child_block'
     AND a.namespace = ANY($8::text[])
     AND a.source = ANY($9::text[])
) child_payout_attr ON true
ORDER BY e.id"
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

async fn load_candidate_batch(
    client: &Client,
    cursor: Option<i64>,
    config: &ReclassifyPoolsConfig,
    source_filter: Option<i64>,
) -> Result<Vec<tokio_postgres::Row>> {
    let child_payout_namespaces = vec![
        NAMECOIN_PAYOUT_ADDRESS_NAMESPACE.to_owned(),
        SYSCOIN_PAYOUT_ADDRESS_NAMESPACE.to_owned(),
        FRACTAL_REWARD_ADDRESS_NAMESPACE.to_owned(),
        QBIT_PAYOUT_ADDRESS_NAMESPACE.to_owned(),
    ];
    let child_payout_sources = vec![
        CHILD_COINBASE_OUTPUT_SOURCE.to_owned(),
        CHILD_PAYOUT_REGISTRY_SOURCE.to_owned(),
    ];
    let sql = candidate_batch_sql();
    client
        .query(
            &sql,
            &[
                &config.overwrite,
                &BTC_POOL_SNAPSHOT_SOURCE,
                &BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE,
                &CHILD_PAYOUT_SOURCE_CODES,
                &source_filter,
                &cursor,
                &config.batch_size,
                &child_payout_namespaces,
                &child_payout_sources,
            ],
        )
        .await
        .context("load reclassify-pools candidate batch")
}

/// Count initial eligible candidates without hydrating provenance.
async fn count_candidates(client: &Client, overwrite: bool, source: Option<i64>) -> Result<u64> {
    let sql = format!(
        "SELECT count(*) FROM merge_mining_event e JOIN source s ON s.id = e.source_id \
         WHERE {CANDIDATE_FILTER_SQL}"
    );
    let row = client
        .query_one(
            &sql,
            &[
                &overwrite,
                &BTC_POOL_SNAPSHOT_SOURCE,
                &BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE,
                &CHILD_PAYOUT_SOURCE_CODES,
                &source,
            ],
        )
        .await
        .context("count reclassify-pools candidates")?;
    Ok(u64::try_from(row.get::<_, i64>(0))?)
}

async fn seed_reclassify_pools(
    client: &mut Client,
    config: &ReclassifyPoolsConfig,
) -> Result<(
    PoolResolver,
    PoolIdentityRegistry,
    HashMap<String, i64>,
    PoolIdentityLookup,
    HashMap<String, i64>,
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
        .context("seed child-payout pool identities")?;
    let mut child_payout_identities = load_pool_identities_by_namespace(
        client,
        &[
            NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
            SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
            FRACTAL_REWARD_ADDRESS_NAMESPACE,
            QBIT_PAYOUT_ADDRESS_NAMESPACE,
            HATHOR_REWARD_ADDRESS_NAMESPACE,
        ],
    )
    .await
    .context("load child payout pool identities")?;
    apply_child_payout_confidence(&mut child_payout_identities, &pool_ids_by_slug)?;
    // The RSK and observation passes resolve RSK miners through these mappings.
    let rsk_identity_ids = if config.run_rsk || config.run_observations {
        upsert_rsk_pool_identities_with_policy(
            client,
            &registry,
            &pool_ids_by_slug,
            config.overwrite,
        )
        .await
        .context("seed RSK miner pool identities")?
    } else {
        HashMap::new()
    };
    Ok((
        resolver,
        registry,
        pool_ids_by_slug,
        child_payout_identities,
        rsk_identity_ids,
    ))
}

/// The `--source` filter as a source id.
async fn source_filter(client: &Client, config: &ReclassifyPoolsConfig) -> Result<Option<i64>> {
    match &config.source {
        Some(code) => Ok(Some(
            get_source_id(client, code)
                .await
                .with_context(|| format!("resolve --source {code:?}"))?,
        )),
        None => Ok(None),
    }
}

async fn run_main_candidate_scan(
    client: &mut Client,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
    child_payout_identities: &PoolIdentityLookup,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Result<()> {
    let progress = ProgressReporter::new("reclassify-pools-main", None);
    let source_filter = source_filter(client, config).await?;

    info!(
        phase = "main_scan",
        source = ?config.source,
        "reclassify-pools: starting main candidate scan"
    );
    let total = count_candidates(client, config.overwrite, source_filter).await?;
    let progress = progress.with_total(total);
    let mut cursor: Option<i64> = None;
    let mut batch_index: u64 = 0;

    loop {
        let rows = load_candidate_batch(client, cursor, config, source_filter).await?;
        if rows.is_empty() {
            break;
        }
        cursor = Some(rows.last().expect("non-empty batch").get(0));
        batch_index += 1;
        let groups = plan_batch_updates(
            &rows,
            resolver,
            pool_ids_by_slug,
            child_payout_identities,
            config,
            stats,
        );

        let updates = groups
            .iter()
            .map(|group| group.updates.len())
            .sum::<usize>();
        progress.advance((rows.len() - updates) as u64);

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
            progress.advance(group.updates.len() as u64);
        }
    }
    progress.finish();

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

pub async fn run_reclassify_pools(
    client: &mut Client,
    config: ReclassifyPoolsConfig,
) -> Result<ReclassifyPoolsStats> {
    let (resolver, registry, pool_ids_by_slug, child_payout_identities, rsk_identity_ids) =
        seed_reclassify_pools(client, &config).await?;

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
        reresolve_rsk_miner_identities(
            client,
            &registry,
            &pool_ids_by_slug,
            &rsk_identity_ids,
            &config,
            &mut stats,
        )
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

    if config.run_observations {
        reresolve_observation_pools(client, &resolver, &pool_ids_by_slug, &config, &mut stats)
            .await?;
    } else {
        info!(
            phase = "observations",
            "reclassify-pools: skipping observation pass (gated by --only/--skip)"
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
