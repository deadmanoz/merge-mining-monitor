//! BTC-coinbase pool resolution. The embedded `data/pools/current.json`
//! snapshot is used to attribute a Bitcoin coinbase script or payout
//! address to a known mining pool.

use std::collections::HashMap;

use serde::Deserialize;

use super::error::PoolResolverError;

/// The embedded `data/pools/current.json` BTC pool snapshot, `include_str!`-pulled
/// at compile time. Reproducible generator output (pool_snapshot_gen), never
/// hand-edited: rebuild to pick up changes. [`PoolResolver::from_default_snapshot`]
/// parses it; production attribution depends on it being valid and duplicate-free.
pub const DEFAULT_POOL_SNAPSHOT_JSON: &str = include_str!("../../../../data/pools/current.json");

/// Deserialized form of `data/pools/current.json`: the schema version (must be 1),
/// provenance metadata, and the flat list of [`PoolRecord`] entries. Consumed by
/// `mmm-store` to populate the `pool` base table and by [`PoolResolver`] to build
/// its tag/address indexes. `Deserialize`-only; the generator (pool_snapshot_gen)
/// is the sole writer of the JSON it is parsed from.
#[derive(Debug, Clone, Deserialize)]
pub struct PoolSnapshot {
    /// Snapshot schema version. Validation hard-fails any value other than 1
    /// (`UnsupportedSchemaVersion`); a bump is a deliberate breaking change to
    /// the embedded JSON contract, not a silent migration.
    pub schema_version: u32,
    /// Generator provenance carried verbatim: when the snapshot was produced.
    pub generated_at: String,
    /// Generator provenance carried verbatim: which pool set the snapshot covers.
    pub scope: String,
    /// Generator provenance carried verbatim: the upstream the data came from.
    pub source: PoolSnapshotSource,
    /// The attribution payload: every pool's tags and addresses. Pub because
    /// `mmm-store` and the gen bin read these fields directly.
    pub pools: Vec<PoolRecord>,
}

/// Provenance block of a pool snapshot or registry: upstream name, optional URL,
/// license, and notes. Shared by both [`PoolSnapshot`] and the RSK
/// [`RskMinerRegistry`](super::RskMinerRegistry) so the two embedded files
/// declare provenance identically.
#[derive(Debug, Clone, Deserialize)]
pub struct PoolSnapshotSource {
    pub name: String,
    pub upstream_url: Option<String>,
    pub license: Option<String>,
    pub notes: Option<String>,
}

/// One pool's BTC attribution row. A pool may carry coinbase tags, payout
/// addresses, both, or (for RSK-only pools) neither BTC signal.
#[derive(Debug, Clone, Deserialize)]
pub struct PoolRecord {
    /// Stable DB primary-key string. Must survive registry refreshes; a rename is
    /// a deliberate `UPDATE pool SET slug` migration, never a regen side effect.
    pub slug: String,
    /// Upstream-churn metadata. Intentionally ignored when re-attributing.
    pub source_id: Option<u64>,
    /// Display name for the pool.
    pub canonical_name: String,
    /// Tags matched as raw byte substrings of a BTC coinbase script, longest-first.
    pub coinbase_tags: Vec<String>,
    /// Exact payout addresses, matched after trimming.
    pub payout_addresses: Vec<String>,
    /// Upstream metadata link.
    pub link: Option<String>,
}

/// Which BTC signal produced a [`PoolMatch`]. `capture` maps these to the
/// attribution-keying namespace: `CoinbaseTag` keys under
/// `BTC_COINBASE_TAG_NAMESPACE`, `PayoutAddress` under
/// `BTC_PAYOUT_ADDRESS_NAMESPACE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    /// Matched because one of the pool's `coinbase_tags` is a raw byte substring
    /// of the coinbase script. Keyed under `BTC_COINBASE_TAG_NAMESPACE`.
    CoinbaseTag,
    /// Matched because a candidate address equals one of the pool's
    /// `payout_addresses` (after trimming). Keyed under
    /// `BTC_PAYOUT_ADDRESS_NAMESPACE`.
    PayoutAddress,
}

/// A successful BTC pool resolution: the borrowed [`PoolRecord`], which signal
/// hit ([`MatchKind`]), and the exact `matched_value`. `matched_value` is the
/// specific tag string or trimmed address that actually matched (not the input
/// bytes); `capture` persists it verbatim as the attribution `matched_value`.
#[derive(Debug, Clone, Copy)]
pub struct PoolMatch<'a> {
    /// The matched snapshot row, borrowed from the resolver.
    pub pool: &'a PoolRecord,
    /// Which signal hit.
    pub matched_by: MatchKind,
    /// The exact tag/address that matched; becomes the persisted attribution value.
    pub matched_value: &'a str,
}

