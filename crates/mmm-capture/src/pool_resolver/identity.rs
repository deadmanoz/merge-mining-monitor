//! RSK miner-address identity registry (`eth_getBlockByNumber.miner` -> pool slug).

use std::collections::HashMap;

use serde::Deserialize;

use super::btc::PoolSnapshotSource;
use super::error::PoolResolverError;
use crate::identity_registry::{
    IdentityRegistryEntry, IdentityRegistryError, validate_identity_registry,
};

/// Embedded `data/pools/child-identities/rsk_miner_registry.json`.
pub const DEFAULT_RSK_MINER_REGISTRY_JSON: &str =
    include_str!("../../../../data/pools/child-identities/rsk_miner_registry.json");

/// Attribution namespace `"rsk_miner_address"` for RSK `pool_identity` rows.
pub const RSK_MINER_ADDRESS_NAMESPACE: &str = "rsk_miner_address";

fn default_source() -> PoolSnapshotSource {
    PoolSnapshotSource {
        name: String::new(),
        upstream_url: None,
        license: None,
        notes: None,
    }
}

/// Deserialized `data/pools/child-identities/rsk_miner_registry.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct RskMinerRegistry {
    pub schema_version: u32,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default = "default_source")]
    pub source: PoolSnapshotSource,
    pub entries: Vec<RskMinerEntry>,
}

/// One RSK miner address mapped to its canonical pool.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RskMinerEntry {
    pub miner_address: String,
    pub pool_slug: String,
    pub pool_canonical_name: String,
}

/// Borrowed match returned by [`PoolIdentityRegistry::resolve_rsk_miner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolIdentityMatch<'a> {
    pub entry: &'a RskMinerEntry,
}

/// Validated index over [`RskMinerRegistry`] for miner-address pool attribution.
#[derive(Debug)]
pub struct PoolIdentityRegistry {
    rsk: RskMinerRegistry,
    rsk_by_address: HashMap<String, usize>,
}

impl PoolIdentityRegistry {
    pub fn from_default_rsk_registry() -> Result<Self, PoolResolverError> {
        Self::from_rsk_json_str(DEFAULT_RSK_MINER_REGISTRY_JSON)
    }

    pub fn from_rsk_json_str(json: &str) -> Result<Self, PoolResolverError> {
        let registry: RskMinerRegistry = serde_json::from_str(json)
            .map_err(|err| PoolResolverError::InvalidRegistryJson(err.to_string()))?;
        Self::from_rsk_registry(registry)
    }

    pub fn from_rsk_registry(rsk: RskMinerRegistry) -> Result<Self, PoolResolverError> {
        validate_rsk_registry(&rsk)?;
        let mut rsk_by_address = HashMap::new();
        for (index, entry) in rsk.entries.iter().enumerate() {
            rsk_by_address.insert(normalize_rsk_address(&entry.miner_address), index);
        }
        Ok(Self {
            rsk,
            rsk_by_address,
        })
    }

    pub fn rsk_registry(&self) -> &RskMinerRegistry {
        &self.rsk
    }

    /// Look up an RSK miner address (case-insensitive, optional `0x` prefix).
    pub fn resolve_rsk_miner(&self, address: &str) -> Option<PoolIdentityMatch<'_>> {
        let key = normalize_rsk_address(address);
        self.rsk_by_address
            .get(&key)
            .map(|index| PoolIdentityMatch {
                entry: &self.rsk.entries[*index],
            })
    }

    /// Deduplicated `(slug, canonical_name)` pairs referenced by the RSK registry.
    pub fn distinct_pool_definitions(&self) -> Vec<(&str, &str)> {
        crate::identity_registry::distinct_pool_definitions(
            self.rsk.entries.iter().map(rsk_registry_entry),
        )
    }
}

/// Trim, lower-case ASCII, and strip a leading `0x`.
pub fn normalize_rsk_address(address: &str) -> String {
    let lower = address.trim().to_ascii_lowercase();
    lower.strip_prefix("0x").unwrap_or(&lower).to_owned()
}

fn validate_rsk_registry(registry: &RskMinerRegistry) -> Result<(), PoolResolverError> {
    let entries = registry.entries.iter().map(rsk_registry_entry);
    validate_identity_registry(
        registry.schema_version,
        entries,
        "miner_address",
        validate_rsk_miner_address_format,
        normalize_rsk_address,
    )
    .map_err(map_rsk_registry_error)
}

fn rsk_registry_entry(entry: &RskMinerEntry) -> IdentityRegistryEntry<'_> {
    IdentityRegistryEntry {
        identifier: &entry.miner_address,
        pool_slug: &entry.pool_slug,
        pool_canonical_name: &entry.pool_canonical_name,
    }
}

fn validate_rsk_miner_address_format(address: &str) -> Result<(), String> {
    let normalized = normalize_rsk_address(address);
    if normalized.len() == 40 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("expected 40 hex characters (optionally prefixed with 0x)".to_owned())
    }
}

