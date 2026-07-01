//! Bounded RSK historical backfill: order-preserving concurrent prefetch of
//! canonical + uncle bundles, written serially in ascending height order.
//! Intentionally divergent from the shared family runner (i64 tip, bundle
//! pipeline, RSK-specific summary); arg parsing is the shared
//! `BackfillConfig`.

use std::time::Instant;

use anyhow::{Context, Result};
use futures::StreamExt;
use tracing::{info, warn};

use crate::chains::backfill::BackfillConfig;
use crate::chains::rsk::capture::{HeightOutcome, RskCaptureContext, write_rsk_bundle};
use crate::chains::rsk::rpc::RskRpcClient;
use crate::chains::rsk::traverse::fetch_rsk_height_bundle;
use crate::producer_runtime::{
    ProducerRuntime, run_post_backfill_repair, warn_backfill_classifier_enabled,
};
use mmm_capture::capture::now_epoch_seconds;

/// Default bounded prefetch concurrency for the RSK historical backfill. Each
/// in-flight slot is one `eth_getBlockByNumber` round-trip (plus its uncle
/// fetches); conservative by default to respect RSKj's JSON-RPC thread pool.
/// Override with `RSK_BACKFILL_FETCH_CONCURRENCY` (clamped to `>= 1`).
pub(crate) const RSK_DEFAULT_BACKFILL_FETCH_CONCURRENCY: usize = 16;

/// Grand totals a backfill run folds [`HeightOutcome`](crate::chains::rsk::capture::HeightOutcome)s
/// into, via `accumulate_rsk_summary`.
/// Canonical counts partition by outcome (written / pre-RSKIP-92 / malformed /
/// missing); uncle counts sum across all heights. Logged at run end.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RskBackfillSummary {
    pub(crate) heights_processed: usize,
    pub(crate) canonical_written: usize,
    pub(crate) canonical_pre_rskip92: usize,
    pub(crate) canonical_malformed: usize,
    pub(crate) canonical_missing: usize,
    pub(crate) uncles_seen: usize,
    pub(crate) uncles_written: usize,
    pub(crate) uncles_pre_rskip92: usize,
    pub(crate) uncles_malformed: usize,
}

/// Registry-dispatched live-poll entry point for RSK.
pub(crate) async fn poll(
    spec: &'static crate::chains::spec::ChainSpec,
    rt: ProducerRuntime,
) -> Result<()> {
    let rpc_config = crate::chains::config::rsk_rpc_config()?;
    let rpc = RskRpcClient::new(rpc_config)?;
    let poller_config = crate::chains::config::poller_config(spec)?;
    let context =
        RskCaptureContext::new_with_classifier(&rt.pg_client, rt.parent_classifier).await?;
    let poller = crate::poller::Poller::new(
        crate::chains::rsk::capture::RskChainPoller::new(rt.pg_client, rpc, context),
        poller_config,
    )
    .await?;
    poller.run_forever().await
}

/// Registry-dispatched backfill entry point for RSK.
pub(crate) async fn backfill(rt: ProducerRuntime, config: BackfillConfig) -> Result<()> {
    let rpc_config = crate::chains::config::rsk_rpc_config()?;
    let rpc = RskRpcClient::new(rpc_config)?;
    run_rsk_backfill(rt, rpc, config).await
}

