//! Poller driver tests (moved verbatim; bodies byte-identical).

use super::*;
use std::cell::Cell;
use std::collections::HashMap;

/// Build the in-memory env map a `from_lookup` test passes as its `lookup`
/// closure, so tests never touch the real (Rust 2024 `unsafe`) process env.
fn lookup_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// Minimal `ChainPoller` for tip-fetch retry tests. Only `chain_tip` and
/// `name` are exercised; the cursor-seeding and capture methods are never
/// reached by `fetch_tip_with_retry_inner`, so they `unimplemented!()`.
/// `chain_tip` fails its first `fail_first` calls (returning an error), then
/// returns `tip` on every subsequent call.
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
    // Fails the first 3 attempts, succeeds on the 4th (within the 5-attempt
    // budget). The no-op sleeper keeps the test instant despite the helper's
    // real linear backoff in production.
    let chain = FlakyTipChain::new(3, 140_050);
    let tip = fetch_tip_with_retry_inner(&chain, |_attempt| async {}).await?;
    assert_eq!(tip, 140_050);
    assert_eq!(chain.calls.get(), 4);
    Ok(())
}

#[tokio::test]
async fn fetch_tip_with_retry_exhausts_attempts_and_errors() {
    // Always fails: all 5 attempts are spent, then a neutral (no "startup")
    // error surfaces so the message reads correctly from poll_tick too.
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

/// Arbitrary non-zero defaults the `from_lookup` tests fall back to when a key
/// is unset, chosen distinct from any override value so assertions can tell a
/// default apart from a parsed override.
const TEST_DEFAULTS: PollerDefaults = PollerDefaults {
    poll_interval_seconds: 30,
    batch_size: 100,
    reorg_depth: 64,
};

#[test]
fn from_lookup_unset_start_height_means_no_override() -> Result<()> {
    let map: HashMap<String, String> = HashMap::new();
    let config = PollerConfig::from_lookup("RSK", TEST_DEFAULTS, |key| map.get(key).cloned())?;
    // Unset START_HEIGHT now means tip-anchored / resume, not a floor default.
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
    // A NAMECOIN_* override must not bleed into an RSK_* lookup.
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
    // Override below the activation floor is lifted to the floor.
    assert_eq!(effective_start(Some(0), 139_999), 139_999);
}

#[test]
fn effective_start_honors_override_above_floor() {
    assert_eq!(effective_start(Some(800_000), 1_973), 800_000);
}

#[test]
fn seed_clamp_lifts_empty_source_below_floor() {
    // RSK start configured at 0 (below floor): a cold seed below the activation
    // floor is clamped to effective_start - 1 = 139_998.
    assert_eq!(effective_seed_cursor(-1, 139_999), 139_998);
}

#[test]
fn seed_clamp_lifts_persisted_max_below_raised_start() {
    // Existing rows up to height 500; START_HEIGHT raised to 1_000.
    // The clamp lifts the seed to 999 instead of trusting the lower max.
    assert_eq!(effective_seed_cursor(500, 1_000), 999);
}

#[test]
fn seed_clamp_is_noop_when_persisted_max_above_start() {
    // Common case: persisted max (5_000) above the configured start (1_000).
    assert_eq!(effective_seed_cursor(5_000, 1_000), 5_000);
}

#[test]
fn window_cold_start_at_floor() {
    // Empty RSK source: cursor = effective_start - 1 = 139_998.
    let window = compute_tick_window(139_999, 139_998, 140_050, 64, 100).unwrap();
    assert_eq!(window.rescan_start, 139_999);
    assert_eq!(window.end, 140_050);
}

#[test]
fn window_empty_when_tip_not_advanced() {
    // cursor at tip, no reorg depth: nothing to do.
    assert_eq!(compute_tick_window(0, 500, 500, 0, 100), None);
}

#[test]
fn window_clamped_by_batch_size() {
    let window = compute_tick_window(0, 1_000, 100_000, 16, 100).unwrap();
    assert_eq!(window.rescan_start, 985); // 1000 + 1 - 16
    assert_eq!(window.end, 1_100); // 1000 + 100
}

#[test]
fn window_start_below_floor_reaches_floor() {
    // Configured start below the floor: effective_start lifts to the floor,
    // and the window reaches the floor once tip >= floor (never parks below).
    let effective_start = 139_999; // max(start=0, floor=139_999)
    let window = compute_tick_window(effective_start, 139_998, 140_000, 64, 100).unwrap();
    assert_eq!(window.rescan_start, 139_999);
    assert_eq!(window.end, 140_000);
}

#[test]
fn window_cold_start_above_floor_begins_at_start() {
    // RSK_START_HEIGHT = 729_000, reorg_depth = 64, cold seed cursor 728_999.
    // The first window begins exactly at 729_000, not 728_936.
    let window = compute_tick_window(729_000, 728_999, 740_000, 64, 100).unwrap();
    assert_eq!(window.rescan_start, 729_000);
    assert_eq!(window.end, 729_099);
}

#[test]
fn window_cursor_ahead_of_tip_caps_at_tip() {
    // The node rolled back: cursor (1_000) is ahead of tip (980), but the
    // tip is still inside the reorg window. The window must cap at the tip.
    let window = compute_tick_window(0, 1_000, 980, 64, 100).unwrap();
    assert_eq!(window.rescan_start, 937); // 1000 + 1 - 64
    assert_eq!(window.end, 980); // min(tip=980, 1000+100)
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
        async |height| {
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
        async |height| {
            seen.push(height);
            if height == 7 {
                anyhow::bail!("boom at {height}");
            }
            Ok(HeightProgress::Advance)
        },
    )
    .await;
    assert!(result.is_err());
    // Heights 5 and 6 completed; the cursor reflects them, 8/9 never ran.
    assert_eq!(seen, vec![5, 6, 7]);
    assert_eq!(cursor, 6);
    Ok(())
}

#[tokio::test]
async fn tick_policy_replay_failure_does_not_starve_tip() -> Result<()> {
    // cursor is ahead; the trailing window replays 3..=5 and adds new 6..=7.
    let mut cursor = 5;
    let mut seen = Vec::new();
    let processed = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 3,
            end: 7,
        },
        async |height| {
            seen.push(height);
            if height == 4 {
                // A replayed (already-processed) height fails.
                anyhow::bail!("replay boom at {height}");
            }
            Ok(HeightProgress::Advance)
        },
    )
    .await?;
    // The failed replay height 4 did not block the new heights 6 and 7.
    assert_eq!(seen, vec![3, 4, 5, 6, 7]);
    assert_eq!(cursor, 7);
    // 3, 5 (replay successes) + 6, 7 (new) = 4 processed; 4 failed.
    assert_eq!(processed, 4);
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
        async |height| {
            seen.push(height);
            if height == 6 {
                Ok(HeightProgress::Hold)
            } else {
                Ok(HeightProgress::Advance)
            }
        },
    )
    .await?;
    // 5 advanced; 6 held and stopped the range; 7/8 never ran.
    assert_eq!(seen, vec![5, 6]);
    assert_eq!(cursor, 5);
    assert_eq!(processed, 1);
    Ok(())
}

