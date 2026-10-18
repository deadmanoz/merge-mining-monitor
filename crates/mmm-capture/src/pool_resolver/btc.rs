//! BTC-coinbase pool resolution against `data/pools/current.json`.

use std::collections::HashMap;

use serde::Deserialize;

use super::error::PoolResolverError;

/// Embedded `data/pools/current.json` BTC pool snapshot.
pub const DEFAULT_POOL_SNAPSHOT_JSON: &str = include_str!("../../../../data/pools/current.json");

#[derive(Debug, Clone, Deserialize)]
pub struct PoolSnapshot {
    pub schema_version: u32,
    pub generated_at: String,
    pub scope: String,
    pub source: PoolSnapshotSource,
    pub pools: Vec<PoolRecord>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolSnapshotSource {
    pub name: String,
    pub upstream_url: Option<String>,
    pub license: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PoolRecord {
    /// Stable DB primary-key string (`pool.slug`).
    pub slug: String,
    pub source_id: Option<u64>,
    pub canonical_name: String,
    /// Raw byte substrings matched longest-first against a BTC coinbase script.
    pub coinbase_tags: Vec<String>,
    pub payout_addresses: Vec<String>,
    pub link: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    CoinbaseTag,
    PayoutAddress,
}

#[derive(Debug, Clone, Copy)]
pub struct PoolMatch<'a> {
    pub pool: &'a PoolRecord,
    pub matched_by: MatchKind,
    pub matched_value: &'a str,
}

/// Validated BTC coinbase-tag and payout-address index over a [`PoolSnapshot`].
/// Coinbase tags are pre-sorted longest-first (then lexically) so the most
/// specific tag wins substring matching.
#[derive(Debug)]
pub struct PoolResolver {
    snapshot: PoolSnapshot,
    coinbase_tags: Vec<TagIndexEntry>,
    payout_addresses: HashMap<String, usize>,
}

#[derive(Debug)]
struct TagIndexEntry {
    tag: String,
    pool_index: usize,
}

impl PoolResolver {
    pub fn from_default_snapshot() -> Result<Self, PoolResolverError> {
        Self::from_json_str(DEFAULT_POOL_SNAPSHOT_JSON)
    }

    pub fn from_json_str(json: &str) -> Result<Self, PoolResolverError> {
        let snapshot = serde_json::from_str(json)
            .map_err(|err| PoolResolverError::InvalidSnapshotJson(err.to_string()))?;
        Self::from_snapshot(snapshot)
    }

    pub fn from_snapshot(snapshot: PoolSnapshot) -> Result<Self, PoolResolverError> {
        validate_snapshot(&snapshot)?;

        let mut coinbase_tags = Vec::new();
        let mut payout_addresses = HashMap::new();

        for (pool_index, pool) in snapshot.pools.iter().enumerate() {
            for tag in &pool.coinbase_tags {
                coinbase_tags.push(TagIndexEntry {
                    tag: tag.clone(),
                    pool_index,
                });
            }

            for address in &pool.payout_addresses {
                payout_addresses.insert(address.trim().to_owned(), pool_index);
            }
        }

        coinbase_tags.sort_by(|left, right| {
            right
                .tag
                .len()
                .cmp(&left.tag.len())
                .then_with(|| left.tag.cmp(&right.tag))
        });

        Ok(Self {
            snapshot,
            coinbase_tags,
            payout_addresses,
        })
    }

    pub fn snapshot(&self) -> &PoolSnapshot {
        &self.snapshot
    }

    /// Match raw coinbase scriptSig bytes against pool tags, longest tag first.
    pub fn resolve_coinbase_script(&self, coinbase_script: &[u8]) -> Option<PoolMatch<'_>> {
        self.coinbase_tags.iter().find_map(|entry| {
            let needle = entry.tag.as_bytes();
            if contains_bytes(coinbase_script, needle) {
                Some(PoolMatch {
                    pool: &self.snapshot.pools[entry.pool_index],
                    matched_by: MatchKind::CoinbaseTag,
                    matched_value: &entry.tag,
                })
            } else {
                None
            }
        })
    }

    pub fn resolve_payout_address(&self, address: &str) -> Option<PoolMatch<'_>> {
        let address = address.trim();
        self.payout_addresses
            .get_key_value(address)
            .map(|(matched_address, pool_index)| PoolMatch {
                pool: &self.snapshot.pools[*pool_index],
                matched_by: MatchKind::PayoutAddress,
                matched_value: matched_address,
            })
    }

    pub fn resolve_payout_addresses<'a, I>(&self, addresses: I) -> Option<PoolMatch<'_>>
    where
        I: IntoIterator<Item = &'a str>,
    {
        addresses
            .into_iter()
            .find_map(|address| self.resolve_payout_address(address))
    }
}