/// Run the bounded RSK backfill over `[start_height, end_height]`. Bails if the
/// requested end exceeds the observed tip; warns (but does not stop) when the
/// start precedes RSKIP-92. The network-bound bundle prefetch runs
/// `fetch_concurrency`-wide while a single `&mut Client` writer consumes bundles
/// in strict ascending height order; the lowest-height fetch error surfaces
/// first via `?` because `buffered` yields in input order. The backfill never
/// moves the live `poll_cursor`. Runs post-backfill repair before returning the
/// summary.
pub(crate) async fn run_rsk_backfill(
    rt: ProducerRuntime,
    rpc: RskRpcClient,
    config: BackfillConfig,
) -> Result<()> {
    let ProducerRuntime {
        pg_client: mut client,
        parent_classifier,
    } = rt;
    let chain_tip = rpc
        .get_block_number()
        .await
        .context("get RSK tip before backfill")?;
    if (config.end_height as i64) > chain_tip {
        anyhow::bail!(
            "requested end height {} exceeds observed RSK chain tip {chain_tip}",
            config.end_height
        );
    }

    if config.start_height < config.spec.activation_floor {
        warn!(
            start_height = config.start_height,
            first_auxpow_height = config.spec.activation_floor,
            "start-height precedes RSKIP-92; pre-RSKIP-92 RSK blocks have no 80-byte BTC parent header and will be skipped"
        );
    }

    let context = RskCaptureContext::new_with_classifier(&client, parent_classifier).await?;
    warn_backfill_classifier_enabled("RSK", context.parent_classifier());
    let fetch_concurrency = crate::chains::config::rsk_backfill_fetch_concurrency()?;
    info!(
        start_height = config.start_height,
        end_height = config.end_height,
        chain_tip,
        fetch_concurrency,
        "starting bounded RSK AuxPoW backfill"
    );

    // Bounded order-preserving prefetch: the network-bound canonical+uncle fetch
    // runs `fetch_concurrency`-wide while the single `&mut Client` writer
    // consumes bundles in strict ascending height order via `.buffered`. A fetch
    // error fails the chunk through `?` (the lowest-height error surfaces first
    // because `buffered` yields in input order), matching the serial path's
    // persisted end-state; the `.tmp/live-test-deployment` chunk driver retries a
    // failed chunk.
    let started = Instant::now();
    let mut summary = RskBackfillSummary::default();
    let mut fetches = futures::stream::iter(config.start_height..=config.end_height)
        .map(|height| fetch_rsk_height_bundle(rpc.clone(), height as i64))
        .buffered(fetch_concurrency);

    while let Some(bundle) = fetches.next().await {
        let outcome = write_rsk_bundle(&mut client, &context, bundle?, &now_epoch_seconds).await?;
        accumulate_rsk_summary(&mut summary, outcome);
    }

    let elapsed = started.elapsed();
    let blocks_per_sec = if elapsed.as_secs_f64() > 0.0 {
        summary.heights_processed as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };
    info!(
        heights_processed = summary.heights_processed,
        canonical_written = summary.canonical_written,
        canonical_pre_rskip92 = summary.canonical_pre_rskip92,
        canonical_malformed = summary.canonical_malformed,
        canonical_missing = summary.canonical_missing,
        uncles_seen = summary.uncles_seen,
        uncles_written = summary.uncles_written,
        uncles_pre_rskip92 = summary.uncles_pre_rskip92,
        uncles_malformed = summary.uncles_malformed,
        elapsed_secs = elapsed.as_secs_f64(),
        blocks_per_sec,
        "completed bounded RSK AuxPoW backfill"
    );

    run_post_backfill_repair(
        &mut client,
        context.parent_classifier(),
        Some(config.spec.source_code),
        config.start_height,
        config.end_height,
        "RSK backfill",
    )
    .await?;

    Ok(())
}

fn accumulate_rsk_summary(summary: &mut RskBackfillSummary, outcome: HeightOutcome) {
    summary.heights_processed += 1;
    if !outcome.canonical_present {
        summary.canonical_missing += 1;
        return;
    }
    match outcome.canonical {
        Some(crate::chains::rsk::capture::BlockOutcome::Written) => summary.canonical_written += 1,
        Some(crate::chains::rsk::capture::BlockOutcome::PreRskip92Skipped) => {
            summary.canonical_pre_rskip92 += 1;
        }
        Some(crate::chains::rsk::capture::BlockOutcome::MalformedSkipped) => {
            summary.canonical_malformed += 1;
        }
        None => summary.canonical_missing += 1,
    }
    summary.uncles_seen += outcome.uncles_seen;
    summary.uncles_written += outcome.uncles_written;
    summary.uncles_pre_rskip92 += outcome.uncles_pre_rskip92;
    summary.uncles_malformed += outcome.uncles_malformed;
}
