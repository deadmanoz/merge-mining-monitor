use super::*;
use std::cell::Cell;
use std::collections::HashMap;

fn lookup_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

struct FlakyTipChain {
    fail_first: u32,
    calls: Cell<u32>,
    tip: i32,
}

impl FlakyTipChain {
    fn new(fail_first: u32, tip: i32) -> Self {
        Self {
            fail_first,
            calls: Cell::new(0),
            tip,
        }
    }
}

impl ChainPoller for FlakyTipChain {
    fn name(&self) -> &'static str {
        "Flaky"
    }

    fn poller_state(&self) -> &ChainPollerState {
        unimplemented!("not used by tip-fetch retry tests")
    }

    fn client_mut(&mut self) -> &mut Client {
        unimplemented!("not used by tip-fetch retry tests")
    }

    async fn chain_tip(&self) -> Result<i32> {
        let n = self.calls.get();
        self.calls.set(n + 1);
        if n < self.fail_first {
            anyhow::bail!("transient tip failure on call {n}");
        }
        Ok(self.tip)
    }
    async fn process_height(&mut self, _height: i32) -> Result<HeightProgress> {
        unimplemented!("not used by tip-fetch retry tests")
    }
}

#[tokio::test]
async fn fetch_tip_with_retry_recovers_after_transient_failures() -> Result<()> {
    let chain = FlakyTipChain::new(3, 140_050);
    let tip = fetch_tip_with_retry_inner(&chain, |_attempt| async {}).await?;
    assert_eq!(tip, 140_050);
    assert_eq!(chain.calls.get(), 4);
    Ok(())
}

#[tokio::test]
async fn fetch_tip_with_retry_exhausts_attempts_and_errors() {
    let chain = FlakyTipChain::new(u32::MAX, 0);
    let err = fetch_tip_with_retry_inner(&chain, |_attempt| async {})
        .await
        .unwrap_err();
    assert_eq!(chain.calls.get(), 5);
    let msg = err.to_string();
    assert!(
        msg.contains("chain tip unavailable after 5 attempts"),
        "unexpected error: {msg}"
    );
    assert!(
        !msg.contains("startup"),
        "steady-state error must not mention startup: {msg}"
    );
}

const TEST_DEFAULTS: PollerDefaults = PollerDefaults {
    poll_interval_seconds: 30,
    batch_size: 100,
    reorg_depth: 64,
};

#[test]
fn child_failures_keep_the_normal_retry_policy_but_core_faults_stop_capture() -> Result<()> {
    let config = PollerConfig::from_lookup("NAMECOIN", TEST_DEFAULTS, |_| None)?;
    let mut poller = Poller {
        chain: FlakyTipChain::new(0, 0),
        cursor: 0,
        config,
    };
    assert_eq!(
        poller.wait_after_tick(Err(anyhow::anyhow!("child transport failed")))?,
        config.poll_interval
    );
    assert!(
        poller
            .wait_after_tick(Err(mmm_bitcoin_core::CoreFault::Authentication.into()))
            .is_err()
    );
    assert!(
        poller
            .wait_after_tick(Err(mmm_store::bitcoin_core_header_cache_integrity_error(
                "fixture conflict"
            )))
            .is_err()
    );
    Ok(())
}

#[test]
fn from_lookup_unset_start_height_means_no_override() -> Result<()> {
    let map: HashMap<String, String> = HashMap::new();
    let config = PollerConfig::from_lookup("RSK", TEST_DEFAULTS, |key| map.get(key).cloned())?;
    assert_eq!(config.start_height_override, None);
    assert_eq!(config.poll_interval, Duration::from_secs(30));
    assert_eq!(config.batch_size, 100);
    assert_eq!(config.reorg_depth, 64);
    Ok(())
}

