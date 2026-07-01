//! Pure, unit-testable generator for `data/pools/current.json`.
//!
//! Reads the upstream `bitcoin-data/mining-pools` per-pool JSON files and emits
//! the embedded `current.json` snapshot in [`PoolRecord`](crate::pool_resolver::PoolRecord)
//! format.
//!
//! The field-mapping, slug-remap, ordering, JSON-formatting, and
//! snapshot-diff logic all live here as pure functions over in-memory inputs so
//! they are covered by `cargo test` with no filesystem or network dependency.
//! The thin `src/bin/gen_pool_snapshot.rs` binary only does IO (read upstream
//! files, verify git cleanliness, write outputs) and delegates the logic to
//! this module.
//!
//! Determinism contract (so the same registry inputs reproduce `current.json`
//! byte-for-byte given the same `generated_at`):
//!
//! - pools are sorted by slug;
//! - JSON uses 2-space indent, a fixed key order, and a trailing newline;
//! - `generated_at` is an explicit caller-supplied value, never wall-clock.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::pool_resolver::PoolSnapshot;

/// One upstream `bitcoin-data/mining-pools` per-pool record (`pools/<slug>.json`).
///
/// The upstream `id` is intentionally captured but NEVER written to the
/// snapshot: the DB keys pools on slug only (`pool.slug` UNIQUE), so the
/// upstream id is irrelevant for our stable-key contract.
#[derive(Debug, Clone, Deserialize)]
pub struct UpstreamPool {
    #[serde(default)]
    pub id: Option<u64>,
    pub name: String,
    #[serde(default)]
    pub addresses: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub link: Option<String>,
}

/// An upstream pool paired with its source filename stem (e.g. `spiderpool`
/// for `pools/spiderpool.json`). The filename stem is the default slug for new
/// pools and the lookup key into the pinned slug map.
#[derive(Debug, Clone)]
pub struct UpstreamPoolFile {
    pub filename_stem: String,
    pub pool: UpstreamPool,
}

/// The pinned slug map (`data/pools/slug-map.json`): upstream filename stem ->
/// repo slug. Only genuine remaps live here; new pools default to their
/// filename stem.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SlugMap {
    #[serde(default)]
    pub remap: BTreeMap<String, String>,
}

impl SlugMap {
    /// Parse the pinned slug map JSON (`data/pools/slug-map.json`). Parse errors
    /// are wrapped as [`GeneratorError::InvalidSlugMap`] so the binary surfaces a
    /// generator error, not a raw serde error.
    pub fn from_json_str(json: &str) -> Result<Self, GeneratorError> {
        serde_json::from_str(json).map_err(|err| GeneratorError::InvalidSlugMap(err.to_string()))
    }

    /// Resolve the stable repo slug for an upstream filename stem: the pinned
    /// remap target if one exists, otherwise the filename stem itself.
    pub fn resolve<'a>(&'a self, filename_stem: &'a str) -> &'a str {
        self.remap
            .get(filename_stem)
            .map(String::as_str)
            .unwrap_or(filename_stem)
    }
}

/// A generated pool record in snapshot (`PoolRecord`) format. Serialized with a
/// fixed key order so the JSON is byte-stable. `source_id` is deliberately
/// dropped (the DB keys on slug only) and `link` is omitted when absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GeneratedPool {
    pub slug: String,
    pub canonical_name: String,
    pub coinbase_tags: Vec<String>,
    pub payout_addresses: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// The `source` provenance block written into `current.json`. The
/// `source_metadata_is_forward_looking` test pins the serialized contents
/// (license, name, notes, upstream_url) so the embedded file stays byte-stable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GeneratedSource {
    pub name: String,
    pub upstream_url: String,
    pub license: String,
    pub notes: String,
}

/// The full generated snapshot, serialized to `current.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GeneratedSnapshot {
    pub schema_version: u32,
    pub generated_at: String,
    pub scope: String,
    pub source: GeneratedSource,
    pub pools: Vec<GeneratedPool>,
}