fn validate_snapshot(snapshot: &PoolSnapshot) -> Result<(), PoolResolverError> {
    if snapshot.schema_version != 1 {
        return Err(PoolResolverError::UnsupportedSchemaVersion(
            snapshot.schema_version,
        ));
    }

    let mut slug_owners = HashMap::new();
    let mut tag_owners = HashMap::new();
    let mut address_owners = HashMap::new();

    for pool in &snapshot.pools {
        validate_non_empty("slug", &pool.slug, &pool.slug)?;
        validate_non_empty("canonical_name", &pool.canonical_name, &pool.slug)?;

        if let Some(first_canonical) =
            slug_owners.insert(pool.slug.clone(), pool.canonical_name.clone())
        {
            return Err(PoolResolverError::DuplicateValue {
                field: "slug",
                value: pool.slug.clone(),
                first_pool: first_canonical,
                duplicate_pool: pool.canonical_name.clone(),
            });
        }

        for tag in &pool.coinbase_tags {
            validate_non_empty("coinbase_tags", tag, &pool.slug)?;
            if let Some(first_pool) = tag_owners.insert(tag.clone(), pool.slug.clone()) {
                return Err(PoolResolverError::DuplicateValue {
                    field: "coinbase_tags",
                    value: tag.clone(),
                    first_pool,
                    duplicate_pool: pool.slug.clone(),
                });
            }
        }

        for address in &pool.payout_addresses {
            validate_non_empty("payout_addresses", address, &pool.slug)?;
            let trimmed = address.trim().to_owned();
            if let Some(first_pool) = address_owners.insert(trimmed.clone(), pool.slug.clone()) {
                return Err(PoolResolverError::DuplicateValue {
                    field: "payout_addresses",
                    value: trimmed,
                    first_pool,
                    duplicate_pool: pool.slug.clone(),
                });
            }
        }
    }

    Ok(())
}