#[test]
fn from_lookup_reads_prefixed_overrides() -> Result<()> {
    let map = lookup_from(&[
        ("RSK_START_HEIGHT", "200000"),
        ("RSK_POLL_INTERVAL_SECONDS", "5"),
        ("RSK_BATCH_SIZE", "10"),
        ("RSK_REORG_DEPTH", "8"),
    ]);
    let config = PollerConfig::from_lookup("RSK", TEST_DEFAULTS, |key| map.get(key).cloned())?;
    assert_eq!(config.start_height_override, Some(200_000));
    assert_eq!(config.poll_interval, Duration::from_secs(5));
    assert_eq!(config.batch_size, 10);
    assert_eq!(config.reorg_depth, 8);
    Ok(())
}

#[test]
fn from_lookup_is_prefix_scoped() -> Result<()> {
    let map = lookup_from(&[("NAMECOIN_BATCH_SIZE", "7")]);
    let config = PollerConfig::from_lookup("RSK", TEST_DEFAULTS, |key| map.get(key).cloned())?;
    assert_eq!(config.batch_size, 100);
    Ok(())
}

#[test]
fn from_lookup_rejects_invalid_values() {
    let map = lookup_from(&[("RSK_BATCH_SIZE", "0")]);
    let err =
        PollerConfig::from_lookup("RSK", TEST_DEFAULTS, |key| map.get(key).cloned()).unwrap_err();
    assert!(err.to_string().contains("RSK_BATCH_SIZE must be positive"));

    let map = lookup_from(&[("RSK_START_HEIGHT", "-1")]);
    let err =
        PollerConfig::from_lookup("RSK", TEST_DEFAULTS, |key| map.get(key).cloned()).unwrap_err();
    assert!(
        err.to_string()
            .contains("RSK_START_HEIGHT must be non-negative")
    );
}

#[test]
fn effective_start_uses_floor_without_override() {
    assert_eq!(effective_start(None, 139_999), 139_999);
}

#[test]
fn effective_start_lifts_override_to_floor() {
    assert_eq!(effective_start(Some(0), 139_999), 139_999);
}

#[test]
fn effective_start_honors_override_above_floor() {
    assert_eq!(effective_start(Some(800_000), 1_973), 800_000);
}

#[test]
fn seed_clamp_lifts_empty_source_below_floor() {
    assert_eq!(effective_seed_cursor(-1, 139_999), 139_998);
}

#[test]
fn seed_clamp_lifts_persisted_max_below_raised_start() {
    assert_eq!(effective_seed_cursor(500, 1_000), 999);
}

#[test]
fn seed_clamp_is_noop_when_persisted_max_above_start() {
    assert_eq!(effective_seed_cursor(5_000, 1_000), 5_000);
}

#[test]
fn window_cold_start_at_floor() {
    let window = compute_tick_window(139_999, 139_998, 140_050, 64, 100).unwrap();
    assert_eq!(window.rescan_start, 139_999);
    assert_eq!(window.end, 140_050);
}

#[test]
fn window_empty_when_tip_not_advanced() {
    assert_eq!(compute_tick_window(0, 500, 500, 0, 100), None);
}

#[test]
fn window_clamped_by_batch_size() {
    let window = compute_tick_window(0, 1_000, 100_000, 16, 100).unwrap();
    assert_eq!(window.rescan_start, 985);
    assert_eq!(window.end, 1_100);
}

#[test]
fn window_cold_start_above_floor_begins_at_start() {
    let window = compute_tick_window(729_000, 728_999, 740_000, 64, 100).unwrap();
    assert_eq!(window.rescan_start, 729_000);
    assert_eq!(window.end, 729_099);
}

#[test]
fn window_cursor_ahead_of_tip_caps_at_tip() {
    let window = compute_tick_window(0, 1_000, 980, 64, 100).unwrap();
    assert_eq!(window.rescan_start, 937);
    assert_eq!(window.end, 980);
}

#[tokio::test]
async fn tick_policy_advances_through_new_heights() -> Result<()> {
    let mut cursor = 4;
    let mut seen = Vec::new();
    let processed = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 5,
            end: 7,
        },
        async |height, _kind| {
            seen.push(height);
            Ok(HeightProgress::Advance)
        },
    )
    .await?;
    assert_eq!(seen, vec![5, 6, 7]);
    assert_eq!(cursor, 7);
    assert_eq!(processed, 3);
    Ok(())
}

