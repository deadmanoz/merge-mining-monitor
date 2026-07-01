//! Hathor reward-address replay for `reclassify-pools`.
//!
//! Re-derives child reward attributions for already-captured Hathor events from
//! their persisted `funds_graph` sidecar, picking up registry additions and slug
//! renames without re-fetching the chain. Keyset-paginated over `event_id`;
//! reuses the offline reward parser and the shared `ExistingAttributionSet` /
//! `WritePolicy` so it never clobbers manual/higher-trust attributions.

use anyhow::Result;
use tokio_postgres::Client;
use tracing::warn;

use crate::chains::hathor::reward::{HATHOR_REWARD_ADDRESS_NAMESPACE, parse_hathor_reward_outputs};
use crate::reclassify_pools::{ReclassifyPoolsConfig, ReclassifyPoolsStats};
use mmm_capture::attribution_policy::{ExistingAttributionSet, WritePolicy};
use mmm_capture::capture::{CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::source_registry::HATHOR_SOURCE_CODE;
use mmm_store::{
    get_source_id, load_hathor_reward_replay_batch, update_hathor_reward_audit,
    upsert_event_pool_attributions_without_stale_cleanup,
};

/// Replay child reward attributions for every captured Hathor event, in
/// keyset-paginated `event_id` batches. Each row's `funds_graph` is re-parsed
/// (a corrupt one is counted and skipped, leaving the row untouched), the audit
/// JSON is refreshed, and the recomputed attributions are filtered through the
/// row's existing set under the child-payout write policy before upsert (no
/// stale cleanup, so other sources' attributions survive). Bumps `stats`.
pub(crate) async fn replay_hathor_reward_attributions(
    client: &Client,
    child_payout_identities: &PoolIdentityLookup,
    config: &ReclassifyPoolsConfig,
    stats: &mut ReclassifyPoolsStats,
) -> Result<()> {
    let source_id = get_source_id(client, HATHOR_SOURCE_CODE).await?;
    let sources = [CHILD_COINBASE_OUTPUT_SOURCE, CHILD_PAYOUT_REGISTRY_SOURCE];
    let mut cursor: Option<i64> = None;

    loop {
        let rows = load_hathor_reward_replay_batch(
            client,
            source_id,
            cursor,
            config.batch_size,
            HATHOR_REWARD_ADDRESS_NAMESPACE,
            &sources,
        )
        .await?;
        if rows.is_empty() {
            break;
        }
        cursor = rows.last().map(|row| row.event_id);

        for row in rows {
            let parsed = match parse_hathor_reward_outputs(&row.funds_graph, row.funds_graph_split)
            {
                Ok(parsed) => parsed,
                Err(err) => {
                    stats.corrupt_hathor_funds_graph_skipped += 1;
                    warn!(
                        event_id = row.event_id,
                        error = %err,
                        "skipping corrupt Hathor funds_graph for reward-address replay; \
                         leaving child reward attribution unchanged for this row"
                    );
                    continue;
                }
            };

            let reward_output_details = parsed.output_details_json();
            let reward_addresses = parsed.reward_addresses_json();
            if update_hathor_reward_audit(
                client,
                row.event_id,
                &reward_output_details,
                &reward_addresses,
            )
            .await?
            {
                stats.hathor_reward_audit_updates += 1;
            }

            let existing = ExistingAttributionSet::from_json(&row.existing_attributions);
            let attributions = parsed
                .reward_attributions(child_payout_identities)
                .into_iter()
                .filter(|attribution| {
                    existing.should_write(
                        attribution,
                        WritePolicy::ChildPayout {
                            overwrite: config.overwrite,
                        },
                    )
                })
                .collect::<Vec<_>>();
            if attributions.is_empty() {
                continue;
            }

            upsert_event_pool_attributions_without_stale_cleanup(
                client,
                row.event_id,
                &attributions,
                row.confirmed_at,
            )
            .await?;
            stats.child_pool_updates += 1;
            stats.hathor_reward_updates += 1;
        }
    }

    Ok(())
}