/// Index over a [`PoolSnapshot`] for BTC coinbase-tag and payout-address
/// attribution. Construction validates the snapshot (schema == 1, no duplicate
/// slug/tag/address) and pre-sorts coinbase tags longest-first so the most
/// specific tag wins a substring match. Pure: owns the snapshot plus derived
/// lookup structures, no I/O.
#[derive(Debug)]
pub struct PoolResolver {
    snapshot: PoolSnapshot,
    coinbase_tags: Vec<TagIndexEntry>,
    payout_addresses: HashMap<String, usize>,
}

/// One (tag, owning pool) entry in the coinbase-tag index. The `Vec` of these is
/// sorted longest-tag-first (then lexically) at build time so
/// `resolve_coinbase_script`'s first substring hit is the most specific tag.
#[derive(Debug)]
struct TagIndexEntry {
    tag: String,
    pool_index: usize,
}

impl PoolResolver {
    /// Build the resolver from the compile-time-embedded
    /// [`DEFAULT_POOL_SNAPSHOT_JSON`]. The production entry point. Errors only if
    /// the embedded JSON is malformed or violates the duplicate/empty/schema rules
    /// (a CI test guards against this), so prod callers `?` it at startup.
    pub fn from_default_snapshot() -> Result<Self, PoolResolverError> {
        Self::from_json_str(DEFAULT_POOL_SNAPSHOT_JSON)
    }

    /// Parse a pool snapshot from a JSON string, then validate and index it. Used
    /// by the `gen_pool_snapshot` bin to load a freshly generated snapshot for
    /// drift comparison, and by the tests here. Parse failures map to
    /// `InvalidSnapshotJson`.
    pub fn from_json_str(json: &str) -> Result<Self, PoolResolverError> {
        let snapshot = serde_json::from_str(json)
            .map_err(|err| PoolResolverError::InvalidSnapshotJson(err.to_string()))?;
        Self::from_snapshot(snapshot)
    }

    /// Validate an already-parsed [`PoolSnapshot`] and build the lookup indexes:
    /// flatten every pool's coinbase tags, sort longest-first then lexically (so
    /// the most specific tag wins a substring match and ordering is
    /// deterministic), and map each trimmed payout address to its pool. Rejects
    /// schema != 1, empty fields, and duplicate slug/tag/address.
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

    /// Borrow the validated underlying snapshot. The standard handoff to
    /// `mmm-store`, which writes the `pool` base table from it, so a validated
    /// resolver and the persisted pool rows stay in lockstep.
    pub fn snapshot(&self) -> &PoolSnapshot {
        &self.snapshot
    }

    /// Attribute a BTC coinbase by scanning its raw script bytes for any pool tag
    /// as a byte substring, longest tag first (so a longer tag wins over a shorter
    /// one it contains). `coinbase_script` is the raw scriptSig bytes; tags are
    /// matched as UTF-8 bytes with no decoding or normalization. Returns the first
    /// (longest) hit.
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

    /// Attribute a single payout address by exact match against the snapshot's
    /// address index. The input is trimmed before lookup (snapshot addresses are
    /// trimmed at index time), so surrounding whitespace is tolerated; the match
    /// is otherwise exact and case-sensitive. `matched_value` returns the stored
    /// address, not the input.
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

    /// Resolve the first payout address in the iterator that maps to a known pool.
    /// Used when a coinbase has several outputs: try each candidate in order and
    /// return the first [`PoolMatch`]. Each candidate goes through
    /// [`resolve_payout_address`](Self::resolve_payout_address) (trim + exact match).
    pub fn resolve_payout_addresses<'a, I>(&self, addresses: I) -> Option<PoolMatch<'_>>
    where
        I: IntoIterator<Item = &'a str>,
    {
        addresses
            .into_iter()
            .find_map(|address| self.resolve_payout_address(address))
    }
}

/// Reject a snapshot before it can be indexed or persisted: schema must be 1;
/// every slug, canonical_name, coinbase tag, and payout address must be non-empty;
/// and slug, coinbase tag, and trimmed payout address must each be globally unique
/// across pools. The hard regen gate: a duplicate in the embedded current.json
/// fails here (`DuplicateValue`) in CI before it could mis-resolve in production.
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

/// Shared field-presence check: error (`EmptyValue`) if `value` is empty after
/// trimming. `pub(super)` so the sibling RSK `identity` validator reuses the exact
/// same emptiness rule and error format across both embedded registries.
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

/// True if `needle` occurs as a contiguous byte subsequence of `haystack`. Empty
/// needles never match. Operates on raw bytes (no UTF-8 awareness), which is what
/// coinbase-tag matching needs: tags are matched against arbitrary script bytes.
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
    fn resolves_newly_added_pool_by_coinbase_tag() {
        // A pool outside the old 9-pool subset (SpiderPool) now resolves.
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let resolved = resolver
            .resolve_coinbase_script(b"\x03\x01\x02\x03/SpiderPool/837/\x00")
            .expect("SpiderPool coinbase tag should resolve after expansion");
        assert_eq!(resolved.pool.slug, "spiderpool");
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