#[tokio::test]
async fn tick_policy_preserves_partial_progress_on_new_height_error() -> Result<()> {
    let mut cursor = 4;
    let mut seen = Vec::new();
    let result = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 5,
            end: 9,
        },
        async |height, _kind| {
            seen.push(height);
            if height == 7 {
                anyhow::bail!("boom at {height}");
            }
            Ok(HeightProgress::Advance)
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(seen, vec![5, 6, 7]);
    assert_eq!(cursor, 6);
    Ok(())
}

#[tokio::test]
async fn replay_failure_preserves_tip_progress_but_not_healthy_completion() -> Result<()> {
    let mut cursor = 5;
    let mut seen = Vec::new();
    let mut progress = TickProgress::default();
    run_tick_policy_counting(
        &mut cursor,
        &mut progress,
        TickWindow {
            rescan_start: 3,
            end: 7,
        },
        async |height, _kind| {
            seen.push(height);
            if height == 4 {
                anyhow::bail!("replay boom at {height}");
            }
            Ok(HeightProgress::Advance)
        },
    )
    .await?;
    assert_eq!(seen, vec![3, 4, 5, 6, 7]);
    assert_eq!(cursor, 7);
    assert_eq!(progress.processed, 4);
    assert!(
        progress.held,
        "a failed replay must not reset outage backoff or report a healthy cycle"
    );
    Ok(())
}

#[tokio::test]
async fn replay_stops_at_a_core_outage_without_attempting_new_heights() -> Result<()> {
    let mut cursor = 5;
    let mut seen = Vec::new();
    let result = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 3,
            end: 7,
        },
        async |height, _kind| {
            seen.push(height);
            if height == 4 {
                return Err(mmm_bitcoin_core::CoreUnavailable::Warmup.into());
            }
            Ok(HeightProgress::Advance)
        },
    )
    .await;
    assert_eq!(
        result
            .unwrap_err()
            .downcast_ref::<mmm_bitcoin_core::CoreUnavailable>(),
        Some(&mmm_bitcoin_core::CoreUnavailable::Warmup)
    );
    assert_eq!(seen, [3, 4]);
    assert_eq!(cursor, 5);
    Ok(())
}

#[tokio::test]
async fn tick_policy_hold_stops_without_advancing() -> Result<()> {
    let mut cursor = 4;
    let mut seen = Vec::new();
    let processed = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 5,
            end: 8,
        },
        async |height, _kind| {
            seen.push(height);
            if height == 6 {
                Ok(HeightProgress::Hold)
            } else {
                Ok(HeightProgress::Advance)
            }
        },
    )
    .await?;
    assert_eq!(seen, vec![5, 6]);
    assert_eq!(cursor, 5);
    assert_eq!(processed, 1);
    Ok(())
}

/// Replay `Hold` does not stop the tick or lower the monotonic cursor, which is
/// why held Qbit heights persist a durable `capture_error` row.
#[tokio::test]
async fn tick_policy_replay_hold_continues_without_lowering_the_cursor() -> Result<()> {
    let mut cursor = 6;
    let mut seen = Vec::new();
    let processed = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 4,
            end: 8,
        },
        async |height, _kind| {
            seen.push(height);
            if height == 5 {
                Ok(HeightProgress::Hold)
            } else {
                Ok(HeightProgress::Advance)
            }
        },
    )
    .await?;
    assert_eq!(seen, vec![4, 5, 6, 7, 8]);
    assert_eq!(cursor, 8, "a replay hold never lowers the cursor");
    assert_eq!(processed, 5);
    Ok(())
}

#[tokio::test]
async fn tick_policy_cursor_ahead_of_tip_only_replays() -> Result<()> {
    let mut cursor = 1_000;
    let mut seen = Vec::new();
    let processed = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 978,
            end: 980,
        },
        async |height, _kind| {
            seen.push(height);
            Ok(HeightProgress::Advance)
        },
    )
    .await?;
    assert_eq!(seen, vec![978, 979, 980]);
    assert_eq!(cursor, 1_000);
    assert_eq!(processed, 3);
    Ok(())
}
