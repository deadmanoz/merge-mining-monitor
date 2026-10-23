//! Supervise the complete live producer lifecycle, from Core readiness to shutdown.

use std::collections::hash_map::RandomState;
use std::future::Future;
use std::hash::BuildHasher;
use std::pin::pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use mmm_bitcoin_core::{ConfiguredParentClassifier, CoreFault, CoreUnavailable};
use mmm_read_model::pending_cascades_settled;
use mmm_store::{ProducerRun, RuntimeTransition};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, sleep_until};
use tokio_postgres::Client;
use tracing::{debug, error, info, warn};

use crate::bitcoin_epoch_cache::{LocalRecoveryPending, refresh_bitcoin_core_header_cache};
use crate::producer_runtime::ProducerRuntime;

/// Longest shutdown waits for a running dependent cascade, inside the deployed
/// 90 s `TimeoutStopSec`.
const CASCADE_SETTLE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TickOutcome {
    pub(crate) progressed: bool,
    pub(crate) idle_at_target: bool,
}

/// The supervisor handles typed dependency waits and local recovery yields;
/// adapters choose normal timing and retain their genuine-stall policy.
#[allow(async_fn_in_trait)]
pub(crate) trait LiveProducer {
    fn normal_interval(&self) -> Duration;

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
}

/// One cancellation boundary covers readiness, chain initialization, capture and
/// waits. Every exit publishes status through the same path.
pub(crate) async fn run_live(
    source_code: &str,
    interval: Duration,
    work: impl AsyncFnOnce(ProducerRuntime, &mut LiveSession) -> Result<()>,
) -> Result<()> {
    let mut shutdown = ShutdownSignals::new()?;
    let mut rt = ProducerRuntime::from_env_live().await?;
    let mut live = LiveSession::start(source_code).await?;
    let result = until_shutdown(
        async {
            live.prepare(&mut rt.pg_client, &rt.parent_classifier, interval)
                .await?;
            work(rt, &mut live).await
        },
        shutdown.recv(),
    )
    .await
    .and_then(|result| result.unwrap_or(Ok(())));
    live.finish(result.as_ref().err()).await;
    info!(source = source_code, "live producer stopped");
    result
}

pub(crate) async fn run_ticks<P: LiveProducer>(
    mut producer: P,
    live: &mut LiveSession,
) -> Result<()> {
    producer.log_starting();
    loop {
        live.attempt().await;
        let tick_result = producer.tick().await;
        producer.log_tick_result(&tick_result);
        let (state, delay) = match tick_result {
            Err(err) => match recovery_state(&err) {
                Some(state) => (state, producer.normal_interval()),
                None => (
                    CycleState::Pending("capture_pending"),
                    producer.wait_after_tick(Err(err))?,
                ),
            },
            Ok(outcome) => {
                let state = if outcome.progressed || outcome.idle_at_target {
                    CycleState::Healthy
                } else {
                    CycleState::Pending("producer_no_progress")
                };
                (state, producer.wait_after_tick(Ok(outcome))?)
            }
        };
        live.wait(state, "running", delay).await;
    }
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
struct ShutdownSignals {
    sigint: tokio::signal::unix::Signal,
    sigterm: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};

        Ok(Self {
            sigint: signal(SignalKind::interrupt()).context("install SIGINT handler")?,
            sigterm: signal(SignalKind::terminate()).context("install SIGTERM handler")?,
        })
    }

    async fn recv(&mut self) -> Result<()> {
        tokio::select! {
            _ = self.sigint.recv() => Ok(()),
            _ = self.sigterm.recv() => Ok(()),
        }
    }
}

#[cfg(not(unix))]
struct ShutdownSignals;

#[cfg(not(unix))]
impl ShutdownSignals {
    fn new() -> Result<Self> {
        Ok(Self)
    }

    async fn recv(&mut self) -> Result<()> {
        tokio::signal::ctrl_c().await.context("listen for SIGINT")
    }
}

enum CycleState {
    Healthy,
    Pending(&'static str),
    Waiting(CoreUnavailable),
}

struct WaitDecision {
    deadline: Instant,
    transition: RuntimeTransition,
}

#[derive(Default)]
struct RetryPolicy {
    failures: i64,
}

impl RetryPolicy {
    fn decide(&mut self, state: CycleState, phase: &'static str, normal: Duration) -> WaitDecision {
        let (state, dependency, reason, delay) = match state {
            CycleState::Healthy => {
                self.failures = 0;
                ("healthy", None, None, normal)
            }
            CycleState::Pending(reason) => ("recovering", None, Some(reason), normal),
            CycleState::Waiting(reason) => {
                self.failures = self.failures.saturating_add(1);
                let ceiling_ms = (5_000_u64 << (self.failures - 1).min(6)).min(300_000);
                let half = ceiling_ms / 2;
                let jitter = RandomState::new().hash_one(self.failures) % (half + 1);
                (
                    "waiting",
                    Some("bitcoin-core"),
                    Some(reason.code()),
                    Duration::from_millis(half + jitter),
                )
            }
        };
        let deadline = Instant::now() + delay;
        let next_retry_at = std::time::SystemTime::now()
            .checked_add(delay)
            .and_then(|wall| wall.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|epoch| i64::try_from(epoch.as_secs()).ok())
            .filter(|_| state != "healthy");
        WaitDecision {
            deadline,
            transition: RuntimeTransition {
                phase,
                state,
                dependency,
                reason_code: reason,
                failures: self.failures,
                next_retry_at,
            },
        }
    }
}

fn recovery_state(err: &anyhow::Error) -> Option<CycleState> {
    if let Some(reason) = err.downcast_ref::<CoreUnavailable>() {
        Some(CycleState::Waiting(*reason))
    } else {
        err.downcast_ref::<LocalRecoveryPending>()
            .map(|reason| CycleState::Pending(reason.0))
    }
}

/// Errors which best-effort replay/repair must not swallow.
pub(crate) fn must_propagate(err: &anyhow::Error) -> bool {
    recovery_state(err).is_some()
        || err.downcast_ref::<CoreFault>().is_some()
        || mmm_store::is_bitcoin_core_header_cache_integrity_error(err)
}

pub(crate) struct LiveSession {
    status: Arc<Client>,
    run: ProducerRun,
    renewable: Arc<AtomicBool>,
    heartbeat: JoinHandle<()>,
    retry: RetryPolicy,
}

impl Drop for LiveSession {
    fn drop(&mut self) {
        self.heartbeat.abort();
    }
}

impl LiveSession {
    async fn start(source_code: &str) -> Result<Self> {
        let status = Arc::new(mmm_pg::connect(&mmm_pg::PgConfig::from_env()?).await?);
        status
            .batch_execute("SET lock_timeout = '1s'; SET statement_timeout = '3s'")
            .await?;
        let source_id = mmm_store::get_source_id(status.as_ref(), source_code).await?;
        let run = mmm_store::start_producer_run(&status, source_id).await?;
        let renewable = Arc::new(AtomicBool::new(true));
        let heartbeat = spawn_heartbeat(status.clone(), run, renewable.clone());
        Ok(Self {
            status,
            run,
            renewable,
            heartbeat,
            retry: RetryPolicy::default(),
        })
    }

