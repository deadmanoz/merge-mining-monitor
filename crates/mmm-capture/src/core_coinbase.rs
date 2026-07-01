//! Bitcoin Core coinbase evidence helpers.
//!
//! Core block coinbase evidence is block-level context, not AuxPoW event
//! evidence. This module keeps extraction and pool resolution shared between
//! Core write paths and the bounded enrichment command.

use std::collections::HashMap;

use tracing::warn;

use crate::capture::resolve_parent_pool_attribution_from_serialized_coinbase_outputs;
use crate::pool_resolver::PoolResolver;

/// Outcome of resolving a Bitcoin Core coinbase to a pool.
///
/// `pool_id` is the matched `pool.id`, `None` when neither the scriptSig tag nor
/// any payout address resolves. `corrupt_outputs` is best-effort: it is set true
/// only when the serialized outputs blob failed to deserialize, so the address
/// fallback could not run. Pool resolution is never aborted on corrupt outputs;
/// the tag-path result (or `None`) still stands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreCoinbasePoolResolution {
    pub pool_id: Option<i64>,
    pub corrupt_outputs: bool,
}

/// Resolve a Bitcoin Core coinbase to a `pool.id`, tag-first then
/// payout-address fallback.
///
/// Precedence: try the coinbase scriptSig pool tag
/// (`PoolResolver::resolve_coinbase_script`); on a hit that maps to a known slug
/// in `pool_ids_by_slug`, return that id. Otherwise consensus-decode
/// `serialized_outputs` (the coinbase tx outputs blob) into `TxOut`s, extract
/// payout addresses, and try `PoolResolver::resolve_payout_addresses`. A slug
/// hit with no entry in `pool_ids_by_slug` yields no id (pools not yet persisted
/// are not invented here).
///
/// Best-effort on corruption: a decode failure of the outputs blob sets
/// `corrupt_outputs = true` and returns no id rather than erroring, so one bad
/// row never aborts a batch. `serialized_outputs == None` skips the fallback
/// silently.
pub fn resolve_btc_pool_from_coinbase(
    script: &[u8],
    serialized_outputs: Option<&[u8]>,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> CoreCoinbasePoolResolution {
    match resolve_parent_pool_attribution_from_serialized_coinbase_outputs(
        script,
        serialized_outputs,
        resolver,
        pool_ids_by_slug,
    ) {
        Ok(attribution) => CoreCoinbasePoolResolution {
            pool_id: attribution.and_then(|attribution| attribution.pool_id),
            corrupt_outputs: false,
        },
        Err(err) => {
            warn!(
                error = %err,
                "skipping corrupt Core coinbase outputs for address fallback"
            );
            CoreCoinbasePoolResolution {
                pool_id: None,
                corrupt_outputs: true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_coinbase_tag_before_outputs() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let mut pool_ids = HashMap::new();
        pool_ids.insert("kncminer".to_owned(), 42);

        let resolution = resolve_btc_pool_from_coinbase(b"/KnCMiner/", None, &resolver, &pool_ids);

        assert_eq!(resolution.pool_id, Some(42));
        assert!(!resolution.corrupt_outputs);
    }

    #[test]
    fn corrupt_outputs_are_best_effort() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let pool_ids = HashMap::new();

        let resolution = resolve_btc_pool_from_coinbase(
            b"/unknown/",
            Some(b"not outputs"),
            &resolver,
            &pool_ids,
        );

        assert_eq!(resolution.pool_id, None);
        assert!(resolution.corrupt_outputs);
    }
}
