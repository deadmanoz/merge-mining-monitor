//! Spec-driven environment configuration for chain producers (`src/chains/`'s
//! sole process-env boundary).

use std::env;
use std::fs;

use anyhow::{Context, Result, bail};

use crate::chains::bitcoind_rpc::BitcoindRpcConfig;
use crate::chains::spec::{ChainSpec, RpcAuth};
use crate::poller::PollerConfig;
use mmm_rpc as rpc_http;
use rpc_http::{DEFAULT_RPC_TIMEOUT_SECS, parse_timeout_secs_from_lookup};

pub(crate) fn env_lookup(key: &str) -> Option<String> {
    env::var(key).ok()
}

pub(crate) fn poller_config(spec: &ChainSpec) -> Result<PollerConfig> {
    poller_config_from_lookup(spec, env_lookup)
}

pub(crate) fn poller_config_from_lookup<F>(spec: &ChainSpec, lookup: F) -> Result<PollerConfig>
where
    F: Fn(&str) -> Option<String>,
{
    PollerConfig::from_lookup(spec.env_prefix, spec.poller, lookup)
}

pub(crate) fn bitcoind_rpc_config(spec: &ChainSpec) -> Result<BitcoindRpcConfig> {
    bitcoind_rpc_config_from_lookup(spec, env_lookup)
}

pub(crate) fn bitcoind_rpc_config_from_lookup<F>(
    spec: &ChainSpec,
    lookup: F,
) -> Result<BitcoindRpcConfig>
where
    F: Fn(&str) -> Option<String>,
{
    let prefix = spec.env_prefix;
    let family = spec
        .family
        .as_ref()
        .with_context(|| format!("{} is not a bitcoind-family chain", spec.slug))?;

    // Evaluation order is part of each preserved contract: RequiredUserPass
    // resolves URL -> USER -> PASSWORD; OptionalUserPassOrCookie resolves the
    // auth pair first, then URL.
    let (url, user, password) = match family.auth {
        RpcAuth::RequiredUserPass => {
            let url = lookup(&format!("{prefix}_RPC_URL"))
                .with_context(|| format!("{prefix}_RPC_URL is required"))?;
            let user = lookup(&format!("{prefix}_RPC_USER"))
                .with_context(|| format!("{prefix}_RPC_USER is required"))?;
            let password = lookup(&format!("{prefix}_RPC_PASSWORD"))
                .with_context(|| format!("{prefix}_RPC_PASSWORD is required"))?;
            (url, Some(user), Some(password))
        }
        RpcAuth::OptionalUserPassOrCookie => {
            let (user, password) = match (
                lookup(&format!("{prefix}_RPC_USER")),
                lookup(&format!("{prefix}_RPC_PASSWORD")),
            ) {
                (Some(user), Some(password)) => (Some(user), Some(password)),
                (None, None) => match lookup(&format!("{prefix}_RPC_COOKIEFILE")) {
                    Some(path) => {
                        let cookie = fs::read_to_string(&path)
                            .with_context(|| format!("read {prefix}_RPC_COOKIEFILE {path}"))?;
                        let (user, password) = cookie.trim().split_once(':').ok_or_else(|| {
                            anyhow::anyhow!("{prefix}_RPC_COOKIEFILE is malformed")
                        })?;
                        (Some(user.to_owned()), Some(password.to_owned()))
                    }
                    None => (None, None),
                },
                _ => bail!("{prefix}_RPC_USER and {prefix}_RPC_PASSWORD must be set together"),
            };
            let url = lookup(&format!("{prefix}_RPC_URL"))
                .with_context(|| format!("{prefix}_RPC_URL is required"))?;
            (url, user, password)
        }
    };

    Ok(BitcoindRpcConfig {
        url,
        user,
        password,
        raw_block_boolean_verbose: family.raw_block_boolean_verbose,
        request_timeout: parse_timeout_secs_from_lookup(
            &format!("{prefix}_RPC_TIMEOUT_SECS"),
            DEFAULT_RPC_TIMEOUT_SECS,
            &lookup,
        )?,
    })
}

