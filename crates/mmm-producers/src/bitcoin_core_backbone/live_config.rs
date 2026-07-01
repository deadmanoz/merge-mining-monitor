//! Bitcoin Core live-mode config defaults and env parsing.

use std::time::Duration;

use anyhow::{Context, Result, bail};

/// Default per-batch height limit when `--follow` is set without an explicit
/// `--limit`. Kept small so a catch-up batch finishes well within the live-test
/// manager's stop grace. The continuous loop keeps overall catch-up throughput,
/// since per-batch overhead is cheap after incremental cursor advance.
pub(super) const FOLLOW_DEFAULT_LIMIT: i64 = 50;
/// Default follow-mode poll interval (seconds) when
/// `BITCOIN_CORE_SYNC_FOLLOW_INTERVAL_SECS` is unset.
pub(super) const DEFAULT_FOLLOW_INTERVAL_SECS: u64 = 60;
/// Default number of recent Core heights kept complete for the tree live-tip
/// window. Operators can lower this with `BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS`
/// when using a large per-height delay, but it must still cover the tree's
/// default 16-block window.
pub(super) const DEFAULT_NEAR_TIP_REPAIR_WINDOW_HEIGHTS: i32 = 64;
/// Floor enforced on the operator-configured live window: it must still cover
/// the tree's default 16-block live-tip window, so a smaller value is rejected.
const MIN_NEAR_TIP_REPAIR_WINDOW_HEIGHTS: i32 = 16;

pub(super) fn parse_follow_interval_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Duration> {
    match lookup("BITCOIN_CORE_SYNC_FOLLOW_INTERVAL_SECS") {
        Some(value) => {
            let secs: u64 = value.parse().with_context(|| {
                format!("BITCOIN_CORE_SYNC_FOLLOW_INTERVAL_SECS has invalid value {value:?}")
            })?;
            if secs == 0 {
                bail!("BITCOIN_CORE_SYNC_FOLLOW_INTERVAL_SECS must be greater than 0");
            }
            Ok(Duration::from_secs(secs))
        }
        None => Ok(Duration::from_secs(DEFAULT_FOLLOW_INTERVAL_SECS)),
    }
}

pub(super) fn parse_near_tip_repair_window_from_lookup(
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<i32> {
    match lookup("BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS") {
        Some(value) => {
            let heights: i32 = value.parse().with_context(|| {
                format!("BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS has invalid value {value:?}")
            })?;
            if heights < MIN_NEAR_TIP_REPAIR_WINDOW_HEIGHTS {
                bail!(
                    "BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS must be at least \
                     {MIN_NEAR_TIP_REPAIR_WINDOW_HEIGHTS}"
                );
            }
            Ok(heights)
        }
        None => Ok(DEFAULT_NEAR_TIP_REPAIR_WINDOW_HEIGHTS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bitcoin_core_backbone::BitcoinCoreSyncConfig;

    #[test]
    fn follow_forces_tip_and_missing_only_and_default_limit() {
        let cfg = BitcoinCoreSyncConfig::from_args_with_lookup(["--follow"], |_| None)
            .expect("follow parses");
        assert!(cfg.follow, "follow set");
        assert!(cfg.tip, "follow forces tip");
        assert!(cfg.missing_only, "follow forces missing_only");
        assert_eq!(cfg.limit, FOLLOW_DEFAULT_LIMIT, "follow default limit");
        assert_eq!(
            cfg.near_tip_repair_window_heights, DEFAULT_NEAR_TIP_REPAIR_WINDOW_HEIGHTS,
            "follow default live window"
        );
        assert_eq!(
            cfg.follow_interval,
            Duration::from_secs(DEFAULT_FOLLOW_INTERVAL_SECS),
            "follow default interval"
        );
    }

    #[test]
    fn follow_explicit_limit_overrides_default() {
        let cfg =
            BitcoinCoreSyncConfig::from_args_with_lookup(["--follow", "--limit", "500"], |_| None)
                .expect("follow + limit parses");
        assert_eq!(cfg.limit, 500);
    }

    #[test]
    fn follow_live_window_env_overrides_default() {
        let cfg = BitcoinCoreSyncConfig::from_args_with_lookup(["--follow"], |key| match key {
            "BITCOIN_CORE_SYNC_LIVE_WINDOW_HEIGHTS" => Some("32".to_owned()),
            _ => None,
        })
        .expect("follow + live window env parses");
        assert_eq!(cfg.near_tip_repair_window_heights, 32);
    }

    #[test]
    fn follow_rejects_height_bounds_but_accepts_redundant_missing_only() {
        assert!(
            BitcoinCoreSyncConfig::from_args_with_lookup(
                ["--follow", "--from-height", "5"],
                |_| None
            )
            .is_err(),
            "follow + from-height rejected"
        );
        assert!(
            BitcoinCoreSyncConfig::from_args_with_lookup(["--follow", "--to-height", "5"], |_| {
                None
            })
            .is_err(),
            "follow + to-height rejected"
        );
        let cfg =
            BitcoinCoreSyncConfig::from_args_with_lookup(["--follow", "--missing-only"], |_| None)
                .expect("follow + redundant missing-only accepted");
        assert!(cfg.follow && cfg.missing_only);
    }

    #[test]
    fn follow_interval_parses_validates_and_defaults() {
        assert_eq!(
            parse_follow_interval_from_lookup(|_| Some("30".to_owned())).expect("valid"),
            Duration::from_secs(30)
        );
        assert_eq!(
            parse_follow_interval_from_lookup(|_| None).expect("unset default"),
            Duration::from_secs(DEFAULT_FOLLOW_INTERVAL_SECS)
        );
        assert!(
            parse_follow_interval_from_lookup(|_| Some("0".to_owned())).is_err(),
            "zero rejected"
        );
        assert!(
            parse_follow_interval_from_lookup(|_| Some("not-a-number".to_owned())).is_err(),
            "non-numeric rejected"
        );
    }

    #[test]
    fn near_tip_repair_window_parses_validates_and_defaults() {
        assert_eq!(
            parse_near_tip_repair_window_from_lookup(|_| Some("32".to_owned())).expect("valid"),
            32
        );
        assert_eq!(
            parse_near_tip_repair_window_from_lookup(|_| None).expect("unset default"),
            DEFAULT_NEAR_TIP_REPAIR_WINDOW_HEIGHTS
        );
        assert!(
            parse_near_tip_repair_window_from_lookup(|_| Some("15".to_owned())).is_err(),
            "too-small window rejected"
        );
        assert!(
            parse_near_tip_repair_window_from_lookup(|_| Some("not-a-number".to_owned())).is_err(),
            "non-numeric rejected"
        );
    }

    #[test]
    fn from_args_consults_follow_interval_only_with_follow() {
        assert!(
            BitcoinCoreSyncConfig::from_args_with_lookup(["--follow"], |_| Some("0".to_owned()))
                .is_err(),
            "invalid follow interval rejected with --follow"
        );
        let cfg = BitcoinCoreSyncConfig::from_args_with_lookup(["--tip"], |_| {
            panic!("env lookup must not be consulted without --follow")
        })
        .expect("one-shot config parses without touching the follow env");
        assert!(!cfg.follow);
    }
}