/// Upstream registry URL recorded in the snapshot `source` block
/// (`bitcoin-data/mining-pools`).
pub const UPSTREAM_URL: &str = "https://github.com/bitcoin-data/mining-pools";
/// Upstream registry license recorded in the snapshot `source` block (MIT).
pub const UPSTREAM_LICENSE: &str = "MIT";
/// Human-readable `scope` string recorded in the snapshot: this registry
/// covers only BTC-coinbase pool attribution for the shared Bitcoin parent.
pub const SNAPSHOT_SCOPE: &str = "BTC-coinbase pool registry for shared Bitcoin parent attribution";
/// Provenance note recorded in the snapshot `source` block: reproducible
/// generator output, slugs governed by `data/pools/slug-map.json`.
pub const SNAPSHOT_NOTES: &str = "Resolves the shared Bitcoin parent pool for every merge-mined chain. Generated by \
     `gen-pool-snapshot` from public mining-pool registry records. Pool slugs are project-stable \
     identifiers governed by data/pools/slug-map.json.";

/// Errors the pure generator can return: malformed slug-map JSON, a slug-map
/// collision (two upstream files resolving to one slug), or a serialization
/// failure. The binary wraps these into `anyhow` at the IO boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratorError {
    /// `slug-map.json` failed to parse (serde error message carried inline).
    InvalidSlugMap(String),
    /// Two upstream files resolved to the same slug (a slug-map
    /// misconfiguration). Carries the colliding `slug` plus the two filename
    /// stems so the operator can fix the map; `map_pools` rejects rather than
    /// silently merging or splitting a pool.
    DuplicateSlug {
        slug: String,
        first_stem: String,
        duplicate_stem: String,
    },
    /// `serde_json` failed to render the generated snapshot to pretty JSON.
    Serialize(String),
}

impl std::fmt::Display for GeneratorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidSlugMap(err) => write!(formatter, "invalid slug map JSON: {err}"),
            Self::DuplicateSlug {
                slug,
                first_stem,
                duplicate_stem,
            } => write!(
                formatter,
                "slug map collision: upstream files {first_stem:?} and {duplicate_stem:?} both \
                 resolve to slug {slug:?}"
            ),
            Self::Serialize(err) => write!(formatter, "serialize generated snapshot: {err}"),
        }
    }
}

impl std::error::Error for GeneratorError {}

/// Map upstream pool files to deterministic [`GeneratedPool`] records.
///
/// - field map: `canonical_name<-name`, `coinbase_tags<-tags`,
///   `payout_addresses<-addresses`, `link` (empty/missing -> omitted);
/// - slug: the pinned slug-map target, defaulting to the filename stem;
/// - the upstream `id` and `source_id` are dropped;
/// - pools are sorted by slug for a byte-stable output.
///
/// Rejects two upstream files that resolve to the same slug (a slug-map
/// misconfiguration that would otherwise split or collide a pool).
pub fn map_pools(
    files: &[UpstreamPoolFile],
    slug_map: &SlugMap,
) -> Result<Vec<GeneratedPool>, GeneratorError> {
    let mut by_slug: BTreeMap<String, (String, GeneratedPool)> = BTreeMap::new();
    for file in files {
        let slug = slug_map.resolve(&file.filename_stem).to_owned();
        let link = file
            .pool
            .link
            .as_deref()
            .map(str::trim)
            .filter(|link| !link.is_empty())
            .map(str::to_owned);
        let generated = GeneratedPool {
            slug: slug.clone(),
            canonical_name: file.pool.name.clone(),
            coinbase_tags: file.pool.tags.clone(),
            payout_addresses: file.pool.addresses.clone(),
            link,
        };
        if let Some((first_stem, _)) =
            by_slug.insert(slug.clone(), (file.filename_stem.clone(), generated))
        {
            return Err(GeneratorError::DuplicateSlug {
                slug,
                first_stem,
                duplicate_stem: file.filename_stem.clone(),
            });
        }
    }
    // BTreeMap iteration is already slug-sorted; collect just the records.
    Ok(by_slug.into_values().map(|(_, pool)| pool).collect())
}

