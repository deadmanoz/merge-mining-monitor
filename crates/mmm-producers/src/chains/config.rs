//! Spec-driven environment configuration for chain producers.
//!
//! This module is the ONLY place code under `src/chains/` reads process env,
//! directly or indirectly. Shared implementations receive resolved values;
//! `main.rs` builds non-chain runtime (DB, classifier) through the
//! `producer_runtime` module, which owns `PG*` / `BITCOIN_RPC_*`.

use std::env;
use std::fs;

use anyhow::{Context, Result, bail, ensure};

use crate::chains::bitcoind_rpc::BitcoindRpcConfig;
use crate::chains::spec::{ChainSpec, ReorgPolicy, RpcAuth};
use crate::poller::PollerConfig;
use mmm_rpc as rpc_http;
use rpc_http::{DEFAULT_RPC_TIMEOUT_SECS, parse_timeout_secs_from_lookup};

/// The process-env lookup, THE single point where `src/chains/` touches
/// `std::env`. Every other module under `chains/` receives resolved values or
/// passes this function (or a test lookup) down.
pub(crate) fn env_lookup(key: &str) -> Option<String> {
    env::var(key).ok()
}

/// Build the live-poll configuration for a chain from the process
/// environment: `<PREFIX>_START_HEIGHT`, `<PREFIX>_POLL_INTERVAL_SECONDS`,
/// `<PREFIX>_BATCH_SIZE`, and (policy permitting) `<PREFIX>_REORG_DEPTH`,
/// falling back to the spec row's defaults.
pub(crate) fn poller_config(spec: &ChainSpec) -> Result<PollerConfig> {
    poller_config_from_lookup(spec, env_lookup)
}

/// Pure variant of [`poller_config`] driven by an arbitrary lookup, so unit
/// tests never mutate the global (and in Rust 2024 `unsafe`) process
/// environment.
///
/// `ReorgPolicy::ForbiddenMonotonic` rejects ANY present
/// `<PREFIX>_REORG_DEPTH` (any value), then forces `reorg_depth = 0` (not read
/// from the generic per-prefix lookup) - the exact semantics the Elastos
/// poller shipped with.
pub(crate) fn poller_config_from_lookup<F>(spec: &ChainSpec, lookup: F) -> Result<PollerConfig>
where
    F: Fn(&str) -> Option<String>,
{
    match spec.reorg_policy {
        ReorgPolicy::EnvConfigurable => {
            PollerConfig::from_lookup(spec.env_prefix, spec.poller, lookup)
        }
        ReorgPolicy::ForbiddenMonotonic => {
            ensure!(
                lookup(&format!("{}_REORG_DEPTH", spec.env_prefix)).is_none(),
                "{}_REORG_DEPTH is not supported in this slice: {} is monotonic, and a \
                 trailing rescan needs same-height reconciliation (a deferred follow-up)",
                spec.env_prefix,
                spec.display_name,
            );
            let mut config = PollerConfig::from_lookup(spec.env_prefix, spec.poller, lookup)?;
            config.reorg_depth = 0;
            Ok(config)
        }
    }
}

/// Build the bitcoind-family RPC transport config for a chain from the
/// process environment, honoring the spec row's `RpcAuth` policy. Each
/// chain's CURRENT auth contract is preserved verbatim (error strings
/// included); see the contract tests below.
pub(crate) fn bitcoind_rpc_config(spec: &ChainSpec) -> Result<BitcoindRpcConfig> {
    bitcoind_rpc_config_from_lookup(spec, env_lookup)
}

/// Pure variant of [`bitcoind_rpc_config`] driven by an arbitrary lookup.
/// The cookie FILE read stays real (`fs::read_to_string`); only env access is
/// injected.
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

    // Evaluation ORDER is part of each preserved contract: RequiredUserPass
    // (Namecoin) historically resolved URL, then USER, then PASSWORD, each
    // with its own `<VAR> is required` error; OptionalUserPassOrCookie
    // (Syscoin/Fractal) resolved the auth pair FIRST (a one-sided pair bails
    // before the URL check), then URL.
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
        request_timeout: parse_timeout_secs_from_lookup(
            &format!("{prefix}_RPC_TIMEOUT_SECS"),
            DEFAULT_RPC_TIMEOUT_SECS,
            &lookup,
        )?,
    })
}

/// Build the RSK JSON-RPC transport config from env. `RSK_RPC_URL` is
/// required; HTTP basic auth via `RSK_RPC_USER` / `RSK_RPC_PASSWORD` is
/// optional AND one-sided-tolerant (RSKj's default configuration exposes the
/// JSON-RPC port unauthenticated on loopback, but ssh-tunnelled deployments
/// often layer basic auth on top). The one-sided tolerance is today's
/// contract, pinned by test.
pub(crate) fn rsk_rpc_config() -> Result<crate::chains::rsk::rpc::RskRpcConfig> {
    rsk_rpc_config_from_lookup(env_lookup)
}

/// Pure variant of [`rsk_rpc_config`] driven by an arbitrary lookup so unit
/// tests never touch the global process env.
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

