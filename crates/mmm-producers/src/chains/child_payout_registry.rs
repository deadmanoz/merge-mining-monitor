//! Child-payout-address identity registries (Namecoin, Syscoin, Fractal, Qbit).
//!
//! These chains attribute the child miner by formatting the
//! child coinbase outputs as chain-native addresses (see
//! [`mmm_capture::child_payout`]) and matching them byte-for-byte against a
//! curated address->pool registry. This module embeds the registry JSON
//! files, validates them through the shared identity-registry validator, and seeds the
//! `pool_identity` rows so capture and `reclassify-pools` resolve them.
//! A new child-payout chain is a row in [`embedded_registries`],
//! never a cloned module.
//!
//! The address validator is keyed off each chain's [`ChildPayoutParams`] - the
//! same version bytes / bech32 HRP the formatter encodes with - so it cannot drift
//! from what capture emits. It is fully checksum-validated: a base58 address must
//! base58check-decode to a version + 20-byte hash under the chain's P2PKH or P2SH
//! version; a witness address must segwit-decode (bech32/bech32m checksum) under the
//! chain's HRP. So a curation typo fails loudly at seed time instead of becoming a
//! mapping that silently never matches `child_output_addresses` at resolve time.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bitcoin::base58;
use bitcoin::bech32::segwit;
use serde::Deserialize;
use tokio_postgres::Client;

use mmm_capture::child_payout::{
    ChildPayoutConfidence, ChildPayoutParams, FRACTAL_CHILD_REWARD_PARAMS,
    FRACTAL_REWARD_ADDRESS_NAMESPACE, NAMECOIN_CHILD_PAYOUT_PARAMS,
    NAMECOIN_PAYOUT_ADDRESS_NAMESPACE, PoolIdentityLookup, QBIT_CHILD_PAYOUT_PARAMS,
    QBIT_PAYOUT_ADDRESS_NAMESPACE, SYSCOIN_CHILD_PAYOUT_PARAMS, SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
    pool_identity_lookup_key,
};
use mmm_capture::identity_registry::{
    IdentityRegistryEntry, identity_key, validate_identity_registry,
};
use mmm_store::upsert_identity_registry;

const NAMECOIN_PAYOUT_REGISTRY_JSON: &str =
    include_str!("../../../../data/pools/child-identities/namecoin_payout_address_registry.json");
const SYSCOIN_PAYOUT_REGISTRY_JSON: &str =
    include_str!("../../../../data/pools/child-identities/syscoin_payout_address_registry.json");
const FRACTAL_REWARD_REGISTRY_JSON: &str =
    include_str!("../../../../data/pools/child-identities/fractal_reward_address_registry.json");
const QBIT_PAYOUT_REGISTRY_JSON: &str =
    include_str!("../../../../data/pools/child-identities/qbit_payout_address_registry.json");

/// A child-payout registry: schema version plus address->pool entries. The
/// per-entry `evidence` block in the JSON is documentation only and is ignored
/// here (serde skips unknown fields).
#[derive(Debug, Clone, Deserialize)]
struct ChildPayoutRegistry {
    schema_version: u32,
    entries: Vec<ChildPayoutEntry>,
}

/// One address->pool mapping. `address` is the chain-native payout/reward address
/// exactly as [`mmm_capture::child_payout::child_output_addresses`] formats it.
#[derive(Debug, Clone, Deserialize)]
struct ChildPayoutEntry {
    address: String,
    pool_slug: String,
    pool_canonical_name: String,
    #[serde(default)]
    confidence: ChildPayoutConfidence,
}

/// The `(namespace, embedded JSON)` table. Adding a child-payout chain is a row
/// here, paired with a [`ChildPayoutParams`](mmm_capture::child_payout::ChildPayoutParams)
/// in the chain spec.
fn embedded_registries() -> [(&'static str, &'static str); 4] {
    [
        (
            NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
            NAMECOIN_PAYOUT_REGISTRY_JSON,
        ),
        (
            SYSCOIN_PAYOUT_ADDRESS_NAMESPACE,
            SYSCOIN_PAYOUT_REGISTRY_JSON,
        ),
        (
            FRACTAL_REWARD_ADDRESS_NAMESPACE,
            FRACTAL_REWARD_REGISTRY_JSON,
        ),
        (QBIT_PAYOUT_ADDRESS_NAMESPACE, QBIT_PAYOUT_REGISTRY_JSON),
    ]
}