    async fn prepare(
        &mut self,
        client: &mut Client,
        classifier: &ConfiguredParentClassifier,
        interval: Duration,
    ) -> Result<()> {
        loop {
            self.attempt().await;
            match refresh_bitcoin_core_header_cache(client, classifier).await {
                // Readiness permits initialization; a capture cycle must succeed
                // before the supervisor clears the outage and resets backoff.
                Ok(_) => {
                    self.wait(
                        CycleState::Pending("startup_pending"),
                        "startup",
                        Duration::ZERO,
                    )
                    .await;
                    return Ok(());
                }
                Err(err) => match recovery_state(&err) {
                    Some(state) => self.wait(state, "startup", interval).await,
                    None => return Err(err),
                },
            }
        }
    }

    async fn attempt(&self) {
        if mmm_store::attempt_producer_run(&self.status, self.run)
            .await
            .is_err()
        {
            self.renewable.store(false, Ordering::Relaxed);
            warn!("live producer attempt status unavailable");
        }
    }

    fn record(&self, result: Result<()>) {
        self.renewable.store(result.is_ok(), Ordering::Relaxed);
        if result.is_err() {
            warn!(
                "live producer status unavailable; heartbeat will expire unless status writes recover"
            );
        }
    }

    async fn publish(&self, transition: &RuntimeTransition) {
        self.record(mmm_store::transition_producer_run(&self.status, self.run, transition).await);
    }

    async fn wait(&mut self, state: CycleState, phase: &'static str, normal: Duration) {
        let decision = self.retry.decide(state, phase, normal);
        self.publish(&decision.transition).await;
        sleep_until(decision.deadline).await;
    }

    async fn finish(&self, error: Option<&anyhow::Error>) {
        self.heartbeat.abort();
        let reason = error.map(|err| {
            err.downcast_ref::<CoreFault>()
                .map_or("producer_failed", |fault| fault.code())
        });
        self.publish(&RuntimeTransition {
            phase: "stopped",
            state: if error.is_some() { "failed" } else { "stopped" },
            dependency: None,
            reason_code: reason,
            failures: self.retry.failures,
            next_retry_at: None,
        })
        .await;
    }
}

fn spawn_heartbeat(
    status: Arc<Client>,
    run: ProducerRun,
    renewable: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            if renewable.load(Ordering::Relaxed)
                && mmm_store::heartbeat_producer_run(&status, run)
                    .await
                    .is_err()
            {
                renewable.store(false, Ordering::Relaxed);
                warn!("live producer heartbeat unavailable");
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outages_back_off_past_five_cycles_and_only_healthy_completion_resets() {
        let mut policy = RetryPolicy::default();
        for failure in 1..=12 {
            let before = Instant::now();
            let decision = policy.decide(
                CycleState::Waiting(CoreUnavailable::Warmup),
                "running",
                Duration::ZERO,
            );
            let delay = decision.deadline - before;
            let ceiling = (5_u64 << (failure - 1).min(6)).min(300);
            assert!(delay >= Duration::from_millis(ceiling * 500));
            assert!(delay <= Duration::from_secs(ceiling) + Duration::from_millis(10));
            assert_eq!(decision.transition.failures, failure);
            let pending = policy.decide(
                CycleState::Pending("core_reconciliation_pending"),
                "running",
                Duration::from_secs(2),
            );
            assert_eq!(pending.transition.failures, failure);
            assert_eq!(pending.transition.dependency, None);
        }
        let healthy = policy.decide(CycleState::Healthy, "running", Duration::ZERO);
        assert_eq!(healthy.transition.next_retry_at, None);
        assert_eq!(
            policy
                .decide(
                    CycleState::Waiting(CoreUnavailable::Transport),
                    "running",
                    Duration::ZERO
                )
                .transition
                .failures,
            1
        );
    }
    #[tokio::test]
    async fn until_shutdown_abandons_unfinished_work() -> Result<()> {
        let output =
            until_shutdown(std::future::pending::<()>(), std::future::ready(Ok(()))).await?;
        assert_eq!(output, None);
        Ok(())
    }
}
