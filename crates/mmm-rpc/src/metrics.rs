//! Atomic RPC transport counters (`attempts`, `elements`, `retries`, `failures`, and latency).
//!
//! Attempts are counted at HTTP dispatch and timed on [`AttemptTimer`] drop (pairing
//! latency with `completed` attempts in each window). Retries and terminal failures
//! are recorded by the caller's retry loop.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct RpcMetrics(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    label: &'static str,
    http_attempts: AtomicU64,
    rpc_elements: AtomicU64,
    retries: AtomicU64,
    failures: AtomicU64,
    completed: AtomicU64,
    latency_total_nanos: AtomicU64,
    latency_max_nanos: AtomicU64,
}

impl RpcMetrics {
    pub fn new(label: &'static str) -> Self {
        Self(Arc::new(Inner {
            label,
            http_attempts: AtomicU64::new(0),
            rpc_elements: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            completed: AtomicU64::new(0),
            latency_total_nanos: AtomicU64::new(0),
            latency_max_nanos: AtomicU64::new(0),
        }))
    }

    pub fn label(&self) -> &'static str {
        self.0.label
    }

    /// Count one dispatched HTTP attempt carrying `elements` JSON-RPC calls; records latency on drop.
    pub fn attempt(&self, elements: u32) -> AttemptTimer<'_> {
        self.record_dispatch(elements);
        AttemptTimer {
            metrics: self,
            start: Instant::now(),
        }
    }

    fn record_dispatch(&self, elements: u32) {
        self.0.http_attempts.fetch_add(1, Ordering::Relaxed);
        self.0
            .rpc_elements
            .fetch_add(u64::from(elements), Ordering::Relaxed);
    }

    fn record_latency(&self, latency: Duration) {
        self.0.completed.fetch_add(1, Ordering::Relaxed);
        let nanos = u64::try_from(latency.as_nanos()).unwrap_or(u64::MAX);
        self.0
            .latency_total_nanos
            .fetch_add(nanos, Ordering::Relaxed);
        self.0.latency_max_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    pub fn record_retry(&self) {
        self.0.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_failure(&self) {
        self.0.failures.fetch_add(1, Ordering::Relaxed);
    }

    pub fn summary(&self) -> impl Fn() -> String + Send + Sync + 'static {
        let metrics = self.clone();
        move || metrics.snapshot().to_string()
    }

    pub fn snapshot(&self) -> RpcMetricsSnapshot {
        RpcMetricsSnapshot {
            label: self.0.label,
            http_attempts: self.0.http_attempts.load(Ordering::Relaxed),
            rpc_elements: self.0.rpc_elements.load(Ordering::Relaxed),
            retries: self.0.retries.load(Ordering::Relaxed),
            failures: self.0.failures.load(Ordering::Relaxed),
            completed: self.0.completed.load(Ordering::Relaxed),
            latency_total: Duration::from_nanos(self.0.latency_total_nanos.load(Ordering::Relaxed)),
            latency_max: Duration::from_nanos(self.0.latency_max_nanos.load(Ordering::Relaxed)),
        }
    }
}

/// Guard for an in-flight attempt; records latency on drop.
pub struct AttemptTimer<'a> {
    metrics: &'a RpcMetrics,
    start: Instant,
}