/// Seed every embedded child-payout registry. Used by `reclassify-pools`.
/// Idempotent; the same mappings are used during live bootstrap.
pub(crate) async fn seed_all_child_payout_identities(
    client: &Client,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<()> {
    for (namespace, json) in embedded_registries() {
        seed_namespace(client, namespace, json, pool_ids_by_slug).await?;
    }
    Ok(())
}

/// Apply curated confidence to identities already loaded from the database.
/// Metadata never creates an identity or overrides a different pool mapping.
/// Shared by live bootstrap and offline replay; no additional database work.
pub(crate) fn apply_child_payout_confidence(
    identities: &mut PoolIdentityLookup,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> Result<()> {
    for (namespace, json) in embedded_registries() {
        let registry: ChildPayoutRegistry = serde_json::from_str(json)
            .with_context(|| format!("parse {namespace} payout confidence"))?;
        for entry in registry.entries {
            if let Some(identity) =
                identities.get_mut(&pool_identity_lookup_key(namespace, &entry.address))
                && pool_ids_by_slug.get(&entry.pool_slug) == Some(&identity.pool_id)
            {
                identity.payout_confidence = entry.confidence;
            }
        }
    }
    Ok(())
}

/// Seed exactly one child-payout namespace's registry. Used at each chain's
/// capture bootstrap, where only that chain's namespace is needed. Idempotent.
/// Errors if `namespace` is not a known child-payout namespace.
pub(crate) async fn seed_child_payout_identities_for(
    client: &Client,
    namespace: &str,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<()> {
    let json = embedded_registries()
        .into_iter()
        .find(|(ns, _)| *ns == namespace)
        .map(|(_, json)| json)
        .with_context(|| format!("no embedded child-payout registry for namespace {namespace}"))?;
    seed_namespace(client, namespace, json, pool_ids_by_slug).await
}

/// Parse + validate one registry JSON, then upsert its pools and address
/// identities (idempotent, non-remapping).
async fn seed_namespace(
    client: &Client,
    namespace: &str,
    json: &str,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<()> {
    let registry: ChildPayoutRegistry =
        serde_json::from_str(json).with_context(|| format!("parse {namespace} registry JSON"))?;
    let entries: Vec<IdentityRegistryEntry<'_>> =
        registry.entries.iter().map(child_payout_entry).collect();
    validate_identity_registry(
        registry.schema_version,
        entries.iter().copied(),
        "address",
        |address| validate_child_payout_address(namespace, address),
        identity_key,
    )
    .map_err(|err| anyhow::anyhow!("{namespace} registry: {err}"))?;
    upsert_identity_registry(
        client,
        namespace,
        namespace,
        &entries,
        false,
        "refusing to remap automatically; revoke the existing mapping explicitly",
        pool_ids_by_slug,
    )
    .await
    .with_context(|| format!("seed {namespace} pool identities"))?;
    Ok(())
}

/// View a [`ChildPayoutEntry`] as a generic [`IdentityRegistryEntry`].
fn child_payout_entry(entry: &ChildPayoutEntry) -> IdentityRegistryEntry<'_> {
    IdentityRegistryEntry {
        identifier: &entry.address,
        pool_slug: &entry.pool_slug,
        pool_canonical_name: &entry.pool_canonical_name,
    }
}

/// base58check decode length for a standard P2PKH/P2SH address: 1 version byte +
/// 20-byte hash.
const BASE58_DECODED_LEN: usize = 21;

/// Well-formedness gate for a child-payout address, keyed off the chain's
/// [`ChildPayoutParams`] and fully checksum-validated so a curation typo fails
/// loudly at seed time rather than becoming a silent never-matches mapping. A
/// witness address (`<hrp>1...`) must segwit-decode (bech32/bech32m checksum +
/// witness program) under the chain's HRP; a legacy address must base58check-decode
/// to 21 bytes whose version is the chain's P2PKH or P2SH version. Decoding rather
/// than matching first characters accepts every valid form - e.g. a Namecoin P2SH
/// address renders `6...`, which a prefix list would have wrongly rejected.
fn validate_child_payout_address(namespace: &str, address: &str) -> Result<(), String> {
    let params = params_for_namespace(namespace)
        .ok_or_else(|| format!("unknown child-payout namespace {namespace}"))?;
    // Witness address: the chain's HRP plus the bech32 separator. No base58 P2PKH/
    // P2SH form for these chains begins with the HRP, so this split is unambiguous.
    if address.starts_with(params.bech32_hrp) && address[params.bech32_hrp.len()..].starts_with('1')
    {
        return match segwit::decode(address) {
            Ok((hrp, version, program))
                if hrp.as_str() == params.bech32_hrp
                    && params.accepts_witness_program(version.to_u8(), program.len()) =>
            {
                Ok(())
            }
            Ok((hrp, version, program)) if hrp.as_str() == params.bech32_hrp => Err(format!(
                "address {address:?} witness version {} and program length {} are unsupported for {namespace}",
                version.to_u8(),
                program.len()
            )),
            Ok((hrp, _, _)) => Err(format!(
                "address {address:?} bech32 HRP {} is not {namespace}'s {}",
                hrp.as_str(),
                params.bech32_hrp
            )),
            Err(err) => Err(format!(
                "address {address:?} is not a valid bech32 address: {err}"
            )),
        };
    }
    match base58::decode_check(address) {
        Ok(decoded)
            if decoded.len() == BASE58_DECODED_LEN
                && (Some(decoded[0]) == params.p2pkh_version
                    || Some(decoded[0]) == params.p2sh_version) =>
        {
            Ok(())
        }
        Ok(decoded) => Err(format!(
            "address {address:?} base58 version {} is not a {namespace} P2PKH/P2SH address",
            decoded.first().copied().unwrap_or_default()
        )),
        Err(err) => Err(format!(
            "address {address:?} is not a valid {namespace} address: {err}"
        )),
    }
}

/// Map a child-payout namespace to its [`ChildPayoutParams`] (version bytes + HRP),
/// the single source of address-format truth shared with the formatter.
fn params_for_namespace(namespace: &str) -> Option<ChildPayoutParams> {
    [
        NAMECOIN_CHILD_PAYOUT_PARAMS,
        SYSCOIN_CHILD_PAYOUT_PARAMS,
        FRACTAL_CHILD_REWARD_PARAMS,
        QBIT_CHILD_PAYOUT_PARAMS,
    ]
    .into_iter()
    .find(|params| params.namespace == namespace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::bech32::{Fe32, Hrp};
    use mmm_capture::child_payout::PoolIdentityRef;

    fn parse(json: &str) -> ChildPayoutRegistry {
        serde_json::from_str(json).expect("registry parses")
    }

    #[test]
    fn qbit_seed_validation_rejects_valid_checksums_with_wrong_witness_policy() {
        for (hrp, version, length) in [
            ("qb", Fe32::P, 32),
            ("qb", Fe32::Z, 20),
            ("bc", Fe32::Z, 32),
        ] {
            let address =
                segwit::encode(Hrp::parse(hrp).unwrap(), version, &vec![0; length]).unwrap();
            assert!(
                validate_child_payout_address(QBIT_PAYOUT_ADDRESS_NAMESPACE, &address).is_err()
            );
        }
        assert!(
            validate_child_payout_address(
                QBIT_PAYOUT_ADDRESS_NAMESPACE,
                "qb1zpyh3ygrlnrdzk5nrz0wh9usmeex2tfanq89p5yyvqj3pj4tuzlgs4hc5uq"
            )
            .is_err()
        );
        assert!(
            validate_child_payout_address(
                QBIT_PAYOUT_ADDRESS_NAMESPACE,
                "MvaNCeVyvP6ZXYFWGpKaDX9ujEQ418F7sm"
            )
            .is_err()
        );
    }

    #[test]
    fn payout_confidence_defaults_and_values_are_validated() {
        let base = r#"{"address":"address", "pool_slug":"pool", "pool_canonical_name":"Pool"}"#;
        let entry: ChildPayoutEntry = serde_json::from_str(base).unwrap();
        assert_eq!(entry.confidence, ChildPayoutConfidence::Medium);
        for invalid in ["low", "HIGH", "certain"] {
            let mut value: serde_json::Value = serde_json::from_str(base).unwrap();
            value["confidence"] = invalid.into();
            assert!(serde_json::from_value::<ChildPayoutEntry>(value).is_err());
        }
    }

    #[test]
    fn curated_confidence_only_decorates_matching_loaded_identities() {
        let registry = parse(QBIT_PAYOUT_REGISTRY_JSON);
        let pools = HashMap::from([
            ("f2pool".to_owned(), 1),
            ("secpool".to_owned(), 2),
            ("luxor".to_owned(), 3),
        ]);
        let mut identities = PoolIdentityLookup::new();
        for (entry, pool_id) in registry.entries.iter().zip([1, 2, 999]) {
            identities.insert(
                pool_identity_lookup_key(QBIT_PAYOUT_ADDRESS_NAMESPACE, &entry.address),
                PoolIdentityRef {
                    pool_id,
                    pool_identity_id: pool_id + 1000,
                    payout_confidence: Default::default(),
                },
            );
        }
        apply_child_payout_confidence(&mut identities, &pools).unwrap();
        for (entry, expected) in registry.entries.iter().zip([
            ChildPayoutConfidence::High,
            ChildPayoutConfidence::Medium,
            ChildPayoutConfidence::Medium,
        ]) {
            let identity = identities
                [&pool_identity_lookup_key(QBIT_PAYOUT_ADDRESS_NAMESPACE, &entry.address)];
            assert_eq!(identity.payout_confidence, expected);
        }
        let luxor = &registry.entries[2];
        assert_eq!(
            identities[&pool_identity_lookup_key(QBIT_PAYOUT_ADDRESS_NAMESPACE, &luxor.address)]
                .pool_id,
            999
        );
        // A mismatched high-confidence entry must not upgrade an independently
        // mapped identity. Using a medium entry would not detect a missing guard.
        let f2pool_key =
            pool_identity_lookup_key(QBIT_PAYOUT_ADDRESS_NAMESPACE, &registry.entries[0].address);
        let f2pool = identities.get_mut(&f2pool_key).unwrap();
        f2pool.pool_id = 999;
        f2pool.payout_confidence = ChildPayoutConfidence::Medium;
        apply_child_payout_confidence(&mut identities, &pools).unwrap();
        assert_eq!(
            identities[&f2pool_key].payout_confidence,
            ChildPayoutConfidence::Medium
        );
        assert_eq!(identities[&f2pool_key].pool_id, 999);
        let mut absent = PoolIdentityLookup::new();
        apply_child_payout_confidence(&mut absent, &pools).unwrap();
        assert!(absent.is_empty());
    }

    #[test]
    fn embedded_registries_parse_validate_and_are_well_formed() {
        for (namespace, json) in embedded_registries() {
            let registry = parse(json);
            assert_eq!(registry.schema_version, 1, "{namespace} schema_version");
            assert!(!registry.entries.is_empty(), "{namespace} has entries");
            let entries: Vec<IdentityRegistryEntry<'_>> =
                registry.entries.iter().map(child_payout_entry).collect();
            // Full shared validation: uniqueness, slug/name consistency, address decode.
            validate_identity_registry(
                registry.schema_version,
                entries.iter().copied(),
                "address",
                |address| validate_child_payout_address(namespace, address),
                identity_key,
            )
            .unwrap_or_else(|err| panic!("{namespace} registry invalid: {err}"));
        }
    }

    #[test]
    fn accepts_every_valid_form_including_namecoin_p2sh() {
        // Namecoin P2PKH (version 52, `N...`) and the bech32 witness form.
        assert!(
            validate_child_payout_address(
                NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
                "N4HYFWGfVV5baz1vJQ9BueaeP4g5gipKKG"
            )
            .is_ok()
        );
        assert!(
            validate_child_payout_address(
                NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
                "nc1qkvg5aakqn9a2lqa3p4d9cnasxfczc555lxngzu"
            )
            .is_ok()
        );
        // Namecoin P2SH (version 13) renders with a `6...` prefix - the case a
        // first-character prefix list wrongly rejected.
        assert!(
            validate_child_payout_address(
                NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
                "6Fx5jVAN9MirDjMbikRJGe2M6Fy4SEUPeC"
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_wrong_chain_address() {
        // A real Fractal/Bitcoin bech32 address must not validate under Namecoin.
        assert!(
            validate_child_payout_address(
                NAMECOIN_PAYOUT_ADDRESS_NAMESPACE,
                "bc1qptpxl288ng4c7mg6klzu9t0are7nhqlfmtmk9k"
            )
            .is_err()
        );
        // A real Namecoin P2PKH address must not validate under Fractal (its
        // version 52 is neither Fractal's P2PKH 0 nor P2SH 5).
        assert!(
            validate_child_payout_address(
                FRACTAL_REWARD_ADDRESS_NAMESPACE,
                "N4HYFWGfVV5baz1vJQ9BueaeP4g5gipKKG"
            )
            .is_err()
        );
        // Corrupt base58 (bad checksum) is rejected.
        assert!(
            validate_child_payout_address(NAMECOIN_PAYOUT_ADDRESS_NAMESPACE, "N4HYFexample")
                .is_err()
        );
    }

    #[test]
    fn rejects_corrupt_witness_address() {
        // Right HRP prefix but not a valid bech32 string - the silent-miss typo case.
        assert!(
            validate_child_payout_address(FRACTAL_REWARD_ADDRESS_NAMESPACE, "bc1notarealaddress")
                .is_err()
        );
        // Right HRP but a flipped character breaks the checksum.
        assert!(
            validate_child_payout_address(
                FRACTAL_REWARD_ADDRESS_NAMESPACE,
                "bc1qptpxl288ng4c7mg6klzu9t0are7nhqlfmtmk9q"
            )
            .is_err()
        );
    }
}
