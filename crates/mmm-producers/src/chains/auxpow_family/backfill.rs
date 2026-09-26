//! The bounded backfill for bitcoind-family chains: tip validation, the
//! spec-driven below-floor warning, the per-height capture loop over the
//! shared runner, and the post-backfill repair under the spec's scope.

use super::*;
use crate::chains::backfill::{
    BackfillConfig, BackfillHeightEffect, BackfillSummary, backfill_progress,
    run_delayed_backfill_range,
};

/// Registry-dispatched backfill entry point for bitcoind-family chains.
pub(crate) async fn backfill(rt: ProducerRuntime, config: BackfillConfig) -> Result<()> {
    let spec = config.spec;
    let rpc_config = crate::chains::config::bitcoind_rpc_config(spec)?;
    let rpc = BitcoindRpcClient::new(family_of(spec).label, rpc_config)?;
    run_auxpow_backfill(rt, rpc, config).await
}

/// Run a bounded backfill for a bitcoind-family chain: tip validation, the
/// spec-driven below-floor warning, classifier warning, the per-height capture
/// loop, and post-backfill repair under the spec's scope.
pub(crate) async fn run_auxpow_backfill(
    rt: ProducerRuntime,
    rpc: BitcoindRpcClient,
    config: BackfillConfig,
) -> Result<()> {
    let ProducerRuntime {
        pg_client: mut client,
        parent_classifier,
    } = rt;
    let spec = config.spec;
    let family = family_of(spec);
    ensure_mainnet_endpoint(&rpc, family).await?;

    let chain_tip = rpc
        .get_block_count()
        .await
        .with_context(|| format!("get {} tip before backfill", spec.display_name))?;
    config.validate_against_tip(chain_tip)?;

    if let Some(message) = family.floor_warning
        && config.start_height < spec.activation_floor
    {
        warn!(
            start_height = config.start_height,
            first_auxpow_height = spec.activation_floor,
            "{message}"
        );
    }

    let context =
        AuxpowCaptureContext::new_with_classifier(&client, spec, parent_classifier).await?;
    info!(
        chain = spec.slug,
        start_height = config.start_height,
        end_height = config.end_height,
        chain_tip,
        "starting bounded AuxPoW backfill"
    );

    let progress = backfill_progress(
        "chain-backfill",
        &config,
        crate::chains::rpc_metrics_for_reporting(rpc.metrics(), context.parent_classifier()),
    );
    let summary = run_delayed_backfill_range(&config, 0, &progress, async |height| {
        let outcome = process_auxpow_height(&mut client, &rpc, &context, height).await?;
        Ok(auxpow_backfill_effect(outcome))
    })
    .await?;

    if summary.malformed_held > 0 {
        warn!(
            chain = spec.slug,
            processed = summary.processed,
            auxpow_written = summary.auxpow_written,
            non_auxpow_skipped = summary.non_auxpow_skipped,
            malformed_skipped = summary.malformed_skipped,
            malformed_held = summary.malformed_held,
            "bounded AuxPoW backfill left unresolved capture errors"
        );
    } else {
        info!(
            chain = spec.slug,
            processed = summary.processed,
            auxpow_written = summary.auxpow_written,
            non_auxpow_skipped = summary.non_auxpow_skipped,
            malformed_skipped = summary.malformed_skipped,
            "completed bounded AuxPoW backfill"
        );
    }

    let repair_scope = match family.repair_scope {
        RepairScope::Global => None,
        RepairScope::SourceScoped => Some(spec.source_code),
    };
    run_post_backfill_repair(
        &mut client,
        context.parent_classifier(),
        repair_scope,
        config.start_height,
        config.end_height,
        &format!("{} backfill", spec.display_name),
    )
    .await?;

    // Everything captured in the range is written and reconciled; the run
    // itself is still not a success. Reporting completion over a hole is the
    // failure this policy exists to prevent.
    ensure_backfill_complete(spec, &config, &summary)?;
    progress.finish();
    Ok(())
}

/// The bounded-backfill completion verdict. A range that left any held
/// malformed proof is incomplete and must exit non-zero, whatever else it
/// captured; the operator re-runs the range once the capture errors clear.
fn ensure_backfill_complete(
    spec: &ChainSpec,
    config: &BackfillConfig,
    summary: &BackfillSummary,
) -> Result<()> {
    ensure!(
        summary.malformed_held == 0,
        "{} backfill {}..{} is incomplete: {} height(s) hold an unresolved capture error; \
         resolve them and re-run the range",
        spec.display_name,
        config.start_height,
        config.end_height,
        summary.malformed_held,
    );
    Ok(())
}

fn auxpow_backfill_effect(outcome: HeightOutcome) -> BackfillHeightEffect {
    match outcome {
        HeightOutcome::AuxpowWritten => BackfillHeightEffect::AuxpowWritten,
        HeightOutcome::NonAuxpowSkipped => BackfillHeightEffect::NonAuxpowSkipped,
        HeightOutcome::MalformedSkipped => BackfillHeightEffect::MalformedSkipped,
        HeightOutcome::MalformedHeld => BackfillHeightEffect::MalformedHeld,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::spec::{ChainId, by_id};

    /// A bounded backfill over a range containing a held malformed proof must
    /// fail the run, naming the range, rather than logging completion.
    #[test]
    fn backfill_over_a_held_height_is_reported_incomplete() {
        let spec = by_id(ChainId::Qbit);
        let config = BackfillConfig::from_args(spec, ["78050", "78060"]).expect("parse range");

        let clean = BackfillSummary {
            processed: 2,
            auxpow_written: 1,
            non_auxpow_skipped: 1,
            ..BackfillSummary::default()
        };
        ensure_backfill_complete(spec, &config, &clean)
            .expect("a clean range completes successfully");

        let held = BackfillSummary {
            processed: 3,
            malformed_held: 1,
            ..clean
        };
        let err = ensure_backfill_complete(spec, &config, &held)
            .expect_err("a held height must fail the run");
        assert_eq!(
            err.to_string(),
            "Qbit backfill 78050..78060 is incomplete: 1 height(s) hold an unresolved \
             capture error; resolve them and re-run the range"
        );
    }

    /// The malformed-outcome to backfill-counter mapping keeps the two policies
    /// in separate columns, so a held height can never be tallied as a skip.
    #[test]
    fn backfill_effects_keep_held_and_skipped_separate() {
        assert_eq!(
            auxpow_backfill_effect(HeightOutcome::MalformedHeld),
            BackfillHeightEffect::MalformedHeld
        );
        assert_eq!(
            auxpow_backfill_effect(HeightOutcome::MalformedSkipped),
            BackfillHeightEffect::MalformedSkipped
        );
    }
}
