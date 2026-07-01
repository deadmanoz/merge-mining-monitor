//! Bounded Elastos historical backfill. Intentionally divergent from the shared
//! family runner: dual-endpoint self-verifying capture, the range cap
//! foot-gun guard, and the per-height delay for the public endpoint. Env
//! reads route through `chains::config`; behavior unchanged.

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::chains::backfill::{BackfillConfig, BackfillHeightEffect, run_delayed_backfill_range};
use crate::chains::elastos::capture::{
    ElastosCaptureContext, ElastosHeightOutcome, process_elastos_height,
};
use crate::chains::elastos::rpc::ElastosRpcClient;
use crate::producer_runtime::{ProducerRuntime, run_post_backfill_repair};

/// Default per-height delay for the Elastos backfill: 0 (no throttle) unless
/// ELASTOS_RPC_BACKFILL_DELAY_MS is set (the public endpoint should be
/// throttled by the operator).
pub(crate) const ELASTOS_DEFAULT_BACKFILL_DELAY_MS: u64 = 0;

/// Registry-dispatched live-poll entry point for Elastos.
pub(crate) async fn poll(
    spec: &'static crate::chains::spec::ChainSpec,
    rt: ProducerRuntime,
) -> Result<()> {
    let rpc_config = crate::chains::config::elastos_rpc_config()?;
    let rpc = ElastosRpcClient::new(rpc_config)?;
    let poller_config = crate::chains::config::poller_config(spec)?;
    let context =
        ElastosCaptureContext::new_with_classifier(&rt.pg_client, rt.parent_classifier).await?;
    let poller = crate::poller::Poller::new(
        crate::chains::elastos::capture::ElastosChainPoller::new(rt.pg_client, rpc, context),
        poller_config,
    )
    .await?;
    poller.run_forever().await
}

/// Registry-dispatched backfill entry point for Elastos.
pub(crate) async fn backfill(rt: ProducerRuntime, config: BackfillConfig) -> Result<()> {
    let rpc_config = crate::chains::config::elastos_rpc_config()?;
    let rpc = ElastosRpcClient::new(rpc_config)?;
    run_elastos_backfill(rt, rpc, config).await
}

/// Drive a bounded Elastos backfill over `[start, end]`: validate the range against
/// the live tip, warn if it dips below the AuxPoW activation floor, self-verify and
/// process each height (per-request throttle for the public endpoint), then run the
/// post-backfill read-model repair over the range. Backfills never move
/// `poll_cursor`. Bails (no silent gaps) if a height hits the nBits-table horizon:
/// regenerate the table first.
pub(crate) async fn run_elastos_backfill(
    rt: ProducerRuntime,
    rpc: ElastosRpcClient,
    config: BackfillConfig,
) -> Result<()> {
    let ProducerRuntime {
        pg_client: mut client,
        parent_classifier,
    } = rt;
    let chain_tip = rpc
        .get_current_height()
        .await
        .context("get Elastos tip before backfill")?;
    config.validate_against_tip(chain_tip)?;

    if config.start_height < config.spec.activation_floor {
        warn!(
            start_height = config.start_height,
            first_auxpow_height = config.spec.activation_floor,
            "start-height precedes the Elastos AuxPoW activation floor; earlier heights are pre-activation dummies and skipped"
        );
    }

    // Per-request throttle: 0 for the local node, set for the public endpoint.
    let delay_ms: u64 = crate::chains::config::elastos_backfill_delay_ms();

    let context = ElastosCaptureContext::new_with_classifier(&client, parent_classifier).await?;
    info!(
        start_height = config.start_height,
        end_height = config.end_height,
        chain_tip,
        "starting bounded Elastos backfill"
    );

    let summary = run_delayed_backfill_range(&config, delay_ms, async |height| {
        let outcome = process_elastos_height(&mut client, &rpc, &context, height).await?;
        elastos_backfill_effect(height, outcome)
    })
    .await?;

    info!(
        processed = summary.processed,
        auxpow_written = summary.auxpow_written,
        non_auxpow_skipped = summary.non_auxpow_skipped,
        malformed_skipped = summary.malformed_skipped,
        "completed bounded Elastos backfill"
    );

    run_post_backfill_repair(
        &mut client,
        context.parent_classifier(),
        Some(mmm_capture::source_registry::ELASTOS_SOURCE_CODE),
        config.start_height,
        config.end_height,
        "Elastos backfill",
    )
    .await?;

    Ok(())
}

fn elastos_backfill_effect(
    height: i32,
    outcome: ElastosHeightOutcome,
) -> Result<BackfillHeightEffect> {
    match outcome {
        ElastosHeightOutcome::AuxpowWritten => Ok(BackfillHeightEffect::AuxpowWritten),
        ElastosHeightOutcome::NonAuxpowSkipped
        | ElastosHeightOutcome::NearSkipped
        | ElastosHeightOutcome::ChildTargetSkipped
        | ElastosHeightOutcome::NonBtcParentSkipped
        | ElastosHeightOutcome::ClassifierConflictSkipped => {
            Ok(BackfillHeightEffect::NonAuxpowSkipped)
        }
        ElastosHeightOutcome::MalformedSkipped => Ok(BackfillHeightEffect::MalformedSkipped),
        // A bounded backfill must not silently leave gaps. A beyond-horizon parent
        // holds (and the backfill fails) only when Bitcoin Core cannot answer: a
        // Core-enabled backfill resolves it from Core, a Core-disabled (offline)
        // backfill fails here as before.
        ElastosHeightOutcome::TableHorizonHold => bail!(
            "Elastos backfill hit the nBits-table horizon at height {height} with no Core answer; enable BITCOIN_RPC_URL or regenerate the table (scripts/gen-nbits-table.py) before backfilling further"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elastos_accepts_a_bounded_range() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let config = BackfillConfig::from_args_with_lookup(
            crate::chains::spec::by_id(crate::chains::spec::ChainId::Elastos),
            ["360000", "360100"],
            |key| empty.get(key).cloned(),
        )
        .unwrap();
        assert_eq!((config.start_height, config.end_height), (360_000, 360_100));
    }

    #[test]
    fn elastos_rejects_oversize_range_without_override() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        // A range over the default cap is rejected unless ELASTOS_ALLOW_LARGE_BACKFILL=1
        // (process-global env, so the override path is left to manual operation).
        let err = BackfillConfig::from_args_with_lookup(
            crate::chains::spec::by_id(crate::chains::spec::ChainId::Elastos),
            ["177000", "300000"],
            |key| empty.get(key).cloned(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("ELASTOS_MAX_BACKFILL_RANGE"));
    }

    #[test]
    fn elastos_rejects_end_before_start() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let err = BackfillConfig::from_args_with_lookup(
            crate::chains::spec::by_id(crate::chains::spec::ChainId::Elastos),
            ["360100", "360000"],
            |key| empty.get(key).cloned(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("greater than or equal"));
    }
}