/// Wrap mapped pools with the fixed snapshot envelope (`schema_version` 1,
/// scope, and the `source` provenance block). `generated_at` is supplied by the
/// caller and never read from wall-clock, so the same inputs reproduce
/// `current.json` byte-for-byte.
pub fn build_snapshot(pools: Vec<GeneratedPool>, generated_at: &str) -> GeneratedSnapshot {
    GeneratedSnapshot {
        schema_version: 1,
        generated_at: generated_at.to_owned(),
        scope: SNAPSHOT_SCOPE.to_owned(),
        source: GeneratedSource {
            name: "bitcoin-data/mining-pools per-pool files".to_owned(),
            upstream_url: UPSTREAM_URL.to_owned(),
            license: UPSTREAM_LICENSE.to_owned(),
            notes: SNAPSHOT_NOTES.to_owned(),
        },
        pools,
    }
}

/// Project a committed resolver snapshot back into generator pool records for
/// diffing or byte-stability checks.
pub fn generated_pools_from_snapshot(snapshot: &PoolSnapshot) -> Vec<GeneratedPool> {
    snapshot
        .pools
        .iter()
        .map(|pool| GeneratedPool {
            slug: pool.slug.clone(),
            canonical_name: pool.canonical_name.clone(),
            coinbase_tags: pool.coinbase_tags.clone(),
            payout_addresses: pool.payout_addresses.clone(),
            link: pool.link.clone(),
        })
        .collect()
}

/// Civil date (`YYYY-MM-DD`, UTC) from a Unix timestamp in seconds, using
/// Howard Hinnant's civil-from-days algorithm. Pure and unit-tested so the
/// date arithmetic the interactive `--generated-at` default depends on is
/// covered (reproducible / `--check` paths never call wall-clock).
pub fn civil_date_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's civil date in UTC (`YYYY-MM-DD`). Convenience default for
/// interactive regeneration when `--generated-at` is omitted; pin
/// `--generated-at` for reproducible builds.
pub fn today_utc() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_date_utc(secs)
}

/// Serialize a value to pretty 2-space-indented JSON with a trailing newline,
/// matching the committed `current.json` formatting exactly so the generator
/// output is byte-stable.
pub fn to_pretty_json<T: Serialize>(value: &T) -> Result<String, GeneratorError> {
    let mut out = serde_json::to_string_pretty(value)
        .map_err(|err| GeneratorError::Serialize(err.to_string()))?;
    out.push('\n');
    Ok(out)
}

/// Render the snapshot JSON bytes (the exact `current.json` contents).
pub fn render_snapshot_json(snapshot: &GeneratedSnapshot) -> Result<String, GeneratorError> {
    to_pretty_json(snapshot)
}

/// A reviewable diff of a regenerated snapshot vs the committed one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotDiff {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// Slugs whose canonical_name, coinbase_tags, payout_addresses, or link
    /// changed.
    pub changed: Vec<String>,
}