/// Resolve the RSK backfill prefetch concurrency from
/// `RSK_BACKFILL_FETCH_CONCURRENCY`, defaulting to
/// `RSK_DEFAULT_BACKFILL_FETCH_CONCURRENCY` and clamping to `>= 1` so a `0`
/// or empty value never stalls the pipeline. Contract (pinned by test):
/// unset or blank falls back to the default; a non-empty malformed value is
/// an ERROR, not a silent default.
pub(crate) fn rsk_backfill_fetch_concurrency() -> Result<usize> {
    rsk_backfill_fetch_concurrency_from_lookup(env_lookup)
}

/// Pure variant of [`rsk_backfill_fetch_concurrency`]; carries the
/// default/clamp/malformed-error contract documented on the wrapper.
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

/// Build the Hathor REST client config from env. `HATHOR_RPC_URL` defaults
/// to the public node1 API; `HATHOR_RPC_FALLBACK_URL` defaults to node2 and
/// an EMPTY value disables the fallback (today's contract, pinned by test).
pub(crate) fn hathor_rpc_config() -> Result<crate::chains::hathor::rpc::HathorRpcConfig> {
    hathor_rpc_config_from_lookup(env_lookup)
}

/// Pure variant of [`hathor_rpc_config`]; carries the URL-defaulting and
/// empty-disables-fallback contract documented on the wrapper.
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

/// `<PREFIX>_MAX_BACKFILL_RANGE` for chains with a spec range cap: malformed
/// or unset values SILENTLY fall back to the spec default (today's contract;
/// unlike RSK_BACKFILL_FETCH_CONCURRENCY, which errors on malformed input).
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

/// `<PREFIX>_ALLOW_LARGE_BACKFILL`: exact-`"1"` boolean.
pub(crate) fn allow_large_backfill_from_lookup<F>(spec: &ChainSpec, lookup: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    exact_one_from_lookup(&format!("{}_ALLOW_LARGE_BACKFILL", spec.env_prefix), lookup)
}

/// Exact-`"1"` boolean (any other value, including `true`, is false).
pub(crate) fn hathor_backfill_skip_holds() -> bool {
    exact_one_from_lookup("HATHOR_BACKFILL_SKIP_HOLDS", env_lookup)
}

/// The exact-`"1"` boolean policy shared by the backfill override flags.
pub(crate) fn exact_one_from_lookup<F>(key: &str, lookup: F) -> bool
where
    F: Fn(&str) -> Option<String>,
{
    lookup(key).is_some_and(|value| value == "1")
}

/// Build the Elastos JSON-RPC 2.0 transport config from env.
/// `ELASTOS_RPC_URL` defaults to the self-hosted node; auth is an optional
/// SET-TOGETHER pair (one without the other is an error - today's contract,
/// pinned by test, deliberately different from RSK's one-sided tolerance).
pub(crate) fn elastos_rpc_config() -> Result<crate::chains::elastos::rpc::ElastosRpcConfig> {
    elastos_rpc_config_from_lookup(env_lookup)
}

/// Pure variant of [`elastos_rpc_config`]; carries the defaulted-URL and
/// set-together auth-pair contract documented on the wrapper.
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

/// `HATHOR_RPC_BACKFILL_DELAY_MS`: malformed or unset silently falls back to
/// 200ms (today's contract, pinned by test).
pub(crate) fn hathor_backfill_delay_ms() -> u64 {
    hathor_backfill_delay_ms_from_lookup(env_lookup)
}

/// Pure variant of [`hathor_backfill_delay_ms`]; malformed or unset silently
/// falls back to 200ms.
pub(crate) fn hathor_backfill_delay_ms_from_lookup<F>(lookup: F) -> u64
where
    F: Fn(&str) -> Option<String>,
{
    lookup("HATHOR_RPC_BACKFILL_DELAY_MS")
        .and_then(|value| value.parse().ok())
        .unwrap_or(200)
}

/// `ELASTOS_RPC_BACKFILL_DELAY_MS`: malformed or unset silently falls back.
pub(crate) fn elastos_backfill_delay_ms() -> u64 {
    elastos_backfill_delay_ms_from_lookup(env_lookup)
}

/// Pure variant of [`elastos_backfill_delay_ms`]; malformed or unset silently
/// falls back to `ELASTOS_DEFAULT_BACKFILL_DELAY_MS`.
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
    fn elastos_from_lookup_forces_zero_reorg_and_rejects_override() -> Result<()> {
        // Unset: reorg_depth is forced to 0 (monotonic), no start override.
        let empty: HashMap<String, String> = HashMap::new();
        let config =
            poller_config_from_lookup(by_id(ChainId::Elastos), |key| empty.get(key).cloned())?;
        assert_eq!(config.reorg_depth, 0);
        assert_eq!(config.start_height_override, None);

        // ANY present ELASTOS_REORG_DEPTH (even 0) is rejected.
        for value in ["1", "0", "64"] {
            let map = lookup_from(&[("ELASTOS_REORG_DEPTH", value)]);
            let err =
                poller_config_from_lookup(by_id(ChainId::Elastos), |key| map.get(key).cloned())
                    .unwrap_err();
            assert!(
                err.to_string().contains("ELASTOS_REORG_DEPTH"),
                "unexpected error for value {value}: {err}"
            );
        }
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
    fn namecoin_is_the_family_reference_chain() {
        let spec = by_id(ChainId::Namecoin);
        let family = spec.family.expect("namecoin is bitcoind-family");
        assert_eq!(family.label, "Namecoin");
        assert!(family.floor_warning.is_none());
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
