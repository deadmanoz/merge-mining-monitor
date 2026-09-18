//! Cheap, `Clone`-able RPC transport counters.
//!
//! Production sits hundreds of milliseconds from every remote node, and every
//! chain client already retries transient failures under a shared HTTP
//! transport policy (see the module docs on [`crate::build_rpc_client`] and
//! [`crate::post_json_rpc`]). Without visibility, a loop that quietly starts
//! making one remote call per row looks identical to a healthy one until it
//! stalls a poller tick or a backfill. [`RpcMetrics`] is a plain counter
//! handle threaded through each client so callers can log attempts, retries,
//! failures, and latency without changing any retry/timeout/ordering
//! behavior. The counters live at two levels: an attempt is one dispatched
//! HTTP request, timed from dispatch to the consumed response body; a retry
//! and a failure belong to the caller's retry loop, which alone knows when a
//! failed attempt is followed by another and when a call has given up.
//!
//! `rpc_elements` counts JSON-RPC calls carried by the HTTP attempts recorded
//! so far; it equals `http_attempts` today because every attempt carries
//! exactly one call, and will differ once a client batches several JSON-RPC
//! calls into one HTTP POST.

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// A cheap (`Arc`-backed) handle to one client's RPC transport counters.
#[derive(Debug, Clone)]
pub struct RpcMetrics(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    label: &'static str,
    http_attempts: AtomicU64,
    rpc_elements: AtomicU64,
    retries: AtomicU64,
    failures: AtomicU64,
    latency_total_nanos: AtomicU64,
    latency_max_nanos: AtomicU64,
}

impl RpcMetrics {
    /// Create a fresh, zeroed counter set labeled for display (e.g. `"core"`,
    /// `"rsk"`, `"hathor"`, `"elastos"`, a bitcoind-family chain name).
    pub fn new(label: &'static str) -> Self {
        Self(Arc::new(Inner {
            label,
            http_attempts: AtomicU64::new(0),
            rpc_elements: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            latency_total_nanos: AtomicU64::new(0),
            latency_max_nanos: AtomicU64::new(0),
        }))
    }

    pub fn label(&self) -> &'static str {
        self.0.label
    }

    /// Start timing one dispatched HTTP attempt carrying `elements` JSON-RPC
    /// calls. The attempt is recorded when the timer drops, so a request the
    /// caller's future abandons mid-flight (a cancelled fetch pipeline) is
    /// still counted with the time it was given.
    pub fn attempt(&self, elements: u32) -> AttemptTimer<'_> {
        AttemptTimer {
            metrics: self,
            elements,
            start: Instant::now(),
        }
    }

    /// Record one dispatched HTTP attempt: `elements` JSON-RPC calls carried
    /// by it and its wall-clock `latency`, from dispatch to the consumed
    /// response body. Recorded whatever the result, and only for requests
    /// that reached the transport: local waiting (a semaphore slot, a backoff
    /// sleep) is not an attempt. Async callers use [`Self::attempt`] so a
    /// cancelled call is recorded too.
    pub fn record_attempt(&self, elements: u32, latency: Duration) {
        self.0.http_attempts.fetch_add(1, Ordering::Relaxed);
        self.0
            .rpc_elements
            .fetch_add(u64::from(elements), Ordering::Relaxed);
        let nanos = u64::try_from(latency.as_nanos()).unwrap_or(u64::MAX);
        self.0
            .latency_total_nanos
            .fetch_add(nanos, Ordering::Relaxed);
        self.0.latency_max_nanos.fetch_max(nanos, Ordering::Relaxed);
    }

    /// Record one retry: the retry loop issued another iteration after a
    /// failure. Called by the loop when it actually retries, so the last
    /// iteration of an exhausted budget is never miscounted as a retry.
    pub fn record_retry(&self) {
        self.0.retries.fetch_add(1, Ordering::Relaxed);
    }

    /// Record one failed call: the caller's retry policy gave up (a permanent
    /// error, or a transient error whose retries are exhausted), whether or
    /// not its last iteration reached the transport.
    pub fn record_failure(&self) {
        self.0.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// A closure rendering this client's cumulative line when called, for a
    /// job summary that is logged when the job ends.
    pub fn summary(&self) -> impl Fn() -> String + Send + Sync + 'static {
        let metrics = self.clone();
        move || metrics.snapshot().to_string()
    }

    /// A point-in-time, `Copy`able read of the counters.
    pub fn snapshot(&self) -> RpcMetricsSnapshot {
        RpcMetricsSnapshot {
            label: self.0.label,
            http_attempts: self.0.http_attempts.load(Ordering::Relaxed),
            rpc_elements: self.0.rpc_elements.load(Ordering::Relaxed),
            retries: self.0.retries.load(Ordering::Relaxed),
            failures: self.0.failures.load(Ordering::Relaxed),
            latency_total: Duration::from_nanos(self.0.latency_total_nanos.load(Ordering::Relaxed)),
            latency_max: Duration::from_nanos(self.0.latency_max_nanos.load(Ordering::Relaxed)),
        }
    }
}