pub(crate) fn rsk_rpc_config() -> Result<crate::chains::rsk::rpc::RskRpcConfig> {
    rsk_rpc_config_from_lookup(env_lookup)
}

pub(crate) fn rsk_rpc_config_from_lookup<F>(
    lookup: F,
) -> Result<crate::chains::rsk::rpc::RskRpcConfig>
where
    F: Fn(&str) -> Option<String>,
{
    Ok(crate::chains::rsk::rpc::RskRpcConfig {
        url: lookup("RSK_RPC_URL").context("RSK_RPC_URL is required")?,
        user: lookup("RSK_RPC_USER"),
        password: lookup("RSK_RPC_PASSWORD"),
        request_timeout: parse_timeout_secs_from_lookup(
            "RSK_RPC_TIMEOUT_SECS",
            DEFAULT_RPC_TIMEOUT_SECS,
            &lookup,
        )?,
    })
}

pub(crate) fn rsk_backfill_fetch_concurrency() -> Result<usize> {
    rsk_backfill_fetch_concurrency_from_lookup(env_lookup)
}

pub(crate) fn rsk_backfill_fetch_concurrency_from_lookup<F>(lookup: F) -> Result<usize>
where
    F: Fn(&str) -> Option<String>,
{
    match lookup("RSK_BACKFILL_FETCH_CONCURRENCY") {
        Some(raw) if !raw.trim().is_empty() => {
            let parsed: usize = raw
                .trim()
                .parse()
                .context("RSK_BACKFILL_FETCH_CONCURRENCY must be a non-negative integer")?;
            Ok(parsed.max(1))
        }
        _ => Ok(crate::chains::rsk::backfill::RSK_DEFAULT_BACKFILL_FETCH_CONCURRENCY),
    }
}

pub(crate) fn hathor_rpc_config() -> Result<crate::chains::hathor::rpc::HathorRpcConfig> {
    hathor_rpc_config_from_lookup(env_lookup)
}

pub(crate) fn hathor_rpc_config_from_lookup<F>(
    lookup: F,
) -> Result<crate::chains::hathor::rpc::HathorRpcConfig>
where
    F: Fn(&str) -> Option<String>,
{
    use crate::chains::hathor::rpc::{DEFAULT_API_URL, DEFAULT_FALLBACK_URL, DEFAULT_MAX_RETRIES};
    let url = lookup("HATHOR_RPC_URL").unwrap_or_else(|| DEFAULT_API_URL.to_owned());
    let fallback_url = match lookup("HATHOR_RPC_FALLBACK_URL") {
        Some(value) if value.trim().is_empty() => None,
        Some(value) => Some(value),
        None => Some(DEFAULT_FALLBACK_URL.to_owned()),
    };
    Ok(crate::chains::hathor::rpc::HathorRpcConfig {
        url,
        fallback_url,
        request_timeout: parse_timeout_secs_from_lookup(
            "HATHOR_RPC_TIMEOUT_SECS",
            DEFAULT_RPC_TIMEOUT_SECS,
            &lookup,
        )?,
        max_retries: DEFAULT_MAX_RETRIES,
    })
}

pub(crate) fn max_backfill_range_from_lookup<F>(spec: &ChainSpec, lookup: F) -> i32
where
    F: Fn(&str) -> Option<String>,
{
    let default_max = spec
        .backfill_range_cap
        .as_ref()
        .expect("max_backfill_range requires a spec range cap")
        .default_max;
    lookup(&format!("{}_MAX_BACKFILL_RANGE", spec.env_prefix))
        .and_then(|value| value.parse().ok())
        .unwrap_or(default_max)
}

pub(crate) fn allow_large_backfill_from_lookup<F>(spec: &ChainSpec, lookup: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    exact_one_from_lookup(&format!("{}_ALLOW_LARGE_BACKFILL", spec.env_prefix), lookup)
}

pub(crate) fn hathor_backfill_skip_holds() -> bool {
    exact_one_from_lookup("HATHOR_BACKFILL_SKIP_HOLDS", env_lookup)
}