pub(super) fn validate_non_empty(
    field: &'static str,
    value: &str,
    pool_slug: &str,
) -> Result<(), PoolResolverError> {
    if value.trim().is_empty() {
        Err(PoolResolverError::EmptyValue {
            field,
            pool_slug: pool_slug.to_owned(),
        })
    } else {
        Ok(())
    }
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|candidate| candidate == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_default_snapshot() {
        // Construction runs `validate_snapshot`, so this is the hard regen gate:
        // a duplicate slug/tag/address in the embedded expanded snapshot would
        // fail here (a `DuplicateValue`) in CI before it could crash producers.
        let resolver = PoolResolver::from_default_snapshot().unwrap();

        assert_eq!(resolver.snapshot().schema_version, 1);
        // The embedded registry carries the broad public pool set, not only the
        // original small seed set.
        assert!(
            resolver.snapshot().pools.len() >= 100,
            "expected the expanded snapshot, got {} pools",
            resolver.snapshot().pools.len()
        );
    }

    #[test]
    fn preserves_stable_slugs_after_registry_refresh() {
        // These initial slugs are stable DB keys; registry refreshes must keep
        // every one so existing `pool.id`s and their FKs survive.
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let slugs: std::collections::HashSet<&str> = resolver
            .snapshot()
            .pools
            .iter()
            .map(|pool| pool.slug.as_str())
            .collect();
        for slug in [
            "foundry-usa",
            "antpool",
            "viabtc",
            "f2pool",
            "binance-pool",
            "braiins-pool",
            "luxor",
            "mara-pool",
            "ocean-xyz",
        ] {
            assert!(slugs.contains(slug), "stable slug {slug} was dropped");
        }
    }

    #[test]
    fn longest_tag_first_disambiguates_real_overlapping_snapshot_tags() {
        // The expanded 162-pool registry contains real nested coinbase tags
        // where one pool's tag is a substring of another's. The substring
        // matcher alone would mis-resolve these; the longest-tag-first sort is
        // what makes the more specific pool win. Pin that property against the
        // embedded snapshot so a future change to the index ordering cannot
        // silently reintroduce misattribution.
        let resolver = PoolResolver::from_default_snapshot().unwrap();

        // `ckpool` is a substring of solo-ck's `/solo.ckpool.org/`.
        let solo = resolver
            .resolve_coinbase_script(b"\x00/solo.ckpool.org/\x00")
            .expect("solo.ckpool.org coinbase should resolve");
        assert_eq!(solo.pool.slug, "solo-ck");
        assert_eq!(solo.matched_value, "/solo.ckpool.org/");

        // `EMC` (eclipsemc) is a substring of emcdpool's `/EMCD/`.
        let emcd = resolver
            .resolve_coinbase_script(b"\x00/EMCD/\x00")
            .expect("/EMCD/ coinbase should resolve");
        assert_eq!(emcd.pool.slug, "emcdpool");
        assert_eq!(emcd.matched_value, "/EMCD/");

        // The shorter substring still resolves to its own pool when the longer
        // tag is absent, confirming the sort does not break the bare match.
        let bare_ck = resolver
            .resolve_coinbase_script(b"\x00ckpool\x00")
            .expect("bare ckpool coinbase should resolve");
        assert_eq!(bare_ck.pool.slug, "ckpool");
    }

    #[test]
    fn resolves_coinbase_script_by_tag_substring() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let script = b"\x03\x01\x02\x03/Foundry USA Pool #dropgold/\x00\x00";

        let resolved = resolver.resolve_coinbase_script(script).unwrap();

        assert_eq!(resolved.pool.slug, "foundry-usa");
        assert_eq!(resolved.matched_by, MatchKind::CoinbaseTag);
        assert_eq!(resolved.matched_value, "Foundry USA Pool");
    }

    #[test]
    fn resolves_payout_address_by_exact_match() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();

        let resolved = resolver
            .resolve_payout_address(" 1PuJjnF476W3zXfVYmJfGnouzFDAXakkL4 ")
            .unwrap();

        assert_eq!(resolved.pool.slug, "viabtc");
        assert_eq!(resolved.matched_by, MatchKind::PayoutAddress);
        assert_eq!(resolved.matched_value, "1PuJjnF476W3zXfVYmJfGnouzFDAXakkL4");
    }

    #[test]
    fn resolves_first_matching_payout_address() {
        let resolver = PoolResolver::from_default_snapshot().unwrap();

        let resolved = resolver
            .resolve_payout_addresses([
                "bc1qunknownunknownunknownunknownunknownunknown",
                "12dRugNcdxK39288NjcDV4GX7rMsKCGn6B",
            ])
            .unwrap();

        assert_eq!(resolved.pool.slug, "antpool");
    }

    #[test]
    fn prefers_longer_coinbase_tags() {
        let resolver = PoolResolver::from_json_str(
            r#"{
                "schema_version": 1,
                "generated_at": "2026-05-25",
                "scope": "test",
                "source": { "name": "test" },
                "pools": [
                    {
                        "slug": "parent",
                        "canonical_name": "Parent",
                        "coinbase_tags": ["/ViaBTC/"],
                        "payout_addresses": []
                    },
                    {
                        "slug": "child",
                        "canonical_name": "Child",
                        "coinbase_tags": ["/ViaBTC/TATMAS Pool/"],
                        "payout_addresses": []
                    }
                ]
            }"#,
        )
        .unwrap();

        let resolved = resolver
            .resolve_coinbase_script(b"\x00/ViaBTC/TATMAS Pool/\x00")
            .unwrap();

        assert_eq!(resolved.pool.slug, "child");
        assert_eq!(resolved.matched_value, "/ViaBTC/TATMAS Pool/");
    }

    #[test]
    fn rejects_duplicate_coinbase_tags() {
        let err = PoolResolver::from_json_str(
            r#"{
                "schema_version": 1,
                "generated_at": "2026-05-25",
                "scope": "test",
                "source": { "name": "test" },
                "pools": [
                    {
                        "slug": "one",
                        "canonical_name": "One",
                        "coinbase_tags": ["same"],
                        "payout_addresses": []
                    },
                    {
                        "slug": "two",
                        "canonical_name": "Two",
                        "coinbase_tags": ["same"],
                        "payout_addresses": []
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            PoolResolverError::DuplicateValue {
                field: "coinbase_tags",
                value: "same".to_owned(),
                first_pool: "one".to_owned(),
                duplicate_pool: "two".to_owned()
            }
        );
    }

    #[test]
    fn rejects_duplicate_payout_addresses() {
        let err = PoolResolver::from_json_str(
            r#"{
                "schema_version": 1,
                "generated_at": "2026-05-25",
                "scope": "test",
                "source": { "name": "test" },
                "pools": [
                    {
                        "slug": "one",
                        "canonical_name": "One",
                        "coinbase_tags": [],
                        "payout_addresses": ["bc1qshared"]
                    },
                    {
                        "slug": "two",
                        "canonical_name": "Two",
                        "coinbase_tags": [],
                        "payout_addresses": ["bc1qshared"]
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            PoolResolverError::DuplicateValue {
                field: "payout_addresses",
                value: "bc1qshared".to_owned(),
                first_pool: "one".to_owned(),
                duplicate_pool: "two".to_owned()
            }
        );
    }
}
