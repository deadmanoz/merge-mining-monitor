//! Bounded Hathor historical backfill over public REST.

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::chains::backfill::{
    BackfillConfig, BackfillHeightEffect, backfill_progress, lineage_pending_error,
    run_delayed_backfill_range,
};
use crate::chains::hathor::capture::{
    ChainObservation, HathorCaptureContext, HathorHeightOutcome, process_hathor_height,
};
use crate::chains::hathor::rpc::HathorRpcClient;
use crate::chains::spec::HATHOR_DEFAULT_BACKFILL_START;
use crate::producer_runtime::{ProducerRuntime, run_post_backfill_repair};

pub(crate) async fn poll(
    spec: &'static crate::chains::spec::ChainSpec,
    rt: ProducerRuntime,
    live: &mut crate::live_loop::LiveSession,
) -> Result<()> {
    let rpc_config = crate::chains::config::hathor_rpc_config()?;
    let rpc = HathorRpcClient::new(rpc_config)?;
    let poller_config = crate::chains::config::poller_config(spec)?;
    let context = HathorCaptureContext::new_with_classifier(
        &rt.pg_client,
        rt.parent_classifier,
        ChainObservation::Live {
            fork_window: poller_config.reorg_depth,
        },
    )
    .await?;
    let poller = crate::poller::Poller::new(
        crate::chains::hathor::poller::HathorChainPoller::new(rt.pg_client, rpc, context),
        poller_config,
    )
    .await?;
    crate::live_loop::run_ticks(poller, live).await
}

pub(crate) async fn backfill(rt: ProducerRuntime, config: BackfillConfig) -> Result<()> {
    let rpc_config = crate::chains::config::hathor_rpc_config()?;
    let rpc = HathorRpcClient::new(rpc_config)?;
    run_hathor_backfill(rt, rpc, config).await
}

/// Absent/transient holds abort by default unless `HATHOR_BACKFILL_SKIP_HOLDS=1`;
/// never moves `poll_cursor`.
pub(crate) async fn run_hathor_backfill(
    rt: ProducerRuntime,
    rpc: HathorRpcClient,
    config: BackfillConfig,
) -> Result<()> {
    let ProducerRuntime {
        pg_client: mut client,
        parent_classifier,
    } = rt;
    let chain_tip = rpc
        .get_chain_tip()
        .await
        .context("get Hathor tip before backfill")?;
    config.validate_against_tip(chain_tip)?;

    if config.start_height < HATHOR_DEFAULT_BACKFILL_START {
        warn!(
            start_height = config.start_height,
            default_start = HATHOR_DEFAULT_BACKFILL_START,
            "start-height precedes the Hathor safe-default backfill start; earlier heights are mostly non-merge-mined and skipped"
        );
    }

    let delay_ms: u64 = crate::chains::config::hathor_backfill_delay_ms();
    let skip_holds = crate::chains::config::hathor_backfill_skip_holds();

    let fork_window = crate::chains::config::poller_config(config.spec)?.reorg_depth;
    let context = HathorCaptureContext::new_with_classifier(
        &client,
        parent_classifier,
        ChainObservation::Live { fork_window },
    )
    .await?;
    info!(
        start_height = config.start_height,
        end_height = config.end_height,
        chain_tip,
        "starting bounded Hathor backfill via public REST"
    );

    let progress = backfill_progress(
        "chain-backfill",
        &config,
        crate::chains::rpc_metrics_for_reporting(rpc.metrics(), context.parent_classifier()),
    );
    let summary = run_delayed_backfill_range(&config, delay_ms, &progress, async |height| {
        let outcome = process_hathor_height(&mut client, &rpc, &context, height).await?;
        hathor_backfill_effect(config.spec, height, outcome, skip_holds)
    })
    .await?;

    info!(
        processed = summary.processed,
        auxpow_written = summary.auxpow_written,
        non_auxpow_skipped = summary.non_auxpow_skipped,
        non_bitcoin_parent = summary.non_bitcoin_parent,
        malformed_skipped = summary.malformed_skipped,
        "completed bounded Hathor backfill"
    );
    run_post_backfill_repair(
        &mut client,
        context.parent_classifier(),
        Some(mmm_capture::source_registry::HATHOR_SOURCE_CODE),
        config.start_height,
        config.end_height,
        "Hathor backfill",
    )
    .await?;
    progress.finish();

    Ok(())
}

fn hathor_backfill_effect(
    spec: &crate::chains::spec::ChainSpec,
    height: i32,
    outcome: HathorHeightOutcome,
    skip_holds: bool,
) -> Result<BackfillHeightEffect> {
    match outcome {
        HathorHeightOutcome::AuxpowWritten => Ok(BackfillHeightEffect::AuxpowWritten),
        HathorHeightOutcome::NonAuxpowSkipped | HathorHeightOutcome::VoidedSkipped => {
            Ok(BackfillHeightEffect::NonAuxpowSkipped)
        }
        HathorHeightOutcome::NonBitcoinParent => Ok(BackfillHeightEffect::NonBitcoinParent),
        HathorHeightOutcome::MalformedSkipped => Ok(BackfillHeightEffect::MalformedSkipped),
        HathorHeightOutcome::LineagePending => Err(lineage_pending_error(spec, height)),
        HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => {
            if skip_holds {
                warn!(
                    height,
                    ?outcome,
                    "Hathor backfill hold skipped under HATHOR_BACKFILL_SKIP_HOLDS"
                );
                Ok(BackfillHeightEffect::NonAuxpowSkipped)
            } else {
                bail!(
                    "Hathor backfill hold at height {height} ({outcome:?}); the public REST API did not return a usable block (set HATHOR_BACKFILL_SKIP_HOLDS=1 to skip)"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hathor_accepts_a_bounded_range() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let config = BackfillConfig::from_args_with_lookup(
            crate::chains::spec::by_id(crate::chains::spec::ChainId::Hathor),
            ["100", "200"],
            |key| empty.get(key).cloned(),
        )
        .unwrap();
        assert_eq!((config.start_height, config.end_height), (100, 200));
    }

    #[test]
    fn hathor_rejects_an_unbounded_public_sweep() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let err = BackfillConfig::from_args_with_lookup(
            crate::chains::spec::by_id(crate::chains::spec::ChainId::Hathor),
            ["0", "1000000"],
            |key| empty.get(key).cloned(),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Hathor backfill range 1000001 exceeds HATHOR_MAX_BACKFILL_RANGE 5000; \
                 set HATHOR_ALLOW_LARGE_BACKFILL=1 to override (the public REST API must not be swept)"
        );
    }
}