pub(crate) fn exact_one_from_lookup<F>(key: &str, lookup: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key).is_some_and(|value| value == "1")
}

pub(crate) fn elastos_rpc_config() -> Result<crate::chains::elastos::rpc::ElastosRpcConfig> {
    elastos_rpc_config_from_lookup(env_lookup)
}

pub(crate) fn elastos_rpc_config_from_lookup<F>(
    lookup: F,
) -> Result<crate::chains::elastos::rpc::ElastosRpcConfig>
where
    F: Fn(&str) -> Option<String>,
{
    use crate::chains::elastos::rpc::DEFAULT_ELASTOS_RPC_URL;
    let (user, password) = match (lookup("ELASTOS_RPC_USER"), lookup("ELASTOS_RPC_PASSWORD")) {
        (Some(user), Some(password)) => (Some(user), Some(password)),
        (None, None) => (None, None),
        _ => bail!("ELASTOS_RPC_USER and ELASTOS_RPC_PASSWORD must be set together"),
    };
    Ok(crate::chains::elastos::rpc::ElastosRpcConfig {
        url: lookup("ELASTOS_RPC_URL").unwrap_or_else(|| DEFAULT_ELASTOS_RPC_URL.to_owned()),
        user,
        password,
        request_timeout: parse_timeout_secs_from_lookup(
            "ELASTOS_RPC_TIMEOUT_SECS",
            DEFAULT_RPC_TIMEOUT_SECS,
            &lookup,
        )?,
    })
}

pub(crate) fn hathor_backfill_delay_ms() -> u64 {
    hathor_backfill_delay_ms_from_lookup(env_lookup)
}

pub(crate) fn hathor_backfill_delay_ms_from_lookup<F>(lookup: F) -> u64
where
    F: Fn(&str) -> Option<String>,
{
    lookup("HATHOR_RPC_BACKFILL_DELAY_MS")
        .and_then(|value| value.parse().ok())
        .unwrap_or(200)
}

pub(crate) fn elastos_backfill_delay_ms() -> u64 {
    elastos_backfill_delay_ms_from_lookup(env_lookup)
}

