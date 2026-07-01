//! Elastos child-side reward and minerinfo identity extraction.
//!
//! The Elastos `auxpow` field is a CAuxPow-only blob and does not authenticate
//! the decoded `tx` vector returned by JSON-RPC. These helpers therefore record
//! reward addresses and minerinfo as RPC-observed decoded identity facts. Pool
//! attribution is upgraded only through explicit `pool_identity` rows.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bitcoin::base58;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_postgres::Client;

use crate::chains::elastos::rpc::ElastosBlock;
use mmm_capture::capture::{
    CHILD_PAYOUT_REGISTRY_SOURCE, EventPoolAttribution, PoolAttributionConfidence,
    PoolAttributionSide,
};
use mmm_capture::child_payout::{PoolIdentityLookup, PoolIdentityRef, pool_identity_lookup_key};
use mmm_capture::identity_registry::{
    IdentityRegistryEntry, accept_any_identifier, identity_key, validate_identity_registry,
};
use mmm_store::upsert_identity_registry;

/// `pool_identity.namespace` for reward-address rows: the key both the loader and
/// the lookup join on to map a payout address to a pool.
pub const ELASTOS_REWARD_ADDRESS_NAMESPACE: &str = "elastos_reward_address";
/// `pool_identity.namespace` for minerinfo rows (the block tag / coinbase payload).
pub const ELASTOS_MINERINFO_NAMESPACE: &str = "elastos_minerinfo";
/// `event_pool_attribution.source` for an UNMAPPED reward address (no
/// `pool_identity` row); a mapped one is promoted to `CHILD_PAYOUT_REGISTRY_SOURCE`.
pub const ELASTOS_RPC_REWARD_ADDRESS_SOURCE: &str = "elastos_rpc_reward_address";
/// `event_pool_attribution.source` for an UNMAPPED minerinfo value; a mapped one is
/// promoted to `CHILD_PAYOUT_REGISTRY_SOURCE`.
pub const ELASTOS_RPC_MINERINFO_SOURCE: &str = "elastos_rpc_minerinfo";

/// Provenance stamp recorded in every attribution's `details`: these identities are
/// decoded from RPC JSON and are NOT authenticated by the CAuxPow blob.
const IDENTITY_AUTHORITY: &str = "elastos_rpc_decoded_unverified_by_auxpow";
/// Protocol payout sinks (asset/stake addresses), not miner payouts; their reward
/// outputs are excluded from attribution.
const RESERVED_ADDRESS_PREFIXES: [&str; 2] = ["CRASSETS", "STAKEREWARD"];

/// The embedded `data/pools/child-identities/elastos_minerinfo_registry.json`, `include_str!`-baked
/// at compile time. A reviewed mapping of Elastos minerinfo labels to pool slugs;
/// edited deliberately under review. Promoted only when the label is itself a
/// documented Bitcoin coinbase signature for the pool the same-event BTC parent
/// reconciled to (the identity is RPC-decoded, not AuxPoW-authenticated).
const DEFAULT_ELASTOS_MINERINFO_REGISTRY_JSON: &str =
    include_str!("../../../../../data/pools/child-identities/elastos_minerinfo_registry.json");

/// Validated `data/pools/child-identities/elastos_minerinfo_registry.json`: the schema version
/// (only `1` is accepted) and the flat `minerinfo -> pool` entries.
/// Provenance/evidence fields in the JSON are ignored on parse.
#[derive(Debug, Clone, Deserialize)]
struct ElastosMinerinfoRegistry {
    /// Only `1` is accepted.
    schema_version: u32,
    /// The minerinfo label -> pool mappings.
    entries: Vec<ElastosMinerinfoEntry>,
}

/// One reviewed minerinfo mapping. `minerinfo` is matched byte-for-byte against the
/// RPC-decoded label (emoji and ASCII strings alike), so it is never trimmed or
/// case-folded.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ElastosMinerinfoEntry {
    minerinfo: String,
    pool_slug: String,
    pool_canonical_name: String,
}