fn map_rsk_registry_error(error: IdentityRegistryError) -> PoolResolverError {
    match error {
        IdentityRegistryError::UnsupportedSchemaVersion(version) => {
            PoolResolverError::UnsupportedSchemaVersion(version)
        }
        IdentityRegistryError::EmptyField { field, pool_slug } => {
            PoolResolverError::EmptyValue { field, pool_slug }
        }
        IdentityRegistryError::WhitespaceField { field, pool_slug } => {
            PoolResolverError::WhitespaceValue { field, pool_slug }
        }
        IdentityRegistryError::DuplicateIdentifier {
            field,
            value,
            first_pool,
            duplicate_pool,
        } => PoolResolverError::DuplicateValue {
            field,
            value,
            first_pool,
            duplicate_pool,
        },
        IdentityRegistryError::SlugCanonicalNameConflict {
            slug,
            first_canonical_name,
            duplicate_canonical_name,
        } => PoolResolverError::SlugCanonicalNameConflict {
            slug,
            first_canonical_name,
            duplicate_canonical_name,
        },
        IdentityRegistryError::InvalidIdentifier {
            value, pool_slug, ..
        } => PoolResolverError::InvalidRskMinerAddress { value, pool_slug },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_default_rsk_miner_registry() {
        let registry = PoolIdentityRegistry::from_default_rsk_registry().unwrap();

        assert_eq!(registry.rsk_registry().schema_version, 1);
        assert!(registry.rsk_registry().entries.len() >= 10);
    }

    #[test]
    fn every_rsk_referenced_slug_is_present_in_btc_snapshot_with_attribution() {
        use crate::pool_resolver::PoolResolver;

        let registry = PoolIdentityRegistry::from_default_rsk_registry().unwrap();
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let btc_pools: std::collections::HashMap<&str, &crate::pool_resolver::PoolRecord> =
            resolver
                .snapshot()
                .pools
                .iter()
                .map(|pool| (pool.slug.as_str(), pool))
                .collect();

        for (slug, _canonical) in registry.distinct_pool_definitions() {
            let pool = btc_pools.get(slug).unwrap_or_else(|| {
                panic!(
                    "RSK miner registry references pool slug {slug:?} that is absent from \
                     data/pools/current.json; a shared pool would lose its BTC attribution"
                )
            });
            assert!(
                !pool.coinbase_tags.is_empty() || !pool.payout_addresses.is_empty(),
                "shared pool {slug:?} is present but has no BTC attribution (empty tags and \
                 addresses); it would resolve BTC captures to unknown"
            );
        }
    }

    #[test]
    fn resolves_rsk_miner_address_case_insensitively_with_optional_prefix() {
        let registry = PoolIdentityRegistry::from_rsk_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "miner_address": "12d3178a62ef1f520944534ed04504609f7307a1",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    }
                ]
            }"#,
        )
        .unwrap();

        let hit = registry
            .resolve_rsk_miner("0x12D3178A62EF1F520944534ED04504609F7307A1")
            .unwrap();
        assert_eq!(hit.entry.pool_slug, "f2pool");

        let upper_prefix_hit = registry
            .resolve_rsk_miner("0X12D3178A62EF1F520944534ED04504609F7307A1")
            .unwrap();
        assert_eq!(upper_prefix_hit.entry.pool_slug, "f2pool");

        assert!(registry.resolve_rsk_miner("deadbeef").is_none());
    }

    #[test]
    fn rejects_duplicate_rsk_miner_addresses() {
        let err = PoolIdentityRegistry::from_rsk_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "miner_address": "12d3178a62ef1f520944534ed04504609f7307a1",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    },
                    {
                        "miner_address": "12D3178A62EF1F520944534ED04504609F7307A1",
                        "pool_slug": "antpool",
                        "pool_canonical_name": "AntPool"
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            PoolResolverError::DuplicateValue {
                field: "miner_address",
                value: "12d3178a62ef1f520944534ed04504609f7307a1".to_owned(),
                first_pool: "f2pool".to_owned(),
                duplicate_pool: "antpool".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_malformed_rsk_miner_address() {
        let err = PoolIdentityRegistry::from_rsk_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "miner_address": "not-hex",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            PoolResolverError::InvalidRskMinerAddress {
                value: "not-hex".to_owned(),
                pool_slug: "f2pool".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_slug_canonical_name_conflict() {
        let err = PoolIdentityRegistry::from_rsk_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "miner_address": "12d3178a62ef1f520944534ed04504609f7307a1",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    },
                    {
                        "miner_address": "4e5dabc28e4a0f5e5b19fcb56b28c5a1989352c1",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "Discus Fish"
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            PoolResolverError::SlugCanonicalNameConflict {
                slug: "f2pool".to_owned(),
                first_canonical_name: "F2Pool".to_owned(),
                duplicate_canonical_name: "Discus Fish".to_owned(),
            }
        );
    }
}