/// One dispatched attempt in flight; records itself on drop.
pub struct AttemptTimer<'a> {
    metrics: &'a RpcMetrics,
    elements: u32,
    start: Instant,
}

impl Drop for AttemptTimer<'_> {
    fn drop(&mut self) {
        self.metrics
            .record_attempt(self.elements, self.start.elapsed());
    }
}

/// A plain, `Copy`able snapshot of [`RpcMetrics`] at one point in time.
#[derive(Debug, Clone, Copy)]
pub struct RpcMetricsSnapshot {
    pub label: &'static str,
    pub http_attempts: u64,
    pub rpc_elements: u64,
    pub retries: u64,
    pub failures: u64,
    pub latency_total: Duration,
    pub latency_max: Duration,
}

impl RpcMetricsSnapshot {
    /// Mean latency per HTTP attempt; zero when no attempt has been recorded.
    pub fn latency_avg(&self) -> Duration {
        mean_latency(self.latency_total, self.http_attempts)
    }

    /// What happened between `earlier` and this snapshot: the counters'
    /// differences, for a per-tick line. The window's maximum latency is not
    /// derivable from two cumulative maxima, so a delta carries none.
    pub fn since(&self, earlier: &RpcMetricsSnapshot) -> RpcMetricsDelta {
        RpcMetricsDelta {
            label: self.label,
            http_attempts: self.http_attempts.saturating_sub(earlier.http_attempts),
            rpc_elements: self.rpc_elements.saturating_sub(earlier.rpc_elements),
            retries: self.retries.saturating_sub(earlier.retries),
            failures: self.failures.saturating_sub(earlier.failures),
            latency_total: self.latency_total.saturating_sub(earlier.latency_total),
        }
    }
}

fn mean_latency(total: Duration, attempts: u64) -> Duration {
    match u32::try_from(attempts) {
        Ok(0) | Err(_) => Duration::ZERO,
        Ok(attempts) => total / attempts,
    }
}

/// The counters' movement between two snapshots of one client.
#[derive(Debug, Clone, Copy)]
pub struct RpcMetricsDelta {
    pub label: &'static str,
    pub http_attempts: u64,
    pub rpc_elements: u64,
    pub retries: u64,
    pub failures: u64,
    pub latency_total: Duration,
}

impl RpcMetricsDelta {
    /// Mean latency per attempt in the window; zero when it saw none.
    pub fn latency_avg(&self) -> Duration {
        mean_latency(self.latency_total, self.http_attempts)
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

/// The counters every line shares: `label: attempts= elements= retries=
/// failures= latency_avg_ms=`.
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

    #[test]
    fn records_attempts_elements_and_latency() {
        let metrics = RpcMetrics::new("core");
        metrics.record_attempt(1, Duration::from_millis(100));
        metrics.record_attempt(1, Duration::from_millis(1280));
        metrics.record_retry();
        metrics.record_attempt(1, Duration::from_millis(52));

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
        metrics.record_attempt(1, Duration::from_millis(10));
        metrics.record_retry();
        metrics.record_attempt(1, Duration::from_millis(10));
        metrics.record_failure();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.http_attempts, 2);
        assert_eq!(snapshot.retries, 1);
        assert_eq!(snapshot.failures, 1);
    }

    #[test]
    fn a_delta_reports_only_what_the_window_saw() {
        let metrics = RpcMetrics::new("core");
        metrics.record_attempt(1, Duration::from_millis(900));
        let before = metrics.snapshot();
        metrics.record_attempt(1, Duration::from_millis(100));
        metrics.record_attempt(1, Duration::from_millis(300));
        metrics.record_retry();
        metrics.record_failure();
        let delta = metrics.snapshot().since(&before);
        assert_eq!(
            delta.to_string(),
            "core: attempts=2 elements=2 retries=1 failures=1 latency_avg_ms=200"
        );
    }

    #[test]
    fn a_dropped_attempt_timer_records_the_attempt() {
        let metrics = RpcMetrics::new("rsk");
        {
            let _in_flight = metrics.attempt(1);
        }
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.http_attempts, 1);
        assert_eq!(snapshot.failures, 0);
    }

    #[test]
    fn display_matches_the_documented_one_line_format() {
        let metrics = RpcMetrics::new("core");
        for _ in 0..11 {
            metrics.record_attempt(1, Duration::from_millis(300));
        }
        metrics.record_attempt(1, Duration::from_millis(1280));
        metrics.record_retry();
        // The retried call still eventually succeeds; that final attempt is
        // recorded separately from the retryable one above.
        let snapshot = metrics.snapshot();
        assert_eq!(
            snapshot.to_string(),
            format!(
                "core: attempts=12 elements=12 retries=1 failures=0 latency_avg_ms={} latency_max_ms=1280",
                snapshot.latency_avg().as_millis()
            )
        );
    }
}