impl ElastosMinerinfoRegistry {
    /// Parse and validate registry JSON via the shared
    /// [`validate_identity_registry`] validator. The minerinfo identifier is a free
    /// string (only non-empty/whitespace-free is required; matched byte-for-byte),
    /// so the identifier validator is [`accept_any_identifier`].
    fn from_json_str(json: &str) -> Result<Self> {
        let registry: Self =
            serde_json::from_str(json).context("parse Elastos minerinfo registry JSON")?;
        validate_identity_registry(
            registry.schema_version,
            registry.entries.iter().map(elastos_minerinfo_entry),
            "minerinfo",
            accept_any_identifier,
            identity_key,
        )
        .map_err(|err| anyhow::anyhow!("invalid Elastos minerinfo registry: {err}"))?;
        Ok(registry)
    }
}

/// View an [`ElastosMinerinfoEntry`] as a generic [`IdentityRegistryEntry`] for the
/// shared validation/seed helpers; keeps the `minerinfo` field name.
fn elastos_minerinfo_entry(entry: &ElastosMinerinfoEntry) -> IdentityRegistryEntry<'_> {
    IdentityRegistryEntry {
        identifier: &entry.minerinfo,
        pool_slug: &entry.pool_slug,
        pool_canonical_name: &entry.pool_canonical_name,
    }
}

/// Seed Elastos minerinfo identities from explicit registry JSON: validate, then
/// upsert pools + `elastos_minerinfo` `pool_identity` rows through the shared
/// orchestrator (idempotent, non-remapping). This is the injectable core the
/// production wrapper delegates to.
async fn upsert_elastos_minerinfo_pool_identities_from_json(
    client: &Client,
    registry_json: &str,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<HashMap<String, i64>> {
    let registry = ElastosMinerinfoRegistry::from_json_str(registry_json)
        .context("load Elastos minerinfo registry")?;
    let entries: Vec<IdentityRegistryEntry<'_>> = registry
        .entries
        .iter()
        .map(elastos_minerinfo_entry)
        .collect();
    upsert_identity_registry(
        client,
        "Elastos minerinfo registry",
        ELASTOS_MINERINFO_NAMESPACE,
        &entries,
        false,
        "refusing to remap automatically; revoke the existing mapping explicitly",
        pool_ids_by_slug,
    )
    .await
    .context("seed Elastos minerinfo pool identities")
}

/// Seed the embedded Elastos minerinfo registry (production wrapper). Run at
/// Elastos capture bootstrap and at replay before identities are loaded, so a
/// fresh database resolves the reviewed labels on the first pass. Idempotent.
pub(crate) async fn upsert_elastos_minerinfo_pool_identities(
    client: &Client,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<HashMap<String, i64>> {
    upsert_elastos_minerinfo_pool_identities_from_json(
        client,
        DEFAULT_ELASTOS_MINERINFO_REGISTRY_JSON,
        pool_ids_by_slug,
    )
    .await
}

/// The embedded `data/pools/child-identities/elastos_reward_address_registry.json`, `include_str!`-baked
/// at compile time. Maps an Elastos child coinbase reward address to a pool slug.
/// The reward address is the reliable Elastos identity key (unlike minerinfo, which
/// secpool stamps as `Antpool`); each mapping rests on the address consistently
/// co-occurring with one pool's independently-attributed Bitcoin parent coinbase.
const DEFAULT_ELASTOS_REWARD_REGISTRY_JSON: &str =
    include_str!("../../../../../data/pools/child-identities/elastos_reward_address_registry.json");

/// Validated `data/pools/child-identities/elastos_reward_address_registry.json`: schema version (only
/// `1`) and the flat `address -> pool` entries. Provenance/evidence fields ignored.
#[derive(Debug, Clone, Deserialize)]
struct ElastosRewardRegistry {
    schema_version: u32,
    entries: Vec<ElastosRewardEntry>,
}

/// One reviewed reward-address mapping. `address` is matched byte-for-byte against
/// the RPC-decoded Elastos coinbase output address.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct ElastosRewardEntry {
    address: String,
    pool_slug: String,
    pool_canonical_name: String,
}

impl ElastosRewardRegistry {
    /// Parse and validate via the shared validator. Elastos mainnet addresses are base58
    /// and start with `E`; that prefix gate plus byte-exact resolve-time matching is
    /// the well-formedness guard (a malformed entry silently never matches).
    fn from_json_str(json: &str) -> Result<Self> {
        let registry: Self =
            serde_json::from_str(json).context("parse Elastos reward registry JSON")?;
        validate_identity_registry(
            registry.schema_version,
            registry.entries.iter().map(elastos_reward_entry),
            "address",
            validate_elastos_reward_address,
            identity_key,
        )
        .map_err(|err| anyhow::anyhow!("invalid Elastos reward registry: {err}"))?;
        Ok(registry)
    }
}

/// Elastos mainnet standard-address base58check version byte (`E...` addresses).
const ELASTOS_ADDRESS_VERSION: u8 = 0x21;
/// Decoded length of an Elastos address: 1 version byte + 20-byte hash.
const ELASTOS_ADDRESS_DECODED_LEN: usize = 21;

/// Elastos reward-address well-formedness gate, fully checksum-validated so a
/// curation typo fails loudly at seed time rather than becoming a mapping that
/// silently never matches an observed reward address. A mainnet address
/// base58check-decodes to 21 bytes under the standard version byte (`E...`).
fn validate_elastos_reward_address(address: &str) -> Result<(), String> {
    let decoded =
        base58::decode_check(address).map_err(|err| format!("not base58check-decodable: {err}"))?;
    if decoded.len() != ELASTOS_ADDRESS_DECODED_LEN {
        return Err(format!(
            "decoded length {}, expected {ELASTOS_ADDRESS_DECODED_LEN}",
            decoded.len()
        ));
    }
    if decoded[0] != ELASTOS_ADDRESS_VERSION {
        return Err(format!(
            "version {:#04x}, expected Elastos mainnet {ELASTOS_ADDRESS_VERSION:#04x}",
            decoded[0]
        ));
    }
    Ok(())
}

/// View an [`ElastosRewardEntry`] as a generic [`IdentityRegistryEntry`].
fn elastos_reward_entry(entry: &ElastosRewardEntry) -> IdentityRegistryEntry<'_> {
    IdentityRegistryEntry {
        identifier: &entry.address,
        pool_slug: &entry.pool_slug,
        pool_canonical_name: &entry.pool_canonical_name,
    }
}

