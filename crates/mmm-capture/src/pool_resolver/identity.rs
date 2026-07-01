//! RSK miner-address identity registry.
//!
//! The RSK structure/capture slice attributes pools by miner address (the
//! 20-byte Ethereum-style address returned by eth_getBlockByNumber.miner),
//! not by BTC coinbase tags or BTC payout addresses. RskMinerRegistry is a
//! repo-local, reviewable embedded JSON file that maps such addresses to
//! canonical pool slugs. Validation rejects duplicate addresses and slug /
//! canonical-name disagreements before any DB write so the embedded fixture
//! cannot contradict the rsk_miner_address namespace UNIQUE constraint on
//! pool_identity.

use std::collections::HashMap;

use serde::Deserialize;

use super::btc::PoolSnapshotSource;
use super::error::PoolResolverError;
use crate::identity_registry::{
    IdentityRegistryEntry, IdentityRegistryError, validate_identity_registry,
};

/// The embedded `data/pools/child-identities/rsk_miner_registry.json`, `include_str!`-pulled at
/// compile time. A repo-local, reviewable mapping of RSK miner addresses to
/// canonical pool slugs, edited deliberately under review (not generator output
/// like current.json). [`PoolIdentityRegistry::from_default_rsk_registry`] parses
/// and validates it.
pub const DEFAULT_RSK_MINER_REGISTRY_JSON: &str =
    include_str!("../../../../data/pools/child-identities/rsk_miner_registry.json");

/// Attribution namespace string `"rsk_miner_address"`: the first half of the
/// `(namespace, matched_value)` key for RSK pool_identity rows. Shared constant so
/// mmm-store inserts and mmm-producers replay/capture all key against the exact
/// same namespace the DB UNIQUE constraint expects; never inline this literal.
pub const RSK_MINER_ADDRESS_NAMESPACE: &str = "rsk_miner_address";

/// Deserialized form of `data/pools/child-identities/rsk_miner_registry.json`: schema version (must
/// be 1), shared [`PoolSnapshotSource`] provenance, and the flat `entries` list.
/// `Deserialize`, but also constructed directly in mmm-producers/test fixtures via
/// [`PoolIdentityRegistry::from_rsk_registry`].
#[derive(Debug, Clone, Deserialize)]
pub struct RskMinerRegistry {
    /// Must be 1 (else `UnsupportedSchemaVersion`).
    pub schema_version: u32,
    /// Generator/edit provenance: when the registry was produced.
    pub generated_at: String,
    /// Generator/edit provenance: which set the registry covers.
    pub scope: String,
    /// Provenance block, reusing [`PoolSnapshotSource`].
    pub source: PoolSnapshotSource,
    /// The address -> pool mapping. Pub so producers and tests can build the
    /// registry in memory without going through JSON.
    pub entries: Vec<RskMinerEntry>,
}

/// One RSK registry row: a miner address mapped to its pool. The `pool_slug` must
/// exist in the BTC snapshot with real attribution, enforced by a conformance test.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct RskMinerEntry {
    /// The raw registry address string (40 hex chars, optional `0x`); normalized
    /// to lower-case with `0x` stripped at index and lookup time.
    pub miner_address: String,
    /// The stable DB pool key this address attributes to.
    pub pool_slug: String,
    /// Display name; must be consistent for a given slug (`SlugCanonicalNameConflict`
    /// otherwise).
    pub pool_canonical_name: String,
}

/// A successful RSK miner-address resolution: borrows the matched [`RskMinerEntry`].
/// Returned by [`PoolIdentityRegistry::resolve_rsk_miner`]; the consumer keys a
/// pool_identity row under ([`RSK_MINER_ADDRESS_NAMESPACE`], normalized address)
/// from `entry.pool_slug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PoolIdentityMatch<'a> {
    /// The borrowed registry row that matched the looked-up miner address.
    pub entry: &'a RskMinerEntry,
}

/// Index over an [`RskMinerRegistry`] for miner-address pool attribution.
/// Construction validates the registry (schema == 1, non-empty fields, 40-hex
/// addresses, unique addresses, consistent slug -> canonical_name) and maps each
/// normalized address to its entry. Pure, no I/O.
#[derive(Debug)]
pub struct PoolIdentityRegistry {
    rsk: RskMinerRegistry,
    rsk_by_address: HashMap<String, usize>,
}

impl PoolIdentityRegistry {
    /// Build the registry from the compile-time-embedded
    /// [`DEFAULT_RSK_MINER_REGISTRY_JSON`]. Production entry point. Errors only if
    /// the embedded JSON is malformed or invalid (guarded by a CI test), so prod
    /// startup `?`s it.
    pub fn from_default_rsk_registry() -> Result<Self, PoolResolverError> {
        Self::from_rsk_json_str(DEFAULT_RSK_MINER_REGISTRY_JSON)
    }

    /// Parse an RSK registry from a JSON string, then validate and index. Used by
    /// the tests here; parse failures map to `InvalidRegistryJson`. Distinct from
    /// [`from_default_rsk_registry`](Self::from_default_rsk_registry) (arbitrary
    /// JSON vs the embedded file).
    pub fn from_rsk_json_str(json: &str) -> Result<Self, PoolResolverError> {
        let registry: RskMinerRegistry = serde_json::from_str(json)
            .map_err(|err| PoolResolverError::InvalidRegistryJson(err.to_string()))?;
        Self::from_rsk_registry(registry)
    }