pub(crate) fn elastos_backfill_delay_ms_from_lookup<F>(lookup: F) -> u64
where
    F: Fn(&str) -> Option<String>,
{
    lookup("ELASTOS_RPC_BACKFILL_DELAY_MS")
        .and_then(|value| value.parse().ok())
        .unwrap_or(crate::chains::elastos::backfill::ELASTOS_DEFAULT_BACKFILL_DELAY_MS)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use super::*;
    use crate::chains::spec::{ChainId, by_id};

    fn lookup_from(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn syscoin_defaults_have_no_start_override_or_trailing_rescan() -> Result<()> {
        let map: HashMap<String, String> = HashMap::new();
        let config =
            poller_config_from_lookup(by_id(ChainId::Syscoin), |key| map.get(key).cloned())?;
        assert_eq!(config.start_height_override, None);
        assert_eq!(config.poll_interval, Duration::from_secs(30));
        assert_eq!(config.batch_size, 100);
        assert_eq!(config.reorg_depth, 0);
        Ok(())
    }

    #[test]
    fn elastos_reorg_depth_defaults_to_zero_and_honours_the_override() -> Result<()> {
        let empty: HashMap<String, String> = HashMap::new();
        let config =
            poller_config_from_lookup(by_id(ChainId::Elastos), |key| empty.get(key).cloned())?;
        assert_eq!(config.reorg_depth, 0);
        assert_eq!(config.start_height_override, None);

        let map = lookup_from(&[("ELASTOS_REORG_DEPTH", "3")]);
        let config =
            poller_config_from_lookup(by_id(ChainId::Elastos), |key| map.get(key).cloned())?;
        assert_eq!(config.reorg_depth, 3);
        Ok(())
    }

    #[test]
    fn rsk_defaults_keep_the_trailing_rescan_window() -> Result<()> {
        let map: HashMap<String, String> = HashMap::new();
        let config = poller_config_from_lookup(by_id(ChainId::Rsk), |key| map.get(key).cloned())?;
        assert_eq!(config.reorg_depth, 64);
        Ok(())
    }

    #[test]
    fn hathor_defaults_keep_the_dag_rescan_window() -> Result<()> {
        let map: HashMap<String, String> = HashMap::new();
        let config =
            poller_config_from_lookup(by_id(ChainId::Hathor), |key| map.get(key).cloned())?;
        assert_eq!(config.reorg_depth, 20);
        Ok(())
    }

    #[test]
    fn namecoin_auth_contract_independent_required_vars_in_url_first_order() {
        let spec = by_id(ChainId::Namecoin);

        // Nothing set: URL is the FIRST required error (historical order).
        let empty: HashMap<String, String> = HashMap::new();
        let err = bitcoind_rpc_config_from_lookup(spec, |key| empty.get(key).cloned()).unwrap_err();
        assert!(err.to_string().contains("NAMECOIN_RPC_URL is required"));

        // URL set, user unset: the exact independent per-var error.
        let map = lookup_from(&[("NAMECOIN_RPC_URL", "http://localhost:8336")]);
        let err = bitcoind_rpc_config_from_lookup(spec, |key| map.get(key).cloned()).unwrap_err();
        assert!(err.to_string().contains("NAMECOIN_RPC_USER is required"));

        // URL+user set, password unset: there is NO set-together error for
        // Namecoin; that phrasing belongs to the optional-pair policy.
        let map = lookup_from(&[
            ("NAMECOIN_RPC_URL", "http://localhost:8336"),
            ("NAMECOIN_RPC_USER", "user"),
        ]);
        let err = bitcoind_rpc_config_from_lookup(spec, |key| map.get(key).cloned()).unwrap_err();
        assert!(
            err.to_string()
                .contains("NAMECOIN_RPC_PASSWORD is required")
        );
        assert!(!err.to_string().contains("must be set together"));

        // Fully configured: resolved values and the 15s default timeout.
        let map = lookup_from(&[
            ("NAMECOIN_RPC_URL", "http://localhost:8336"),
            ("NAMECOIN_RPC_USER", "user"),
            ("NAMECOIN_RPC_PASSWORD", "pass"),
        ]);
        let config = bitcoind_rpc_config_from_lookup(spec, |key| map.get(key).cloned()).unwrap();
        assert_eq!(config.user.as_deref(), Some("user"));
        assert_eq!(config.password.as_deref(), Some("pass"));
        assert_eq!(config.request_timeout, Duration::from_secs(15));
    }

    #[test]
    fn rsk_auth_contract_is_one_sided_tolerant() {
        // Today's contract: user without password (or vice versa) is silently
        // accepted, never a set-together error.
        let map = lookup_from(&[
            ("RSK_RPC_URL", "http://localhost:4444"),
            ("RSK_RPC_USER", "user"),
        ]);
        let config = rsk_rpc_config_from_lookup(|key| map.get(key).cloned()).unwrap();
        assert_eq!(config.user.as_deref(), Some("user"));
        assert_eq!(config.password, None);
        assert_eq!(config.request_timeout, Duration::from_secs(15));

        let empty: HashMap<String, String> = HashMap::new();
        let err = rsk_rpc_config_from_lookup(|key| empty.get(key).cloned()).unwrap_err();
        assert!(err.to_string().contains("RSK_RPC_URL is required"));
    }

    #[test]
    fn rsk_fetch_concurrency_contract_default_clamp_and_malformed_error() {
        let empty: HashMap<String, String> = HashMap::new();
        assert_eq!(
            rsk_backfill_fetch_concurrency_from_lookup(|key| empty.get(key).cloned()).unwrap(),
            16
        );
        // Blank falls back to the default (not an error).
        let map = lookup_from(&[("RSK_BACKFILL_FETCH_CONCURRENCY", "  ")]);
        assert_eq!(
            rsk_backfill_fetch_concurrency_from_lookup(|key| map.get(key).cloned()).unwrap(),
            16
        );
        // Zero clamps to 1 so the pipeline never stalls.
        let map = lookup_from(&[("RSK_BACKFILL_FETCH_CONCURRENCY", "0")]);
        assert_eq!(
            rsk_backfill_fetch_concurrency_from_lookup(|key| map.get(key).cloned()).unwrap(),
            1
        );
        // Non-empty malformed values ERROR (no silent default).
        let map = lookup_from(&[("RSK_BACKFILL_FETCH_CONCURRENCY", "abc")]);
        let err =
            rsk_backfill_fetch_concurrency_from_lookup(|key| map.get(key).cloned()).unwrap_err();
        assert!(
            err.to_string()
                .contains("RSK_BACKFILL_FETCH_CONCURRENCY must be a non-negative integer")
        );
    }

    #[test]
    fn hathor_rpc_config_contract_defaults_and_empty_fallback_disable() {
        use crate::chains::hathor::rpc::{DEFAULT_API_URL, DEFAULT_FALLBACK_URL};

        let empty: HashMap<String, String> = HashMap::new();
        let config = hathor_rpc_config_from_lookup(|key| empty.get(key).cloned()).unwrap();
        assert_eq!(config.url, DEFAULT_API_URL);
        assert_eq!(config.fallback_url.as_deref(), Some(DEFAULT_FALLBACK_URL));

        // An EMPTY fallback value disables the fallback entirely.
        let map = lookup_from(&[("HATHOR_RPC_FALLBACK_URL", "  ")]);
        let config = hathor_rpc_config_from_lookup(|key| map.get(key).cloned()).unwrap();
        assert_eq!(config.fallback_url, None);
    }

    #[test]
    fn hathor_backfill_env_contracts_silent_fallback_and_exact_one_booleans() {
        // Malformed range/delay values SILENTLY fall back to defaults.
        let map = lookup_from(&[
            ("HATHOR_MAX_BACKFILL_RANGE", "abc"),
            ("HATHOR_RPC_BACKFILL_DELAY_MS", "abc"),
        ]);
        assert_eq!(
            max_backfill_range_from_lookup(by_id(ChainId::Hathor), |key| map.get(key).cloned()),
            5_000
        );
        assert_eq!(
            hathor_backfill_delay_ms_from_lookup(|key| map.get(key).cloned()),
            200
        );
        let map = lookup_from(&[("HATHOR_MAX_BACKFILL_RANGE", "9000")]);
        assert_eq!(
            max_backfill_range_from_lookup(by_id(ChainId::Hathor), |key| map.get(key).cloned()),
            9_000
        );

        // Boolean-likes are true for EXACTLY "1" - "true"/"yes" are false.
        for (value, expected) in [("1", true), ("true", false), ("0", false), ("yes", false)] {
            let map = lookup_from(&[("X", value)]);
            assert_eq!(
                exact_one_from_lookup("X", |key| map.get(key).cloned()),
                expected,
                "value {value}"
            );
        }
    }

    #[test]
    fn elastos_auth_contract_is_set_together_with_defaulted_url() {
        use crate::chains::elastos::rpc::DEFAULT_ELASTOS_RPC_URL;

        // Nothing set: defaulted URL, unauthenticated.
        let empty: HashMap<String, String> = HashMap::new();
        let config = elastos_rpc_config_from_lookup(|key| empty.get(key).cloned()).unwrap();
        assert_eq!(config.url, DEFAULT_ELASTOS_RPC_URL);
        assert_eq!(config.user, None);

        // One-sided pair is an ERROR (unlike RSK's one-sided tolerance).
        let map = lookup_from(&[("ELASTOS_RPC_USER", "user")]);
        let err = elastos_rpc_config_from_lookup(|key| map.get(key).cloned()).unwrap_err();
        assert!(
            err.to_string()
                .contains("ELASTOS_RPC_USER and ELASTOS_RPC_PASSWORD must be set together")
        );
    }
}
