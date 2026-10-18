//! Lightweight progress reporting for long-running batch jobs (backfills,
//! reconciliation passes, queue drains) with closure-rendered RPC metrics summaries.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Tracks completed units and elapsed time for a batch job, logging progress at most once
/// per [`LOG_INTERVAL`] and emitting a final summary on drop (`finished` or `aborted`).
pub struct ProgressReporter {
    job: &'static str,
    total: Option<u64>,
    start: Instant,
    done: AtomicU64,
    last_log: Mutex<Instant>,
    finished: AtomicBool,
    summaries: Vec<Box<dyn Fn() -> String + Send + Sync>>,
}

impl ProgressReporter {
    pub fn new(job: &'static str, total: Option<u64>) -> Self {
        let now = Instant::now();
        Self {
            job,
            total,
            start: now,
            done: AtomicU64::new(0),
            last_log: Mutex::new(now),
            finished: AtomicBool::new(false),
            summaries: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_total(mut self, total: u64) -> Self {
        self.total = Some(total);
        self
    }

    #[must_use]
    pub fn with_summary(mut self, render: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.summaries.push(Box::new(render));
        self
    }

    pub fn done(&self) -> u64 {
        self.done.load(Ordering::Relaxed)
    }

    pub fn advance(&self, n: u64) {
        let done = self.done.fetch_add(n, Ordering::Relaxed) + n;
        let now = Instant::now();
        let mut last_log = self
            .last_log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if now.duration_since(*last_log) < LOG_INTERVAL {
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

    pub fn finish(&self) {
        self.finished.store(true, Ordering::Relaxed);
    }
}

impl Drop for ProgressReporter {
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
    fn advance_accumulates_done() {
        let reporter = ProgressReporter::new("test-job", Some(100));
        reporter.advance(10);
        reporter.advance(5);
        assert_eq!(reporter.done(), 15);
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
