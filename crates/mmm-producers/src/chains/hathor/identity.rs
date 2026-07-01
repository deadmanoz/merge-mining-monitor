//! Hathor reward-address identity registry.
//!
//! Hathor rewards are decoded from the block funds graph, but a reward address
//! is only pool-resolved when this embedded registry maps it to a stable pool
//! slug. The registry is deliberately conservative: local same-event BTC parent
//! attribution is only a candidate generator, not sufficient evidence by itself.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bitcoin::base58;
use serde::Deserialize;
use tokio_postgres::Client;

use crate::chains::hathor::address::{MAINNET_P2PKH_VERSION, MAINNET_P2SH_VERSION};
use crate::chains::hathor::reward::HATHOR_REWARD_ADDRESS_NAMESPACE;
#[cfg(test)]
use mmm_capture::identity_registry::distinct_pool_definitions;
use mmm_capture::identity_registry::{
    IdentityRegistryEntry, IdentityRegistryError, identity_key, validate_identity_registry,
};
use mmm_store::upsert_identity_registry;

/// The embedded reward-address registry, `include_str!`-baked from
/// `data/pools/child-identities/`. Edit the JSON and rebuild to change the mappings.
const DEFAULT_HATHOR_REWARD_REGISTRY_JSON: &str =
    include_str!("../../../../../data/pools/child-identities/hathor_reward_registry.json");

/// Decoded length of a valid Hathor address: 1 version byte + 20-byte hash.
const HATHOR_ADDRESS_DECODED_LEN: usize = 21;

/// The validated reward-address-to-pool registry.
#[derive(Debug, Clone, Deserialize)]
struct HathorRewardRegistry {
    /// Registry schema version; only `1` is accepted.
    schema_version: u32,
    entries: Vec<HathorRewardEntry>,
}

/// One reward-address-to-pool mapping. Addresses are unique across the registry;
/// a slug maps to exactly one canonical name.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct HathorRewardEntry {
    reward_address: String,
    pool_slug: String,
    pool_canonical_name: String,
}

impl HathorRewardRegistry {
    /// Parse and validate the embedded registry.
    fn from_default() -> Result<Self> {
        Self::from_json_str(DEFAULT_HATHOR_REWARD_REGISTRY_JSON)
    }

    /// Parse and fully validate registry JSON (schema version, address format,
    /// duplicate-address and slug/canonical-name consistency); any violation
    /// errors rather than seeding a malformed registry.
    fn from_json_str(json: &str) -> Result<Self> {
        let registry: Self =
            serde_json::from_str(json).context("parse Hathor reward registry JSON")?;
        validate_hathor_reward_registry(&registry)?;
        Ok(registry)
    }
}

/// Seed the registry's pools and reward-address identities (idempotent upserts),
/// extending `pool_ids_by_slug` with any newly-created slugs and returning the
/// namespace's resolved identity ids. Called once at capture bootstrap.
pub(crate) async fn upsert_hathor_reward_pool_identities(
    client: &Client,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<HashMap<String, i64>> {
    let registry = HathorRewardRegistry::from_default().context("load Hathor reward registry")?;
    let entries: Vec<IdentityRegistryEntry<'_>> =
        registry.entries.iter().map(hathor_registry_entry).collect();
    upsert_identity_registry(
        client,
        "Hathor reward registry",
        HATHOR_REWARD_ADDRESS_NAMESPACE,
        &entries,
        false,
        "refusing to remap automatically",
        pool_ids_by_slug,
    )
    .await
    .context("seed Hathor reward pool identities")
}

/// View a [`HathorRewardEntry`] as a generic [`IdentityRegistryEntry`] for the
/// shared validation/distinct-pool helpers; keeps the `reward_address` field name.
fn hathor_registry_entry(entry: &HathorRewardEntry) -> IdentityRegistryEntry<'_> {
    IdentityRegistryEntry {
        identifier: &entry.reward_address,
        pool_slug: &entry.pool_slug,
        pool_canonical_name: &entry.pool_canonical_name,
    }
}

/// Reject a registry that would silently misattribute via the shared
/// [`validate_identity_registry`] validator (unsupported schema, empty/whitespaced
/// fields, malformed address, a reward address mapped to two pools, or a slug with
/// conflicting canonical names), with the failures re-phrased in Hathor terms.
fn validate_hathor_reward_registry(registry: &HathorRewardRegistry) -> Result<()> {
    validate_identity_registry(
        registry.schema_version,
        registry.entries.iter().map(hathor_registry_entry),
        "reward_address",
        validate_hathor_address,
        identity_key,
    )
    .map_err(map_hathor_registry_error)
}

/// Hathor identifier format check (reason on failure): a Hathor mainnet address is
/// base58check-decodable to 21 bytes with a recognized P2PKH/P2SH version byte.
fn validate_hathor_address(address: &str) -> Result<(), String> {
    let decoded =
        base58::decode_check(address).map_err(|err| format!("not base58check-decodable: {err}"))?;
    if decoded.len() != HATHOR_ADDRESS_DECODED_LEN {
        return Err(format!(
            "decoded length {}, expected {HATHOR_ADDRESS_DECODED_LEN}",
            decoded.len()
        ));
    }
    let version = decoded[0];
    if version != MAINNET_P2PKH_VERSION && version != MAINNET_P2SH_VERSION {
        return Err(format!("unsupported version byte {version:#04x}"));
    }
    Ok(())
}