impl SnapshotDiff {
    /// True when the regenerated snapshot is identical to the committed one (no
    /// added, removed, or changed slugs). The binary's `--check` path keys its
    /// exit status on this.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// Diff a regenerated pool set against the committed one so churn (added,
/// removed, changed slugs) is explicit and reviewable before commit. Removed
/// pools are flagged but not deleted: `upsert_pool_snapshot` never DELETEs, so
/// their DB rows and historical FKs remain as tombstones.
pub fn diff_pools(committed: &[GeneratedPool], regenerated: &[GeneratedPool]) -> SnapshotDiff {
    let committed_by_slug: BTreeMap<&str, &GeneratedPool> =
        committed.iter().map(|p| (p.slug.as_str(), p)).collect();
    let regenerated_by_slug: BTreeMap<&str, &GeneratedPool> =
        regenerated.iter().map(|p| (p.slug.as_str(), p)).collect();

    let mut diff = SnapshotDiff::default();
    for (slug, pool) in &regenerated_by_slug {
        match committed_by_slug.get(slug) {
            None => diff.added.push((*slug).to_owned()),
            Some(prev) if prev != pool => diff.changed.push((*slug).to_owned()),
            Some(_) => {}
        }
    }
    for slug in committed_by_slug.keys() {
        if !regenerated_by_slug.contains_key(slug) {
            diff.removed.push((*slug).to_owned());
        }
    }
    diff.added.sort();
    diff.removed.sort();
    diff.changed.sort();
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(
        stem: &str,
        name: &str,
        tags: &[&str],
        addrs: &[&str],
        link: Option<&str>,
    ) -> UpstreamPoolFile {
        UpstreamPoolFile {
            filename_stem: stem.to_owned(),
            pool: UpstreamPool {
                id: Some(7),
                name: name.to_owned(),
                addresses: addrs.iter().map(|s| s.to_string()).collect(),
                tags: tags.iter().map(|s| s.to_string()).collect(),
                link: link.map(str::to_owned),
            },
        }
    }

    fn default_slug_map() -> SlugMap {
        SlugMap::from_json_str(
            r#"{ "remap": { "ocean": "ocean-xyz", "braiins": "braiins-pool" } }"#,
        )
        .unwrap()
    }

    #[test]
    fn maps_field_names_and_drops_id() {
        let files = vec![upstream(
            "spiderpool",
            "SpiderPool",
            &["SpiderPool"],
            &["125m2H43pwKpSZjLhMQHneuTwTJN5qRyYu"],
            Some("https://www.spiderpool.com"),
        )];
        let pools = map_pools(&files, &default_slug_map()).unwrap();
        assert_eq!(pools.len(), 1);
        let pool = &pools[0];
        assert_eq!(pool.slug, "spiderpool");
        assert_eq!(pool.canonical_name, "SpiderPool");
        assert_eq!(pool.coinbase_tags, vec!["SpiderPool".to_owned()]);
        assert_eq!(
            pool.payout_addresses,
            vec!["125m2H43pwKpSZjLhMQHneuTwTJN5qRyYu".to_owned()]
        );
        assert_eq!(pool.link.as_deref(), Some("https://www.spiderpool.com"));
    }

    #[test]
    fn remaps_pinned_slugs() {
        let files = vec![
            upstream(
                "ocean",
                "Ocean.xyz",
                &["OCEAN.XYZ"],
                &[],
                Some("https://ocean.xyz/"),
            ),
            upstream(
                "braiins",
                "Braiins Pool",
                &["/slush/"],
                &[],
                Some("https://braiins.com/pool"),
            ),
        ];
        let pools = map_pools(&files, &default_slug_map()).unwrap();
        let slugs: Vec<&str> = pools.iter().map(|p| p.slug.as_str()).collect();
        // Sorted by slug: braiins-pool before ocean-xyz.
        assert_eq!(slugs, vec!["braiins-pool", "ocean-xyz"]);
    }

    #[test]
    fn new_pool_defaults_to_filename_stem() {
        let files = vec![upstream("secpool", "SecPool", &["SecPool"], &[], None)];
        let pools = map_pools(&files, &default_slug_map()).unwrap();
        assert_eq!(pools[0].slug, "secpool");
    }

    #[test]
    fn filename_rename_does_not_change_slug_via_map() {
        // Upstream renamed `ocean.json` -> `ocean-mining.json`; the pinned map
        // keeps the stable slug so the DB id is unaffected.
        let mut map = default_slug_map();
        map.remap
            .insert("ocean-mining".to_owned(), "ocean-xyz".to_owned());
        let files = vec![upstream(
            "ocean-mining",
            "Ocean.xyz",
            &["OCEAN.XYZ"],
            &[],
            None,
        )];
        let pools = map_pools(&files, &map).unwrap();
        assert_eq!(pools[0].slug, "ocean-xyz");
    }

    #[test]
    fn rejects_slug_map_collision() {
        let mut map = default_slug_map();
        map.remap
            .insert("ocean-old".to_owned(), "ocean-xyz".to_owned());
        let files = vec![
            upstream("ocean", "Ocean.xyz", &["OCEAN.XYZ"], &[], None),
            upstream("ocean-old", "Ocean Legacy", &["OCEAN.OLD"], &[], None),
        ];
        let err = map_pools(&files, &map).unwrap_err();
        match err {
            GeneratorError::DuplicateSlug { slug, .. } => assert_eq!(slug, "ocean-xyz"),
            other => panic!("expected DuplicateSlug, got {other:?}"),
        }
    }

    #[test]
    fn empty_or_missing_link_is_omitted() {
        let files = vec![
            upstream("a", "A", &["A"], &[], Some("")),
            upstream("b", "B", &["B"], &[], None),
            upstream("c", "C", &["C"], &[], Some("  ")),
        ];
        let pools = map_pools(&files, &default_slug_map()).unwrap();
        for pool in &pools {
            assert_eq!(pool.link, None, "link should be omitted for {}", pool.slug);
        }
        // The omitted link must not appear in the serialized JSON.
        let snapshot = build_snapshot(pools, "2026-06-05");
        let json = render_snapshot_json(&snapshot).unwrap();
        assert!(!json.contains("\"link\""));
    }

    #[test]
    fn pools_are_sorted_by_slug() {
        let files = vec![
            upstream("zulupool", "ZuluPool", &["ZuluPool"], &[], None),
            upstream("antpool", "AntPool", &["/AntPool/"], &[], None),
            upstream(
                "foundry-usa",
                "Foundry USA",
                &["Foundry USA Pool"],
                &[],
                None,
            ),
        ];
        let pools = map_pools(&files, &default_slug_map()).unwrap();
        let slugs: Vec<&str> = pools.iter().map(|p| p.slug.as_str()).collect();
        assert_eq!(slugs, vec!["antpool", "foundry-usa", "zulupool"]);
    }

    #[test]
    fn json_formatting_is_two_space_indent_with_trailing_newline() {
        let files = vec![upstream(
            "antpool",
            "AntPool",
            &["/AntPool/"],
            &["1abc"],
            None,
        )];
        let pools = map_pools(&files, &default_slug_map()).unwrap();
        let snapshot = build_snapshot(pools, "2026-06-05");
        let json = render_snapshot_json(&snapshot).unwrap();
        assert!(json.ends_with("}\n"));
        assert!(json.contains("\n  \"schema_version\": 1,"));
        // 2-space-per-level: a pool object sits in the `pools` array (level 2,
        // 4-space indent), its `coinbase_tags` key at level 3 (6 spaces), and
        // each tag value at level 4 (8 spaces).
        assert!(json.contains("\n      \"coinbase_tags\": ["));
        assert!(json.contains("\n        \"/AntPool/\""));
    }

    #[test]
    fn snapshot_reproduces_byte_for_byte_given_same_inputs() {
        let files = vec![
            upstream(
                "spiderpool",
                "SpiderPool",
                &["SpiderPool"],
                &["1addr"],
                Some("https://s"),
            ),
            upstream("antpool", "AntPool", &["/AntPool/"], &[], None),
        ];
        let map = default_slug_map();
        let pools_a = map_pools(&files, &map).unwrap();
        let pools_b = map_pools(&files, &map).unwrap();
        let snap_a = build_snapshot(pools_a, "2026-06-05");
        let snap_b = build_snapshot(pools_b, "2026-06-05");
        let json_a = render_snapshot_json(&snap_a).unwrap();
        let json_b = render_snapshot_json(&snap_b).unwrap();
        assert_eq!(json_a, json_b);
    }

    #[test]
    fn embedded_current_json_is_byte_stable() {
        // Pin the byte-stability contract `--check` depends on, without needing
        // the upstream clone: parse the embedded current.json, project its own
        // pools, re-render via `render_snapshot_json`, and assert the bytes are
        // identical (2-space indent, key order, trailing newline, and unescaped
        // non-ASCII such as f2pool's CJK / emoji tags). Catches formatting drift
        // in the committed file in CI.
        use crate::pool_resolver::{DEFAULT_POOL_SNAPSHOT_JSON, PoolSnapshot};

        let parsed: PoolSnapshot = serde_json::from_str(DEFAULT_POOL_SNAPSHOT_JSON).unwrap();
        let pools = generated_pools_from_snapshot(&parsed);
        let rebuilt = build_snapshot(pools, &parsed.generated_at);
        let rendered = render_snapshot_json(&rebuilt).unwrap();
        assert_eq!(
            rendered, DEFAULT_POOL_SNAPSHOT_JSON,
            "embedded current.json drifted from the generator's byte-stable output"
        );
    }

    #[test]
    fn source_metadata_is_forward_looking() {
        let snapshot = build_snapshot(Vec::new(), "2026-06-05");
        let json = render_snapshot_json(&snapshot).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let source = parsed["source"].as_object().unwrap();
        let keys: Vec<&str> = source.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["license", "name", "notes", "upstream_url"]);
        assert_eq!(source["license"], "MIT");
    }