impl Drop for AttemptTimer<'_> {
    fn drop(&mut self) {
        self.metrics.record_latency(self.start.elapsed());
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RpcMetricsSnapshot {
    pub label: &'static str,
    pub http_attempts: u64,
    pub rpc_elements: u64,
    pub retries: u64,
    pub failures: u64,
    pub completed: u64,
    pub latency_total: Duration,
    pub latency_max: Duration,
}

impl RpcMetricsSnapshot {
    pub fn latency_avg(&self) -> Duration {
        mean_latency(self.latency_total, self.completed)
    }

    pub fn since(&self, earlier: &RpcMetricsSnapshot) -> RpcMetricsDelta {
        RpcMetricsDelta {
            label: self.label,
            http_attempts: self.http_attempts.saturating_sub(earlier.http_attempts),
            rpc_elements: self.rpc_elements.saturating_sub(earlier.rpc_elements),
            retries: self.retries.saturating_sub(earlier.retries),
            failures: self.failures.saturating_sub(earlier.failures),
            completed: self.completed.saturating_sub(earlier.completed),
            latency_total: self.latency_total.saturating_sub(earlier.latency_total),
        }
    }
}

fn mean_latency(total: Duration, completed: u64) -> Duration {
    match u32::try_from(completed) {
        Ok(0) | Err(_) => Duration::ZERO,
        Ok(completed) => total / completed,
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RpcMetricsDelta {
    pub label: &'static str,
    pub http_attempts: u64,
    pub rpc_elements: u64,
    pub retries: u64,
    pub failures: u64,
    pub completed: u64,
    pub latency_total: Duration,
}

impl RpcMetricsDelta {
    pub fn latency_avg(&self) -> Duration {
        mean_latency(self.latency_total, self.completed)
    }
}

impl fmt::Display for RpcMetricsDelta {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_counters(
            f,
            self.label,
            self.http_attempts,
            self.rpc_elements,
            self.retries,
            self.failures,
            self.latency_avg(),
        )
    }
}

fn write_counters(
    f: &mut fmt::Formatter<'_>,
    label: &str,
    http_attempts: u64,
    rpc_elements: u64,
    retries: u64,
    failures: u64,
    latency_avg: Duration,
) -> fmt::Result {
    write!(
        f,
        "{label}: attempts={http_attempts} elements={rpc_elements} retries={retries} \
         failures={failures} latency_avg_ms={}",
        latency_avg.as_millis()
    )
}

impl fmt::Display for RpcMetricsSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_counters(
            f,
            self.label,
            self.http_attempts,
            self.rpc_elements,
            self.retries,
            self.failures,
            self.latency_avg(),
        )?;
        write!(f, " latency_max_ms={}", self.latency_max.as_millis())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One attempt that completed after `latency`, without sleeping for it.
    fn completed_attempt(metrics: &RpcMetrics, latency: Duration) {
        metrics.record_dispatch(1);
        metrics.record_latency(latency);
    }

    #[test]
    fn records_attempts_elements_and_latency() {
        let metrics = RpcMetrics::new("core");
        completed_attempt(&metrics, Duration::from_millis(100));
        completed_attempt(&metrics, Duration::from_millis(1280));
        metrics.record_retry();
        completed_attempt(&metrics, Duration::from_millis(52));

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.http_attempts, 3);
        assert_eq!(snapshot.rpc_elements, 3);
        assert_eq!(snapshot.retries, 1);
        assert_eq!(snapshot.failures, 0);
        assert_eq!(snapshot.latency_max, Duration::from_millis(1280));
    }

    #[test]
    fn a_failure_is_the_call_giving_up_not_an_attempt() {
        let metrics = RpcMetrics::new("rsk");
        completed_attempt(&metrics, Duration::from_millis(10));
        metrics.record_retry();
        completed_attempt(&metrics, Duration::from_millis(10));
        metrics.record_failure();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.http_attempts, 2);
        assert_eq!(snapshot.retries, 1);
        assert_eq!(snapshot.failures, 1);
    }

    #[test]
    fn a_delta_reports_only_what_the_window_saw() {
        let metrics = RpcMetrics::new("core");
        completed_attempt(&metrics, Duration::from_millis(900));
        let before = metrics.snapshot();
        completed_attempt(&metrics, Duration::from_millis(100));
        completed_attempt(&metrics, Duration::from_millis(300));
        metrics.record_retry();
        metrics.record_failure();
        let delta = metrics.snapshot().since(&before);
        assert_eq!(
            delta.to_string(),
            "core: attempts=2 elements=2 retries=1 failures=1 latency_avg_ms=200"
        );
    }

    #[test]
    fn an_attempt_timer_counts_at_dispatch_and_times_on_drop() {
        let metrics = RpcMetrics::new("rsk");
        {
            let _in_flight = metrics.attempt(1);
            let dispatched = metrics.snapshot();
            assert_eq!(dispatched.http_attempts, 1);
            assert_eq!(dispatched.rpc_elements, 1);
            assert_eq!(dispatched.latency_total, Duration::ZERO);
            std::thread::sleep(Duration::from_millis(5));
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.http_attempts, 1);
        assert_eq!(snapshot.completed, 1);
        assert!(snapshot.latency_max >= Duration::from_millis(5));
        assert_eq!(snapshot.failures, 0);
    }

    #[test]
    fn a_window_averages_the_latency_of_the_attempts_that_completed_in_it() {
        // An attempt dispatched in one window and completed in the next is
        // counted by the first and timed by the second, whose mean pairs the
        // latency with that attempt rather than with the window's own.
        let metrics = RpcMetrics::new("core");
        let start = metrics.snapshot();
        let in_flight = metrics.attempt(1);
        let boundary = metrics.snapshot();
        let first = boundary.since(&start);
        assert_eq!(first.http_attempts, 1);
        assert_eq!(first.completed, 0);
        assert_eq!(first.latency_avg(), Duration::ZERO);

        std::thread::sleep(Duration::from_millis(5));
        drop(in_flight);
        let idle = metrics.snapshot().since(&boundary);
        assert_eq!(idle.http_attempts, 0);
        assert_eq!(idle.completed, 1);
        assert!(idle.latency_avg() >= Duration::from_millis(5));

        completed_attempt(&metrics, Duration::from_millis(100));
        let busy = metrics.snapshot().since(&boundary);
        assert_eq!(busy.http_attempts, 1);
        assert_eq!(busy.completed, 2);
        assert!(busy.latency_avg() >= Duration::from_millis(52));
        assert!(busy.latency_avg() < Duration::from_millis(100));
    }

    #[test]
    fn display_matches_the_documented_one_line_format() {
        let metrics = RpcMetrics::new("core");
        for _ in 0..11 {
            completed_attempt(&metrics, Duration::from_millis(300));
        }
        completed_attempt(&metrics, Duration::from_millis(1280));
        metrics.record_retry();
        // The retried call still eventually succeeds; that final attempt is
        // recorded separately from the retryable one above.
        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot.to_string(),
            "core: attempts=12 elements=12 retries=1 failures=0 latency_avg_ms=381 latency_max_ms=1280"
        );
    }
}