    /// Validate an already-parsed [`RskMinerRegistry`] and build the address index:
    /// normalize each miner address (trim, lower-case, strip `0x`) into the lookup
    /// map. Rejects schema != 1, empty fields, non-40-hex addresses, duplicate
    /// addresses, and inconsistent slug -> canonical_name.
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

    /// Borrow the validated underlying RSK registry (e.g. to read `schema_version`
    /// or iterate entries). Used by mmm-store and producer replay paths to
    /// enumerate addresses.
    pub fn rsk_registry(&self) -> &RskMinerRegistry {
        &self.rsk
    }

    /// Look up an RSK miner address. Accepts hex with or without the `0x`
    /// prefix; comparison is case-insensitive after normalising to lower-case.
    pub fn resolve_rsk_miner(&self, address: &str) -> Option<PoolIdentityMatch<'_>> {
        let key = normalize_rsk_address(address);
        self.rsk_by_address
            .get(&key)
            .map(|index| PoolIdentityMatch {
                entry: &self.rsk.entries[*index],
            })
    }

    /// Iterator over the (slug, canonical_name) pairs the RSK registry
    /// references, deduplicated via the shared
    /// [`crate::identity_registry::distinct_pool_definitions`]. Startup uses this
    /// to ensure RSK-only pool rows exist before pool_identity inserts land.
    pub fn distinct_pool_definitions(&self) -> Vec<(&str, &str)> {
        crate::identity_registry::distinct_pool_definitions(
            self.rsk.entries.iter().map(rsk_registry_entry),
        )
    }
}

/// Canonicalize an RSK/Ethereum-style miner address for keying and comparison:
/// trim, lower-case ASCII, strip a leading `0x`. This function defines
/// the `rsk_miner_address` key form; mmm-store and mmm-producers must call this
/// (not ad-hoc lower-casing) so DB writes and in-memory lookups agree
/// byte-for-byte.
pub fn normalize_rsk_address(address: &str) -> String {
    let lower = address.trim().to_ascii_lowercase();
    lower.strip_prefix("0x").unwrap_or(&lower).to_owned()
}

/// Reject an RSK registry before indexing/persisting via the shared
/// [`validate_identity_registry`] validator: schema must be 1; miner_address,
/// pool_slug, pool_canonical_name non-empty and whitespace-free; each address
/// exactly 40 hex chars after normalization; addresses globally unique under
/// [`normalize_rsk_address`]; and a given pool_slug never mapped to two
/// canonical_name values. The generic failures are mapped back onto the RSK
/// [`PoolResolverError`] variants the callers and tests expect.
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

/// View an [`RskMinerEntry`] as a generic [`IdentityRegistryEntry`] for the shared
/// validation/distinct-pool helpers; keeps the `miner_address` JSON field name.
fn rsk_registry_entry(entry: &RskMinerEntry) -> IdentityRegistryEntry<'_> {
    IdentityRegistryEntry {
        identifier: &entry.miner_address,
        pool_slug: &entry.pool_slug,
        pool_canonical_name: &entry.pool_canonical_name,
    }
}

/// RSK identifier format check: 40 hex chars after [`normalize_rsk_address`].
fn validate_rsk_miner_address_format(address: &str) -> Result<(), String> {
    let normalized = normalize_rsk_address(address);
    if normalized.len() == 40 && normalized.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("expected 40 hex characters (optionally prefixed with 0x)".to_owned())
    }
}

/// Map a shared [`IdentityRegistryError`] onto the RSK [`PoolResolverError`]
/// variant for that failure (an invalid identifier is an invalid RSK miner
/// address; the duplicate value carries the normalized key).
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

    /// Cross-registry completeness invariant: every pool slug referenced by the
    /// RSK miner-address registry must exist in the embedded BTC snapshot with
    /// its full BTC attribution. Otherwise `upsert_rsk_only_pools` would
    /// self-create the shared pool as a bare stub (empty `coinbase_tags` /
    /// `payout_addresses`), silently downgrading its BTC captures to "unknown".
    /// This generalizes to any future miner-address identity registry.
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
                "generated_at": "2026-05-26",
                "scope": "test",
                "source": { "name": "test" },
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
                "generated_at": "2026-05-26",
                "scope": "test",
                "source": { "name": "test" },
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
                "generated_at": "2026-05-26",
                "scope": "test",
                "source": { "name": "test" },
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
                "generated_at": "2026-05-26",
                "scope": "test",
                "source": { "name": "test" },
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

    #[test]
    fn distinct_pool_definitions_preserves_first_seen_order() {
        let registry = PoolIdentityRegistry::from_rsk_json_str(
            r#"{
                "schema_version": 1,
                "generated_at": "2026-05-26",
                "scope": "test",
                "source": { "name": "test" },
                "entries": [
                    {
                        "miner_address": "12d3178a62ef1f520944534ed04504609f7307a1",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    },
                    {
                        "miner_address": "4e5dabc28e4a0f5e5b19fcb56b28c5a1989352c1",
                        "pool_slug": "antpool",
                        "pool_canonical_name": "AntPool"
                    },
                    {
                        "miner_address": "1b7a75ef070ff49e6b9491a26403d799f2099ebd",
                        "pool_slug": "antpool",
                        "pool_canonical_name": "AntPool"
                    }
                ]
            }"#,
        )
        .unwrap();

        let definitions = registry.distinct_pool_definitions();
        assert_eq!(
            definitions,
            vec![("f2pool", "F2Pool"), ("antpool", "AntPool")]
        );
    }
}