    #[test]
    fn civil_date_utc_matches_known_epoch_days() {
        // Fixed Unix timestamps (UTC midnight) -> known civil dates.
        assert_eq!(civil_date_utc(0), "1970-01-01");
        // 2000-01-01: 946684800 = 10957 days * 86400.
        assert_eq!(civil_date_utc(946_684_800), "2000-01-01");
        // 2000-02-29 (leap day) = 951782400.
        assert_eq!(civil_date_utc(951_782_400), "2000-02-29");
        // 2026-06-05 midnight UTC = 1780617600; mid-day seconds round down to
        // the same date.
        assert_eq!(civil_date_utc(1_780_617_600), "2026-06-05");
        assert_eq!(civil_date_utc(1_780_617_600 + 86_399), "2026-06-05");
    }

    #[test]
    fn rendered_snapshot_with_duplicate_tag_fails_resolver_validation() {
        // The generator validates its own output by constructing a
        // `PoolResolver` from the rendered JSON before writing. Two pools that
        // share a coinbase tag must therefore fail at generation time, not only
        // at producer startup. This pins the property `build_validated_snapshot`
        // in the binary relies on.
        use crate::pool_resolver::PoolResolver;

        let files = vec![
            upstream("one", "One", &["SHARED"], &[], None),
            upstream("two", "Two", &["SHARED"], &[], None),
        ];
        let pools = map_pools(&files, &default_slug_map()).unwrap();
        let snapshot = build_snapshot(pools, "2026-06-05");
        let json = render_snapshot_json(&snapshot).unwrap();
        let err = PoolResolver::from_json_str(&json).unwrap_err();
        assert!(
            matches!(
                err,
                crate::pool_resolver::PoolResolverError::DuplicateValue {
                    field: "coinbase_tags",
                    ..
                }
            ),
            "expected DuplicateValue for coinbase_tags, got {err:?}"
        );
    }

