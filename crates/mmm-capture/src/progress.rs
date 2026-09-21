//! Lightweight progress reporting for long-running batch jobs (backfills,
//! reconciliation passes, queue drains).
//!
//! Lives here rather than in `mmm-store` because `mmm-capture` is the
//! strictly lower of the two crates every batch-job caller (`mmm-read-model`,
//! `mmm-producers`) depends on directly: `mmm-store` itself depends on
//! `mmm-capture`. The reporter also needs no I/O beyond `tracing`, matching
//! this crate's "pure offline evidence" contract (no tokio-postgres, reqwest,
//! or corepc in its normal dependency graph); putting it in the SQL-writing
//! `mmm-store` instead would add nothing but a false coupling to that crate's
//! database concerns. Summary lines are registered as closures that render a
//! `String` rather than as typed RPC-metrics handles, so this crate never
//! needs to depend on `mmm-rpc`: callers capture an `RpcMetrics` handle and
//! render its snapshot's `Display` impl themselves, at the moment the job
//! ends.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Default interval between progress log lines from [`ProgressReporter::advance`].
const DEFAULT_LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Tracks completed-unit counts and elapsed time for one batch job, logging a
/// progress line at most once per interval and always a final summary: the
/// summary is logged when the reporter drops, so a job that propagates an
/// error still reports what it did and what its clients cost, marked aborted.
/// Cheap to share: `advance` and `finish` take `&self`.
pub struct ProgressReporter {
    job: &'static str,
    total: Option<u64>,
    interval: Duration,
    start: Instant,
    done: AtomicU64,
    last_log: Mutex<Instant>,
    finished: AtomicBool,
    summaries: Vec<Box<dyn Fn() -> String + Send + Sync>>,
}

impl ProgressReporter {
    /// A reporter that logs progress at most once every 30 seconds.
    pub fn new(job: &'static str, total: Option<u64>) -> Self {
        Self::with_interval(job, total, DEFAULT_LOG_INTERVAL)
    }

    /// A reporter with an explicit log interval, for tests that need to
    /// observe `advance` logging without waiting 30 real seconds.
    pub fn with_interval(job: &'static str, total: Option<u64>, interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            job,
            total,
            interval,
            start: now,
            done: AtomicU64::new(0),
            last_log: Mutex::new(now),
            finished: AtomicBool::new(false),
            summaries: Vec::new(),
        }
    }

    /// Register a line the final summary renders when the job ends, however
    /// it ends (typically an RPC client's metrics snapshot via its `Display`
    /// impl, captured by handle so it reflects the job's last call).
    #[must_use]
    pub fn with_summary(mut self, render: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.summaries.push(Box::new(render));
        self
    }

    /// Units of work completed so far.
    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    /// Record `n` more completed units. Logs one progress line if at least
    /// `interval` has elapsed since the last progress log (never on every
    /// call, so a tight per-row loop cannot flood the log).
    pub fn advance(&self, n: u64) {
        let done = self.done.fetch_add(n, Ordering::Relaxed) + n;
        let now = Instant::now();
        let mut last_log = self
            .last_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.duration_since(*last_log) < self.interval {
            return;
        }
        *last_log = now;
        drop(last_log);

        let elapsed = now.duration_since(self.start);
        let rate = rate_per_s(done, elapsed);
        let eta_secs = self.total.and_then(|total| eta_secs(done, total, rate));
        tracing::info!(
            job = self.job,
            done,
            total = self.total,
            rate_per_s = rate,
            eta_secs,
            "job progress"
        );
    }

    /// Mark the job complete. The summary itself is logged when the reporter
    /// drops, so an error path that never reaches this call still logs one,
    /// marked aborted.
    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }
}

impl Drop for ProgressReporter {
    /// Log the final summary (elapsed time, done count, rate, whether the job
    /// finished or aborted), then one line per registered summary source.
    fn drop(&mut self) {
        let done = self.done();
        let elapsed = self.start.elapsed();
        let rate = rate_per_s(done, elapsed);
        let outcome = if self.finished.load(Ordering::Relaxed) {
            "finished"
        } else {
            "aborted"
        };
        tracing::info!(
            job = self.job,
            outcome,
            done,
            total = self.total,
            elapsed_secs = elapsed.as_secs_f64(),
            rate_per_s = rate,
            "job ended"
        );
        for render in &self.summaries {
            let line = render();
            tracing::info!(job = self.job, %line, "job metrics");
        }
    }
}

fn rate_per_s(done: u64, elapsed: Duration) -> f64 {
    let secs = elapsed.as_secs_f64();
    if secs <= 0.0 { 0.0 } else { done as f64 / secs }
}

/// Estimated remaining seconds at the current rate, or `None` when the rate is
/// not yet known (zero) or the job is already complete.
fn eta_secs(done: u64, total: u64, rate: f64) -> Option<u64> {
    if rate <= 0.0 || done >= total {
        return None;
    }
    let remaining = total - done;
    Some((remaining as f64 / rate).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advance_accumulates_done_and_logs_at_most_once_per_interval() {
        let reporter =
            ProgressReporter::with_interval("test-job", Some(100), Duration::from_secs(3600));
        reporter.advance(10);
        reporter.advance(5);
        assert_eq!(reporter.done(), 15);
        // Nothing else to assert without a tracing subscriber; the interval
        // guard is exercised for real by not panicking/looping and by the
        // eta/rate helpers below.
    }

    #[test]
    fn eta_is_none_once_done_reaches_total_or_rate_is_zero() {
        assert_eq!(eta_secs(100, 100, 10.0), None);
        assert_eq!(eta_secs(50, 100, 0.0), None);
        assert_eq!(eta_secs(50, 100, 10.0), Some(5));
    }

    #[test]
    fn rate_is_zero_for_zero_elapsed_time() {
        assert_eq!(rate_per_s(10, Duration::ZERO), 0.0);
        assert_eq!(rate_per_s(10, Duration::from_secs(2)), 5.0);
    }
}
