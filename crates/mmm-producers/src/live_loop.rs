//! Shared live-producer `tick -> wait -> sleep/shutdown` loop skeleton.

use std::future::Future;
use std::pin::pin;
use std::time::Duration;

use anyhow::{Context, Result};
use mmm_read_model::pending_cascades_settled;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

/// Longest shutdown waits for a running dependent cascade, inside the deployed
/// 90 s `TimeoutStopSec`.
const CASCADE_SETTLE_TIMEOUT: Duration = Duration::from_secs(60);

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
        // A tick can outlast the service stop timeout (sequential remote calls
        // at ~340 ms, RPC retries, lock waits), so shutdown drops it at its
        // current await point. Uncommitted work rolls back and resumes from the
        // persisted cursors and reconcile queues on the next start. Only a
        // committed write's in-memory dependent cascade gets time to finish.
        let Some(tick_result) = until_shutdown(producer.tick(), shutdown.recv()).await? else {
            break;
        };
        producer.log_tick_result(&tick_result);
        let wait = producer.wait_after_tick(tick_result)?;
        let Some(()) = until_shutdown(sleep(wait), shutdown.recv()).await? else {
            break;
        };
    }
    producer.log_shutdown();
    Ok(())
}

/// Drive `work` to completion unless `shutdown` resolves first. After shutdown,
/// keep driving `work` only while a dependent cascade is still running (at most
/// [`CASCADE_SETTLE_TIMEOUT`]), then drop it and return `None`.
async fn until_shutdown<T>(
    work: impl Future<Output = T>,
    shutdown: impl Future<Output = Result<()>>,
) -> Result<Option<T>> {
    let mut work = pin!(work);
    tokio::select! {
        // Biased shutdown-first so SIGINT/SIGTERM wins even when `work` is ready at once.
        biased;
        result = shutdown => result?,
        output = &mut work => return Ok(Some(output)),
    }
    tokio::select! {
        biased;
        () = pending_cascades_settled() => {}
        _ = work => {}
        () = sleep(CASCADE_SETTLE_TIMEOUT) => warn!(
            timeout_secs = CASCADE_SETTLE_TIMEOUT.as_secs(),
            "shutdown abandoned a dependent cascade still running; its remaining dependents may be stale"
        ),
    }
    Ok(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn until_shutdown_abandons_unfinished_work() -> Result<()> {
        let output =
            until_shutdown(std::future::pending::<()>(), std::future::ready(Ok(()))).await?;
        assert_eq!(output, None);
        Ok(())
    }
}
