//! Bitcoin Core block-level coinbase pool resolution.

use std::collections::HashMap;

use tracing::warn;

use crate::capture::resolve_parent_pool_attribution_from_serialized_coinbase_outputs;
use crate::pool_resolver::PoolResolver;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CoreCoinbasePoolResolution {
    pub pool_id: Option<i64>,
    pub corrupt_outputs: bool,
}

/// Resolve a Bitcoin Core coinbase to `pool.id` (scriptSig tag first, then payout-address
/// fallback over `serialized_outputs`); corrupt output blobs set `corrupt_outputs = true` without failing.
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