    #[test]
    fn diff_reports_added_removed_and_changed() {
        let committed = vec![
            GeneratedPool {
                slug: "antpool".to_owned(),
                canonical_name: "AntPool".to_owned(),
                coinbase_tags: vec!["/AntPool/".to_owned()],
                payout_addresses: vec![],
                link: None,
            },
            GeneratedPool {
                slug: "oldpool".to_owned(),
                canonical_name: "OldPool".to_owned(),
                coinbase_tags: vec!["OldPool".to_owned()],
                payout_addresses: vec![],
                link: None,
            },
        ];
        let regenerated = vec![
            GeneratedPool {
                slug: "antpool".to_owned(),
                canonical_name: "AntPool".to_owned(),
                // tag changed
                coinbase_tags: vec!["/AntPool/".to_owned(), "Mined by AntPool".to_owned()],
                payout_addresses: vec![],
                link: None,
            },
            GeneratedPool {
                slug: "spiderpool".to_owned(),
                canonical_name: "SpiderPool".to_owned(),
                coinbase_tags: vec!["SpiderPool".to_owned()],
                payout_addresses: vec![],
                link: None,
            },
        ];
        let diff = diff_pools(&committed, &regenerated);
        assert_eq!(diff.added, vec!["spiderpool".to_owned()]);
        assert_eq!(diff.removed, vec!["oldpool".to_owned()]);
        assert_eq!(diff.changed, vec!["antpool".to_owned()]);
        assert!(!diff.is_empty());
    }

    #[test]
    fn diff_is_empty_for_identical_sets() {
        let pools = vec![GeneratedPool {
            slug: "antpool".to_owned(),
            canonical_name: "AntPool".to_owned(),
            coinbase_tags: vec!["/AntPool/".to_owned()],
            payout_addresses: vec![],
            link: None,
        }];
        let diff = diff_pools(&pools, &pools.clone());
        assert!(diff.is_empty());
    }
}