#[tokio::test]
async fn tick_policy_new_height_abort_bails() -> Result<()> {
    // The Hathor nBits-table horizon maps TableHorizonHold -> Abort: a new
    // height returning Abort must error the tick (not silently Hold like a
    // not-yet-captured height), after advancing the heights below it.
    let mut cursor = 4;
    let mut seen = Vec::new();
    let result = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 5,
            end: 8,
        },
        async |height| {
            seen.push(height);
            if height == 6 {
                Ok(HeightProgress::Abort)
            } else {
                Ok(HeightProgress::Advance)
            }
        },
    )
    .await;
    assert!(result.is_err());
    // 5 advanced; 6 aborted the tick; 7/8 never ran.
    assert_eq!(seen, vec![5, 6]);
    assert_eq!(cursor, 5);
    Ok(())
}

#[tokio::test]
async fn tick_policy_replay_abort_bails() -> Result<()> {
    // Unlike a best-effort replay *error* (swallowed so it never starves the
    // tip), an Abort in the replay sub-range stops the whole tick.
    let mut cursor = 5;
    let mut seen = Vec::new();
    let result = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 3,
            end: 7,
        },
        async |height| {
            seen.push(height);
            if height == 4 {
                Ok(HeightProgress::Abort)
            } else {
                Ok(HeightProgress::Advance)
            }
        },
    )
    .await;
    assert!(result.is_err());
    // 3 replayed; 4 aborted; replay 5 and the new heights 6/7 never ran.
    assert_eq!(seen, vec![3, 4]);
    assert_eq!(cursor, 5);
    Ok(())
}

#[tokio::test]
async fn tick_policy_cursor_ahead_of_tip_only_replays() -> Result<()> {
    // cursor (1_000) ahead of tip; window ends at tip (980). Only the
    // replay sub-range runs, capped at the tip; no new heights requested.
    let mut cursor = 1_000;
    let mut seen = Vec::new();
    let processed = run_tick_policy(
        &mut cursor,
        TickWindow {
            rescan_start: 978,
            end: 980,
        },
        async |height| {
            seen.push(height);
            Ok(HeightProgress::Advance)
        },
    )
    .await?;
    assert_eq!(seen, vec![978, 979, 980]);
    assert_eq!(cursor, 1_000); // unchanged: never requested above the tip
    assert_eq!(processed, 3);
    Ok(())
}

#[tokio::test]
async fn wait_for_next_tick_continues_when_interval_elapses() -> Result<()> {
    let decision =
        wait_for_next_tick_or_shutdown(Duration::from_millis(1), std::future::pending()).await?;
    assert_eq!(decision, PollLoopDecision::Continue);
    Ok(())
}

#[tokio::test]
async fn wait_for_next_tick_stops_when_shutdown_fires() -> Result<()> {
    let decision =
        wait_for_next_tick_or_shutdown(Duration::from_secs(60), std::future::ready(Ok(()))).await?;
    assert_eq!(decision, PollLoopDecision::Shutdown);
    Ok(())
}
