//! Shared live-producer loop skeleton.
//!
//! Adapters own domain work, cursor state, and fail-stop policy. This module
//! only owns the common `tick -> classify wait -> sleep/shutdown` loop.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;
use tracing::{debug, error, info};

/// Generic per-tick outcome used by adapter-specific wait policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickOutcome {
    /// Whether the producer made durable forward progress.
    pub(crate) progressed: bool,
    /// Whether the producer is idle because its target has been reached.
    pub(crate) idle_at_target: bool,
}

/// Minimal adapter trait for a long-running producer.
///
/// The loop deliberately does not `?` the result of [`LiveProducer::tick`].
/// Tick errors are handed to [`LiveProducer::wait_after_tick`], and the adapter
/// decides whether the service retries or fail-stops.
#[allow(async_fn_in_trait)]
pub(crate) trait LiveProducer {
    fn name(&self) -> &'static str;

    async fn tick(&mut self) -> Result<TickOutcome>;

    fn wait_after_tick(&mut self, result: Result<TickOutcome>) -> Result<Duration>;

    fn log_starting(&self) {
        info!(producer = self.name(), "starting live producer");
    }

    fn log_tick_result(&self, result: &Result<TickOutcome>) {
        match result {
            Ok(outcome) => debug!(
                producer = self.name(),
                progressed = outcome.progressed,
                idle_at_target = outcome.idle_at_target,
                "live producer tick complete"
            ),
            Err(err) => error!(
                producer = self.name(),
                error = %err,
                "live producer tick failed"
            ),
        }
    }

    fn log_shutdown(&self) {
        info!(
            producer = self.name(),
            "shutdown signal received; stopping live producer"
        );
    }
}

pub(crate) async fn run_live_loop<P: LiveProducer>(mut producer: P) -> Result<()> {
    let mut shutdown = ShutdownSignals::new()?;
    producer.log_starting();

    loop {
        let tick_result = producer.tick().await;
        producer.log_tick_result(&tick_result);
        let wait = producer.wait_after_tick(tick_result)?;

        match wait_for_next_tick_or_shutdown(wait, shutdown.recv()).await? {
            PollLoopDecision::Continue => {}
            PollLoopDecision::Shutdown => {
                producer.log_shutdown();
                return Ok(());
            }
        }
    }
}

/// Outcome of the inter-tick wait: keep looping or stop on a received signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollLoopDecision {
    /// The interval elapsed with no shutdown; run the next tick.
    Continue,
    /// A SIGINT/SIGTERM arrived; the loop must return.
    Shutdown,
}

/// Race the inter-tick sleep against a shutdown future, returning whichever
/// fires first. `Shutdown` wins ties (see the `biased` note) so a pending
/// signal is never deferred past the process manager's grace period.
pub(crate) async fn wait_for_next_tick_or_shutdown(
    poll_interval: Duration,
    shutdown: impl Future<Output = Result<()>>,
) -> Result<PollLoopDecision> {
    tokio::select! {
        // Biased + shutdown-first so a pending SIGINT/SIGTERM always wins, even
        // when poll_interval is zero (the backbone live loop uses a zero wait
        // while catching up); otherwise an immediately-ready sleep could lose the
        // race and defer the stop past the manager's grace. For the AuxPoW
        // pollers' nonzero intervals the two are effectively never ready
        // together, so this only makes shutdown deterministic.
        biased;
        result = shutdown => {
            result?;
            Ok(PollLoopDecision::Shutdown)
        }
        () = sleep(poll_interval) => Ok(PollLoopDecision::Continue),
    }
}

/// Unix graceful-shutdown source: both SIGINT and SIGTERM, since the live-test
/// deployment manager stops producers with SIGTERM and an interactive run with
/// SIGINT.
#[cfg(unix)]
pub(crate) struct ShutdownSignals {
    sigint: tokio::signal::unix::Signal,
    sigterm: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    /// Install both handlers up front, before the loop starts, so a signal that
    /// arrives during the very first tick is not missed.
    pub(crate) fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            sigint: signal(SignalKind::interrupt()).context("install SIGINT handler")?,
            sigterm: signal(SignalKind::terminate()).context("install SIGTERM handler")?,
        })
    }

    /// Resolve when either SIGINT or SIGTERM is received.
    pub(crate) async fn recv(&mut self) -> Result<()> {
        tokio::select! {
            _ = self.sigint.recv() => Ok(()),
            _ = self.sigterm.recv() => Ok(()),
        }
    }
}

/// Non-unix fallback: only Ctrl-C (SIGINT-equivalent) is available.
#[cfg(not(unix))]
pub(crate) struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    /// No handler state to install on non-unix; constructed infallibly.
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }

    /// Resolve on Ctrl-C.
    pub(crate) async fn recv(&mut self) -> Result<()> {
        tokio::signal::ctrl_c().await.context("listen for SIGINT")
    }
}
