//! Shared bounded-backfill argument parsing, summary counting, and simple
//! delayed range driving. Protocol-specific validation and outcome policy stay
//! in the chain modules.

use std::time::Duration;

use anyhow::{Context, Result, ensure};

use crate::chains::spec::ChainSpec;

/// Shared bounded-backfill argument config. Usage strings and validation
/// errors are byte-identical to the historical per-chain configs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BackfillConfig {
    pub(crate) spec: &'static ChainSpec,
    /// Inclusive first height to capture.
    pub(crate) start_height: i32,
    /// Inclusive last height to capture.
    pub(crate) end_height: i32,
}

impl BackfillConfig {
    /// Parse the two positional CLI args against process env. Thin wrapper over
    /// `from_args_with_lookup` using the real env lookup; tests use the
    /// injectable variant to keep the range-cap check off process env.
    pub(crate) fn from_args<I, S>(spec: &'static ChainSpec, args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::from_args_with_lookup(spec, args, crate::chains::config::env_lookup)
    }

    /// Lookup-injectable variant so range-cap tests never depend on process
    /// env (the cap check runs at PARSE time, before any DB or RPC bootstrap,
    /// preserving the historical usage-error-without-Postgres ordering).
    pub(crate) fn from_args_with_lookup<I, S, F>(
        spec: &'static ChainSpec,
        args: I,
        lookup: F,
    ) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
        F: Fn(&str) -> Option<String>,
    {
        let args = args.into_iter().map(Into::into).collect::<Vec<_>>();
        if args.len() != 2 {
            anyhow::bail!("usage: backfill-{} <start-height> <end-height>", spec.slug);
        }

        let start_height = parse_height("start-height", &args[0])?;
        let end_height = parse_height("end-height", &args[1])?;
        let config = Self {
            spec,
            start_height,
            end_height,
        };
        config.validate_range()?;
        if let Some(cap) = &spec.backfill_range_cap {
            let max_range = crate::chains::config::max_backfill_range_from_lookup(spec, &lookup);
            let allow_large =
                crate::chains::config::allow_large_backfill_from_lookup(spec, &lookup);
            let range = config.end_height - config.start_height + 1;
            if range > max_range && !allow_large {
                anyhow::bail!(
                    "{} backfill range {range} exceeds {}_MAX_BACKFILL_RANGE {max_range}; \
                 set {}_ALLOW_LARGE_BACKFILL=1 to override ({})",
                    spec.display_name,
                    spec.env_prefix,
                    spec.env_prefix,
                    cap.note,
                );
            }
        }
        Ok(config)
    }

    /// Reject a range that runs past the live tip. Runs after RPC bootstrap (the
    /// tip is fetched), so unlike the range cap this cannot be checked at parse
    /// time. The error string is part of the CLI contract (asserted in tests).
    pub(crate) fn validate_against_tip(&self, chain_tip: i32) -> Result<()> {
        if self.end_height > chain_tip {
            anyhow::bail!(
                "requested end height {} exceeds observed {} chain tip {}",
                self.end_height,
                self.spec.display_name,
                chain_tip
            );
        }
        Ok(())
    }

    /// Non-negative start and `end >= start`. Error strings are part of the CLI
    /// contract (asserted in tests verbatim).
    fn validate_range(&self) -> Result<()> {
        ensure!(self.start_height >= 0, "start-height must be non-negative");
        ensure!(
            self.end_height >= self.start_height,
            "end-height must be greater than or equal to start-height"
        );
        Ok(())
    }
}

/// Parse one height arg to `i32`. `label` names the field in the error so the
/// "start-height must be a valid i32 height" contract message identifies which
/// arg failed.
pub(crate) fn parse_height(label: &'static str, value: &str) -> Result<i32> {
    value
        .parse()
        .with_context(|| format!("{label} must be a valid i32 height"))
}

/// Tally of a bounded backfill for the final log line. `processed` equals the
/// sum of the three outcome counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct BackfillSummary {
    /// Heights visited (every height in the requested inclusive range).
    pub processed: usize,
    /// Heights that produced a `merge_mining_event` upsert.
    pub auxpow_written: usize,
    /// Heights skipped as non-AuxPoW or failing the version gate.
    pub non_auxpow_skipped: usize,
    /// Heights skipped as malformed-but-claimed-AuxPoW (held, not written).
    pub malformed_skipped: usize,
}

