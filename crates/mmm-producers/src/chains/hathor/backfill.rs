//! Bounded Hathor historical backfill. Intentionally divergent from the shared
//! family runner: public-REST range cap, per-height delay, and the
//! absent/transient hold policy (`HATHOR_BACKFILL_SKIP_HOLDS`; a
//! nBits-table-horizon hold always stops the backfill). Env reads route
//! through `chains::config`; behavior unchanged.

use anyhow::{Context, Result, bail};
use tracing::{info, warn};

use crate::chains::backfill::{BackfillConfig, BackfillHeightEffect, run_delayed_backfill_range};
use crate::chains::hathor::capture::{
    HathorCaptureContext, HathorHeightOutcome, process_hathor_height,
};
use crate::chains::hathor::rpc::HathorRpcClient;
use crate::chains::spec::HATHOR_DEFAULT_BACKFILL_START;
use crate::producer_runtime::{ProducerRuntime, run_post_backfill_repair};

/// Registry-dispatched live-poll entry point for Hathor.
pub(crate) async fn poll(
    spec: &'static crate::chains::spec::ChainSpec,
    rt: ProducerRuntime,
) -> Result<()> {
    let rpc_config = crate::chains::config::hathor_rpc_config()?;
    let rpc = HathorRpcClient::new(rpc_config)?;
    let poller_config = crate::chains::config::poller_config(spec)?;
    let context =
        HathorCaptureContext::new_with_classifier(&rt.pg_client, rt.parent_classifier).await?;
    let poller = crate::poller::Poller::new(
        crate::chains::hathor::capture::HathorChainPoller::new(rt.pg_client, rpc, context),
        poller_config,
    )
    .await?;
    poller.run_forever().await
}

/// Registry-dispatched backfill entry point for Hathor.
pub(crate) async fn backfill(rt: ProducerRuntime, config: BackfillConfig) -> Result<()> {
    let rpc_config = crate::chains::config::hathor_rpc_config()?;
    let rpc = HathorRpcClient::new(rpc_config)?;
    run_hathor_backfill(rt, rpc, config).await
}

/// Drive the inclusive `[start, end]` range through the shared capture path over
/// the public REST API, then run the post-backfill read-model repair. A bounded
/// backfill must not leave silent gaps: the nBits-table horizon always aborts
/// (regenerate the table first), and absent/transient holds abort by default
/// unless `HATHOR_BACKFILL_SKIP_HOLDS` downgrades them to logged skips. Honors
/// the per-height delay; does NOT move the live poll cursor.
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

    let context = HathorCaptureContext::new_with_classifier(&client, parent_classifier).await?;
    info!(
        start_height = config.start_height,
        end_height = config.end_height,
        chain_tip,
        "starting bounded Hathor backfill via public REST"
    );

    let summary = run_delayed_backfill_range(&config, delay_ms, async |height| {
        let outcome = process_hathor_height(&mut client, &rpc, &context, height).await?;
        hathor_backfill_effect(height, outcome, skip_holds)
    })
    .await?;

    info!(
        processed = summary.processed,
        auxpow_written = summary.auxpow_written,
        non_auxpow_skipped = summary.non_auxpow_skipped,
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

    Ok(())
}

fn hathor_backfill_effect(
    height: i32,
    outcome: HathorHeightOutcome,
    skip_holds: bool,
) -> Result<BackfillHeightEffect> {
    match outcome {
        HathorHeightOutcome::AuxpowWritten => Ok(BackfillHeightEffect::AuxpowWritten),
        HathorHeightOutcome::NonAuxpowSkipped
        | HathorHeightOutcome::VoidedSkipped
        | HathorHeightOutcome::NonBtcParentSkipped
        | HathorHeightOutcome::ConflictSkipped => Ok(BackfillHeightEffect::NonAuxpowSkipped),
        HathorHeightOutcome::MalformedSkipped => Ok(BackfillHeightEffect::MalformedSkipped),
        // A bounded backfill must not silently leave gaps. A beyond-horizon parent
        // holds (and the backfill fails) only when Bitcoin Core cannot answer: a
        // Core-enabled backfill resolves it from Core. Transient/absent holds fail
        // by default, with an explicit skip override.
        HathorHeightOutcome::TableHorizonHold => bail!(
            "Hathor backfill hit the nBits-table horizon at height {height} with no Core answer; enable BITCOIN_RPC_URL or regenerate the table (scripts/gen-nbits-table.py) before backfilling further"
        ),
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
        // A huge range trips the public-endpoint guard under any sane default
        // (`HATHOR_MAX_BACKFILL_RANGE`); without it the public REST API would be
        // swept. The `HATHOR_ALLOW_LARGE_BACKFILL=1` override is process-global
        // env and so left to manual operation rather than a racy test.
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

    #[test]
    fn hathor_rejects_end_before_start() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let err = BackfillConfig::from_args_with_lookup(
            crate::chains::spec::by_id(crate::chains::spec::ChainId::Hathor),
            ["20", "10"],
            |key| empty.get(key).cloned(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("end-height must be greater than or equal to start-height")
        );
    }

    #[test]
    fn hathor_rejects_end_above_tip() {
        let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let config = BackfillConfig::from_args_with_lookup(
            crate::chains::spec::by_id(crate::chains::spec::ChainId::Hathor),
            ["10", "20"],
            |key| empty.get(key).cloned(),
        )
        .unwrap();
        let err = config.validate_against_tip(19).unwrap_err();
        assert!(
            err.to_string()
                .contains("requested end height 20 exceeds observed Hathor chain tip 19")
        );
    }
}