/// Seed the embedded Elastos reward-address registry. Run at Elastos capture
/// bootstrap and at replay before identities are loaded (the minerinfo pattern),
/// so the `elastos_reward_address` namespace resolves on the first pass. Idempotent.
pub(crate) async fn upsert_elastos_reward_address_pool_identities(
    client: &Client,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<HashMap<String, i64>> {
    let registry = ElastosRewardRegistry::from_json_str(DEFAULT_ELASTOS_REWARD_REGISTRY_JSON)
        .context("load Elastos reward registry")?;
    let entries: Vec<IdentityRegistryEntry<'_>> =
        registry.entries.iter().map(elastos_reward_entry).collect();
    upsert_identity_registry(
        client,
        "Elastos reward registry",
        ELASTOS_REWARD_ADDRESS_NAMESPACE,
        &entries,
        false,
        "refusing to remap automatically; revoke the existing mapping explicitly",
        pool_ids_by_slug,
    )
    .await
    .context("seed Elastos reward address pool identities")
}

/// Build all child-side identity attributions for a block: reward-address rows from
/// the coinbase outputs plus minerinfo rows from the block tag / coinbase payload.
/// Each is promoted to a mapped pool only when a matching `pool_identity` exists.
pub(crate) fn resolve_elastos_identity_attributions(
    block: &ElastosBlock,
    identities: &PoolIdentityLookup,
) -> Vec<EventPoolAttribution> {
    let mut attributions = Vec::new();
    attributions.extend(reward_address_attributions(block, identities));
    attributions.extend(minerinfo_attributions(block, identities));
    attributions
}

/// Reward attributions from the coinbase (tx[0]) outputs: flatten each output's
/// addresses, drop empty/reserved ones, dedupe per address while collecting the
/// output indexes it appeared at, then resolve each against the identity lookup.
fn reward_address_attributions(
    block: &ElastosBlock,
    identities: &PoolIdentityLookup,
) -> Vec<EventPoolAttribution> {
    let mut rewards: Vec<RewardAddressObservation> = Vec::new();
    let Some(coinbase) = block.tx.first() else {
        return Vec::new();
    };

    for (position, vout) in coinbase.vout.iter().enumerate() {
        let output_index = vout.n.unwrap_or(position as i32);
        for raw_address in vout.decoded_addresses() {
            let address = raw_address.trim();
            if address.is_empty() || is_reserved_address(address) {
                continue;
            }
            match rewards.iter_mut().find(|reward| reward.address == address) {
                Some(existing) => {
                    if !existing.output_indexes.contains(&output_index) {
                        existing.output_indexes.push(output_index);
                    }
                }
                None => rewards.push(RewardAddressObservation {
                    address: address.to_owned(),
                    output_indexes: vec![output_index],
                }),
            }
        }
    }

    rewards
        .into_iter()
        .map(|reward| {
            let identity = lookup_identity(
                identities,
                ELASTOS_REWARD_ADDRESS_NAMESPACE,
                &reward.address,
            );
            elastos_identity_attribution(ElastosIdentityAttributionInput {
                namespace: ELASTOS_REWARD_ADDRESS_NAMESPACE,
                match_kind: "reward_address",
                matched_value: reward.address,
                unmapped_source: ELASTOS_RPC_REWARD_ADDRESS_SOURCE,
                identity,
                details: json!({
                    "address_source": "elastos_getblockbyheight_tx_vout",
                    "identity_authority": IDENTITY_AUTHORITY,
                    "output_indexes": reward.output_indexes,
                    "reserved_prefixes_excluded": RESERVED_ADDRESS_PREFIXES,
                }),
            })
        })
        .collect()
}

/// Minerinfo attributions from the two candidate sources (the block-level
/// `minerinfo` tag and the coinbase `payload.coinbasedata`), deduped by value with
/// the source field(s) recorded, then resolved against the identity lookup.
fn minerinfo_attributions(
    block: &ElastosBlock,
    identities: &PoolIdentityLookup,
) -> Vec<EventPoolAttribution> {
    let mut observations: Vec<MinerInfoObservation> = Vec::new();
    push_minerinfo(&mut observations, block.minerinfo.as_deref(), "minerinfo");
    if let Some(coinbase) = block.tx.first() {
        push_minerinfo(
            &mut observations,
            coinbase
                .payload
                .as_ref()
                .and_then(|payload| payload.coinbasedata.as_deref()),
            "tx[0].payload.coinbasedata",
        );
    }

    observations
        .into_iter()
        .map(|observation| {
            let identity =
                lookup_identity(identities, ELASTOS_MINERINFO_NAMESPACE, &observation.value);
            elastos_identity_attribution(ElastosIdentityAttributionInput {
                namespace: ELASTOS_MINERINFO_NAMESPACE,
                match_kind: "minerinfo",
                matched_value: observation.value,
                unmapped_source: ELASTOS_RPC_MINERINFO_SOURCE,
                identity,
                details: json!({
                    "identity_authority": IDENTITY_AUTHORITY,
                    "source_fields": observation.source_fields,
                }),
            })
        })
        .collect()
}

/// Fold one candidate minerinfo value into the observation list: trim, drop empty,
/// and either merge the source field into an existing matching value or push a new
/// observation. Keeps one row per distinct value with all contributing fields.
fn push_minerinfo(
    observations: &mut Vec<MinerInfoObservation>,
    raw_value: Option<&str>,
    source_field: &'static str,
) {
    let Some(raw_value) = raw_value else {
        return;
    };
    let value = raw_value.trim();
    if value.is_empty() {
        return;
    }
    match observations
        .iter_mut()
        .find(|observation| observation.value == value)
    {
        Some(existing) => {
            if !existing.source_fields.contains(&source_field) {
                existing.source_fields.push(source_field);
            }
        }
        None => observations.push(MinerInfoObservation {
            value: value.to_owned(),
            source_fields: vec![source_field],
        }),
    }
}

/// True for a protocol-reserved payout sink (matches any `RESERVED_ADDRESS_PREFIXES`
/// entry); such outputs are not miner rewards and are excluded.
fn is_reserved_address(address: &str) -> bool {
    RESERVED_ADDRESS_PREFIXES
        .iter()
        .any(|prefix| address.starts_with(prefix))
}

/// Look up a `(namespace, identifier)` in the preloaded identity map, returning the
/// pool ref when a `pool_identity` row maps it (drives the source/pool promotion).
fn lookup_identity(
    identities: &PoolIdentityLookup,
    namespace: &str,
    identifier: &str,
) -> Option<PoolIdentityRef> {
    identities
        .get(&pool_identity_lookup_key(namespace, identifier))
        .copied()
}

/// One distinct reward address and the coinbase output indexes it was seen at
/// (recorded in `details.output_indexes` for provenance).
struct RewardAddressObservation {
    address: String,
    output_indexes: Vec<i32>,
}

/// One distinct minerinfo value and which source field(s) carried it (recorded in
/// `details.source_fields`).
struct MinerInfoObservation {
    value: String,
    source_fields: Vec<&'static str>,
}

/// Builder inputs shared by both identity kinds, so [`elastos_identity_attribution`]
/// constructs the row uniformly: namespace/match_kind, the matched value, the
/// unmapped fallback source, the optional resolved identity, and the details JSON.
struct ElastosIdentityAttributionInput {
    namespace: &'static str,
    match_kind: &'static str,
    matched_value: String,
    unmapped_source: &'static str,
    identity: Option<PoolIdentityRef>,
    details: Value,
}

/// Assemble one child-block attribution. A resolved identity sets the pool ids and
/// promotes `source` to `CHILD_PAYOUT_REGISTRY_SOURCE`; an unresolved one keeps the
/// per-kind unmapped source. Confidence is always Medium (RPC-decoded, unverified).
fn elastos_identity_attribution(input: ElastosIdentityAttributionInput) -> EventPoolAttribution {
    let mapped = input.identity.is_some();
    EventPoolAttribution {
        side: PoolAttributionSide::ChildBlock,
        namespace: input.namespace,
        match_kind: input.match_kind,
        matched_value: input.matched_value,
        pool_id: input.identity.map(|identity| identity.pool_id),
        pool_identity_id: input.identity.map(|identity| identity.pool_identity_id),
        source: if mapped {
            CHILD_PAYOUT_REGISTRY_SOURCE
        } else {
            input.unmapped_source
        },
        confidence: PoolAttributionConfidence::Medium,
        details: input.details,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::elastos::rpc::ElastosBlock;
    use mmm_capture::attribution_policy::{ExistingAttributionSet, WritePolicy};
    use mmm_capture::capture::CHILD_PAYOUT_REGISTRY_SOURCE;
    use mmm_capture::child_payout::pool_identity_lookup_key;

    fn block(fixture: &str) -> ElastosBlock {
        serde_json::from_str(fixture).expect("deserialize Elastos fixture")
    }

    fn default_registry() -> ElastosMinerinfoRegistry {
        ElastosMinerinfoRegistry::from_json_str(DEFAULT_ELASTOS_MINERINFO_REGISTRY_JSON).unwrap()
    }

    #[test]
    fn loads_default_elastos_minerinfo_registry() {
        let registry = default_registry();
        assert_eq!(registry.schema_version, 1);
        assert_eq!(registry.entries.len(), 2);
    }

    #[test]
    fn loads_default_elastos_reward_registry() {
        let registry = ElastosRewardRegistry::from_json_str(DEFAULT_ELASTOS_REWARD_REGISTRY_JSON)
            .expect("embedded Elastos reward registry parses and validates");
        assert_eq!(registry.schema_version, 1);
        assert!(!registry.entries.is_empty());
        assert!(
            registry
                .entries
                .iter()
                .all(|entry| entry.address.starts_with('E'))
        );
    }

    #[test]
    fn byte_exact_minerinfo_values_survive_round_trip() {
        let registry = default_registry();
        let fish = registry
            .entries
            .iter()
            .find(|entry| entry.pool_slug == "f2pool")
            .expect("f2pool entry");
        assert_eq!(fish.minerinfo, "🐟");
        let viabtc = registry
            .entries
            .iter()
            .find(|entry| entry.pool_slug == "viabtc")
            .expect("viabtc entry");
        assert_eq!(viabtc.minerinfo, "Mined by ViaBTC");
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let err =
            ElastosMinerinfoRegistry::from_json_str(r#"{ "schema_version": 2, "entries": [] }"#)
                .unwrap_err();
        assert!(format!("{err:#}").contains("unsupported identity registry schema_version 2"));
    }

    #[test]
    fn rejects_empty_minerinfo() {
        let err = ElastosMinerinfoRegistry::from_json_str(
            r#"{ "schema_version": 1, "entries": [
                { "minerinfo": "", "pool_slug": "f2pool", "pool_canonical_name": "F2Pool" }
            ] }"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("minerinfo is empty"));
    }

    #[test]
    fn rejects_duplicate_minerinfo() {
        let err = ElastosMinerinfoRegistry::from_json_str(
            r#"{ "schema_version": 1, "entries": [
                { "minerinfo": "x", "pool_slug": "f2pool", "pool_canonical_name": "F2Pool" },
                { "minerinfo": "x", "pool_slug": "viabtc", "pool_canonical_name": "ViaBTC" }
            ] }"#,
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("duplicate identity registry minerinfo"));
    }

    /// Cross-registry completeness: every pool slug the Elastos minerinfo registry
    /// references must already exist in the embedded BTC snapshot with real
    /// attribution, or seeding it would create a bare stub that loses BTC
    /// attribution (the invariant the RSK registry test documents).
    #[test]
    fn default_registry_slugs_present_in_btc_snapshot_with_attribution() {
        use mmm_capture::pool_resolver::{PoolRecord, PoolResolver};

        let registry = default_registry();
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let btc_pools: std::collections::HashMap<&str, &PoolRecord> = resolver
            .snapshot()
            .pools
            .iter()
            .map(|pool| (pool.slug.as_str(), pool))
            .collect();

        for entry in &registry.entries {
            let pool = btc_pools.get(entry.pool_slug.as_str()).unwrap_or_else(|| {
                panic!(
                    "Elastos minerinfo registry references pool slug {:?} that is absent from \
                     data/pools/current.json",
                    entry.pool_slug
                )
            });
            assert!(
                !pool.coinbase_tags.is_empty() || !pool.payout_addresses.is_empty(),
                "shared pool {:?} has no BTC attribution",
                entry.pool_slug
            );
        }
    }

    #[test]
    fn fixture_shape_exposes_coinbase_vout_and_minerinfo_fields() {
        let b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-2232276.json"
        )));
        assert_eq!(b.tx.len(), 2);
        assert_eq!(
            b.tx[0].payload.as_ref().unwrap().coinbasedata.as_deref(),
            Some("")
        );
        assert_eq!(
            b.tx[0].vout[0].address.as_deref(),
            Some("CRASSETSXXXXXXXXXXXXXXXXXXXX2qDX5J")
        );
        assert_eq!(
            b.tx[0].vout[1].address.as_deref(),
            Some("EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR")
        );
        assert_eq!(b.minerinfo.as_deref(), Some(""));
    }

    #[test]
    fn reward_addresses_exclude_reserved_outputs() {
        let b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-2232276.json"
        )));
        let attributions = resolve_elastos_identity_attributions(&b, &PoolIdentityLookup::new());
        assert_eq!(attributions.len(), 1);
        let reward = &attributions[0];
        assert_eq!(reward.namespace, ELASTOS_REWARD_ADDRESS_NAMESPACE);
        assert_eq!(reward.matched_value, "EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR");
        assert_eq!(reward.source, ELASTOS_RPC_REWARD_ADDRESS_SOURCE);
        assert_eq!(reward.pool_id, None);
        assert_eq!(reward.details["output_indexes"], json!([1]));
    }

    #[test]
    fn minerinfo_deduplicates_top_level_and_coinbase_payload() {
        let mut b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-2232276.json"
        )));
        b.minerinfo = Some(" binance ".to_owned());
        b.tx[0].payload.as_mut().unwrap().coinbasedata = Some("binance".to_owned());

        let attributions = resolve_elastos_identity_attributions(&b, &PoolIdentityLookup::new());
        let minerinfo = attributions
            .iter()
            .find(|attribution| attribution.namespace == ELASTOS_MINERINFO_NAMESPACE)
            .expect("minerinfo attribution");
        assert_eq!(minerinfo.matched_value, "binance");
        assert_eq!(minerinfo.source, ELASTOS_RPC_MINERINFO_SOURCE);
        assert_eq!(
            minerinfo.details["source_fields"],
            json!(["minerinfo", "tx[0].payload.coinbasedata"])
        );
    }

    #[test]
    fn known_reward_and_minerinfo_identities_map_to_pool_identity() {
        let mut b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-2232276.json"
        )));
        b.minerinfo = Some("binance".to_owned());

        let mut identities = PoolIdentityLookup::new();
        identities.insert(
            pool_identity_lookup_key(
                ELASTOS_REWARD_ADDRESS_NAMESPACE,
                "EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR",
            ),
            PoolIdentityRef {
                pool_id: 42,
                pool_identity_id: 4200,
            },
        );
        identities.insert(
            pool_identity_lookup_key(ELASTOS_MINERINFO_NAMESPACE, "binance"),
            PoolIdentityRef {
                pool_id: 43,
                pool_identity_id: 4300,
            },
        );

        let attributions = resolve_elastos_identity_attributions(&b, &identities);
        let reward = attributions
            .iter()
            .find(|attribution| attribution.namespace == ELASTOS_REWARD_ADDRESS_NAMESPACE)
            .unwrap();
        assert_eq!(reward.source, CHILD_PAYOUT_REGISTRY_SOURCE);
        assert_eq!(reward.pool_id, Some(42));
        assert_eq!(reward.pool_identity_id, Some(4200));

        let minerinfo = attributions
            .iter()
            .find(|attribution| attribution.namespace == ELASTOS_MINERINFO_NAMESPACE)
            .unwrap();
        assert_eq!(minerinfo.source, CHILD_PAYOUT_REGISTRY_SOURCE);
        assert_eq!(minerinfo.pool_id, Some(43));
        assert_eq!(minerinfo.pool_identity_id, Some(4300));
    }

    #[test]
    fn existing_identity_filter_preserves_resolved_rows() {
        let unresolved = elastos_identity_attribution(ElastosIdentityAttributionInput {
            namespace: ELASTOS_REWARD_ADDRESS_NAMESPACE,
            match_kind: "reward_address",
            matched_value: "EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR".to_owned(),
            unmapped_source: ELASTOS_RPC_REWARD_ADDRESS_SOURCE,
            identity: None,
            details: json!({}),
        });
        let resolved = elastos_identity_attribution(ElastosIdentityAttributionInput {
            namespace: ELASTOS_REWARD_ADDRESS_NAMESPACE,
            match_kind: "reward_address",
            matched_value: "EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR".to_owned(),
            unmapped_source: ELASTOS_RPC_REWARD_ADDRESS_SOURCE,
            identity: Some(PoolIdentityRef {
                pool_id: 1,
                pool_identity_id: 2,
            }),
            details: json!({}),
        });
        let remapped = elastos_identity_attribution(ElastosIdentityAttributionInput {
            namespace: ELASTOS_REWARD_ADDRESS_NAMESPACE,
            match_kind: "reward_address",
            matched_value: "EXm7Gqs1bS4ddry8EUrN7KZHF7oax79upR".to_owned(),
            unmapped_source: ELASTOS_RPC_REWARD_ADDRESS_SOURCE,
            identity: Some(PoolIdentityRef {
                pool_id: 99,
                pool_identity_id: 100,
            }),
            details: json!({}),
        });
        let existing = ExistingAttributionSet::from_json(&json!([{
            "source": resolved.source,
            "namespace": resolved.namespace,
            "match_kind": resolved.match_kind,
            "matched_value": resolved.matched_value,
            "pool_id": resolved.pool_id,
            "pool_identity_id": resolved.pool_identity_id,
            "confidence": resolved.confidence.as_db_str(),
            "details": resolved.details,
        }]));

        assert!(!existing.should_write(&unresolved, WritePolicy::IdentityPromoteOnly));
        assert!(!existing.should_write(&resolved, WritePolicy::IdentityPromoteOnly));
        assert!(!existing.should_write(&remapped, WritePolicy::IdentityPromoteOnly));
    }
}