/// Per-height contribution to a [`BackfillSummary`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackfillHeightEffect {
    AuxpowWritten,
    NonAuxpowSkipped,
    MalformedSkipped,
}

impl BackfillSummary {
    fn record(&mut self, effect: BackfillHeightEffect) {
        self.processed += 1;
        match effect {
            BackfillHeightEffect::AuxpowWritten => self.auxpow_written += 1,
            BackfillHeightEffect::NonAuxpowSkipped => self.non_auxpow_skipped += 1,
            BackfillHeightEffect::MalformedSkipped => self.malformed_skipped += 1,
        }
    }
}

/// Drive an inclusive backfill range with an optional per-height delay, folding
/// each chain-specific outcome into the shared summary format.
pub(crate) async fn run_delayed_backfill_range<F>(
    config: &BackfillConfig,
    delay_ms: u64,
    mut process_height: F,
) -> Result<BackfillSummary>
where
    F: AsyncFnMut(i32) -> Result<BackfillHeightEffect>,
{
    let mut summary = BackfillSummary::default();
    for height in config.start_height..=config.end_height {
        let effect = process_height(height).await?;
        summary.record(effect);
        sleep_backfill_delay(delay_ms).await;
    }
    Ok(summary)
}

async fn sleep_backfill_delay(delay_ms: u64) {
    if delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::spec::{ChainId, by_id};

    // Ported from the historical NamecoinBackfillConfig tests: assertion
    // bodies unchanged, the call adapted to the spec-driven config.

    #[test]
    fn accepts_a_valid_range() {
        let config = BackfillConfig::from_args(by_id(ChainId::Namecoin), ["10", "20"]).unwrap();
        assert_eq!((config.start_height, config.end_height), (10, 20));
    }

    #[test]
    fn rejects_wrong_arity_with_the_exact_usage_string() {
        let err = BackfillConfig::from_args(by_id(ChainId::Namecoin), ["10"]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "usage: backfill-namecoin <start-height> <end-height>"
        );
    }

    #[test]
    fn rejects_invalid_integer() {
        let err = BackfillConfig::from_args(by_id(ChainId::Namecoin), ["abc", "10"]).unwrap_err();
        assert!(err.to_string().contains("start-height must be a valid"));
    }

    #[test]
    fn rejects_negative_start() {
        let err = BackfillConfig::from_args(by_id(ChainId::Namecoin), ["-1", "10"]).unwrap_err();
        assert!(
            err.to_string()
                .contains("start-height must be non-negative")
        );
    }

    #[test]
    fn rejects_end_before_start() {
        let err = BackfillConfig::from_args(by_id(ChainId::Namecoin), ["10", "9"]).unwrap_err();
        assert!(
            err.to_string()
                .contains("end-height must be greater than or equal to start-height")
        );
    }

    #[test]
    fn rejects_end_above_tip() {
        let config = BackfillConfig::from_args(by_id(ChainId::Namecoin), ["10", "20"]).unwrap();
        let err = config.validate_against_tip(19).unwrap_err();
        assert!(
            err.to_string()
                .contains("requested end height 20 exceeds observed Namecoin chain tip 19")
        );
    }

    #[test]
    fn syscoin_rejects_end_above_tip() {
        let config = BackfillConfig::from_args(by_id(ChainId::Syscoin), ["10", "20"]).unwrap();
        let err = config.validate_against_tip(19).unwrap_err();
        assert!(
            err.to_string()
                .contains("requested end height 20 exceeds observed Syscoin chain tip 19")
        );
    }

    #[test]
    fn syscoin_usage_string_is_exact() {
        let err = BackfillConfig::from_args(by_id(ChainId::Syscoin), ["10"]).unwrap_err();
        assert_eq!(
            err.to_string(),
            "usage: backfill-syscoin <start-height> <end-height>"
        );
    }
}