/// Re-phrase a shared [`IdentityRegistryError`] in Hathor terms so existing
/// callers and tests keep their messages.
fn map_hathor_registry_error(error: IdentityRegistryError) -> anyhow::Error {
    match error {
        IdentityRegistryError::UnsupportedSchemaVersion(version) => {
            anyhow::anyhow!("unsupported Hathor reward registry schema_version {version}")
        }
        IdentityRegistryError::EmptyField { field, pool_slug } => {
            anyhow::anyhow!("Hathor reward registry {field} is empty for pool {pool_slug}")
        }
        IdentityRegistryError::WhitespaceField { field, pool_slug } => anyhow::anyhow!(
            "Hathor reward registry {field} has surrounding whitespace for pool {pool_slug}"
        ),
        IdentityRegistryError::DuplicateIdentifier {
            value,
            first_pool,
            duplicate_pool,
            ..
        } => anyhow::anyhow!(
            "duplicate Hathor reward address {value} first mapped to {first_pool}, \
             duplicate mapped to {duplicate_pool}"
        ),
        IdentityRegistryError::SlugCanonicalNameConflict {
            slug,
            first_canonical_name,
            duplicate_canonical_name,
        } => anyhow::anyhow!(
            "Hathor reward registry slug {slug} has conflicting canonical names \
             {first_canonical_name} and {duplicate_canonical_name}"
        ),
        IdentityRegistryError::InvalidIdentifier {
            value,
            pool_slug,
            reason,
            ..
        } => anyhow::anyhow!("Hathor reward address {value} for pool {pool_slug}: {reason}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_default_hathor_reward_registry() {
        let registry = HathorRewardRegistry::from_default().unwrap();

        assert_eq!(registry.schema_version, 1);
        assert!(registry.entries.len() >= 11);
        assert!(
            registry
                .entries
                .iter()
                .any(|entry| entry.reward_address == "HH5As5aLtzFkcbmbXZmE65wSd22GqPWq2T")
        );
    }

    #[test]
    fn distinct_pool_definitions_preserves_first_seen_order() {
        let registry = HathorRewardRegistry::from_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "reward_address": "HH5As5aLtzFkcbmbXZmE65wSd22GqPWq2T",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    },
                    {
                        "reward_address": "HV3iKMJpuZpktXwpoBxKEUetG6NS3zfXje",
                        "pool_slug": "poolin",
                        "pool_canonical_name": "Poolin"
                    },
                    {
                        "reward_address": "HHDXRkSZorcWkZ9sHhSM6bWA9W9ozj8uxe",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    }
                ]
            }"#,
        )
        .unwrap();

        assert_eq!(
            distinct_pool_definitions(registry.entries.iter().map(hathor_registry_entry)),
            vec![("f2pool", "F2Pool"), ("poolin", "Poolin")]
        );
    }

    #[test]
    fn rejects_duplicate_reward_addresses() {
        let err = HathorRewardRegistry::from_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "reward_address": "HH5As5aLtzFkcbmbXZmE65wSd22GqPWq2T",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    },
                    {
                        "reward_address": "HH5As5aLtzFkcbmbXZmE65wSd22GqPWq2T",
                        "pool_slug": "poolin",
                        "pool_canonical_name": "Poolin"
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("duplicate Hathor reward address"));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let err = HathorRewardRegistry::from_json_str(
            r#"{
                "schema_version": 2,
                "entries": []
            }"#,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("unsupported Hathor reward registry schema_version 2"));
    }

    #[test]
    fn rejects_empty_and_whitespace_fields() {
        let empty = HathorRewardRegistry::from_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "reward_address": "",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    }
                ]
            }"#,
        )
        .unwrap_err();
        assert!(format!("{empty:#}").contains("reward_address is empty"));

        let whitespace = HathorRewardRegistry::from_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "reward_address": "HH5As5aLtzFkcbmbXZmE65wSd22GqPWq2T",
                        "pool_slug": " f2pool",
                        "pool_canonical_name": "F2Pool"
                    }
                ]
            }"#,
        )
        .unwrap_err();
        assert!(format!("{whitespace:#}").contains("pool_slug has surrounding whitespace"));
    }

    #[test]
    fn rejects_wrong_decoded_address_length() {
        let mut decoded = vec![MAINNET_P2PKH_VERSION];
        decoded.extend_from_slice(&[0_u8; 19]);
        let short_address = bitcoin::base58::encode_check(&decoded);
        let json = format!(
            r#"{{
                "schema_version": 1,
                "entries": [
                    {{
                        "reward_address": "{short_address}",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    }}
                ]
            }}"#
        );

        let err = HathorRewardRegistry::from_json_str(&json).unwrap_err();

        assert!(format!("{err:#}").contains("decoded length 20, expected 21"));
    }

    #[test]
    fn rejects_non_hathor_base58_address() {
        let err = HathorRewardRegistry::from_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "reward_address": "1KFHE7w8BhaENAswwryaoccDb6qcT6DbYY",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("unsupported version byte"));
    }

    #[test]
    fn rejects_slug_canonical_name_conflicts() {
        let err = HathorRewardRegistry::from_json_str(
            r#"{
                "schema_version": 1,
                "entries": [
                    {
                        "reward_address": "HH5As5aLtzFkcbmbXZmE65wSd22GqPWq2T",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "F2Pool"
                    },
                    {
                        "reward_address": "HV3iKMJpuZpktXwpoBxKEUetG6NS3zfXje",
                        "pool_slug": "f2pool",
                        "pool_canonical_name": "Discus Fish"
                    }
                ]
            }"#,
        )
        .unwrap_err();

        assert!(format!("{err:#}").contains("conflicting canonical names"));
    }
}
