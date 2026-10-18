//! Shared live-producer `tick -> wait -> sleep/shutdown` loop skeleton.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::time::sleep;
use tracing::{debug, error, info};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickOutcome {
    pub(crate) progressed: bool,
    pub(crate) idle_at_target: bool,
}

/// Tick errors are passed to [`LiveProducer::wait_after_tick`] so the adapter
/// decides whether to retry or fail-stop.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PollLoopDecision {
    Continue,
    Shutdown,
}

pub(crate) async fn wait_for_next_tick_or_shutdown(
    poll_interval: Duration,
    shutdown: impl Future<Output = Result<()>>,
) -> Result<PollLoopDecision> {
    tokio::select! {
        // Biased shutdown-first so SIGINT/SIGTERM wins even when poll_interval is zero.
        biased;
        result = shutdown => {
            result?;
            Ok(PollLoopDecision::Shutdown)
        }
        () = sleep(poll_interval) => Ok(PollLoopDecision::Continue),
    }
}

#[cfg(unix)]
pub(crate) struct ShutdownSignals {
    sigint: tokio::signal::unix::Signal,
    sigterm: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    pub(crate) fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            sigint: signal(SignalKind::interrupt()).context("install SIGINT handler")?,
            sigterm: signal(SignalKind::terminate()).context("install SIGTERM handler")?,
        })
    }

    pub(crate) async fn recv(&mut self) -> Result<()> {
        tokio::select! {
            _ = self.sigint.recv() => Ok(()),
            _ = self.sigterm.recv() => Ok(()),
        }
    }
}

#[cfg(not(unix))]
pub(crate) struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self)
    }

    pub(crate) async fn recv(&mut self) -> Result<()> {
        tokio::signal::ctrl_c().await.context("listen for SIGINT")
    }
}
