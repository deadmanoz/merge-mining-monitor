//! Embedded protocol-account association registries: declared CORE reward addresses and exSat
//! accounts mapped to the pools whose blocks declare them, each with dated evidence kept in the
//! registry file. Seeding makes each namespace equal its registry, so removing an entry unmaps it.
//! An association says which pool's blocks carry a declaration, never who controls the account.

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use tokio_postgres::Client;

use mmm_capture::identity_registry::{
    IdentityRegistryEntry, identity_key, parse_registry_date, validate_identity_registry,
};
use mmm_capture::protocol_commitment::{
    CORE_REWARD_ADDRESS_NAMESPACE, EXSAT_ACCOUNT_NAMESPACE, is_exsat_account,
};
use mmm_store::reconcile_identity_registry;

const CORE_REWARD_ADDRESS_REGISTRY_JSON: &str =
    include_str!("../../../data/pools/protocol-accounts/core_reward_address_registry.json");
const EXSAT_ACCOUNT_REGISTRY_JSON: &str =
    include_str!("../../../data/pools/protocol-accounts/exsat_account_registry.json");

/// The observation rule for an empirical association (`docs/attribution.md`).
const MIN_CARRYING_BLOCKS: u32 = 20;
const MIN_POOL_SHARE_PERCENT: u32 = 90;

#[derive(Debug, Deserialize)]
struct ProtocolAccountRegistry {
    schema_version: u32,
    /// When the empirical counts were taken.
    generated_on: String,
    entries: Vec<ProtocolAccountEntry>,
}

#[derive(Debug, Deserialize)]
struct ProtocolAccountEntry {
    identifier: String,
    pool_slug: String,
    pool_canonical_name: String,
    evidence: AssociationEvidence,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "basis", rename_all = "snake_case")]
enum AssociationEvidence {
    /// The protocol's own listing of the account for the pool.
    FirstPartyRegistration { source: String, recorded_on: String },
    /// Core-attested canonical blocks carrying the identifier, as of the registry's
    /// `generated_on`.
    EmpiricalAssociation {
        carrying_blocks: u32,
        /// Carrying blocks whose miner resolves to a pool.
        resolved_blocks: u32,
        /// Resolved blocks whose miner is the mapped pool.
        pool_blocks: u32,
        first_height: u32,
        last_height: u32,
    },
}

type IdentifierValidator = fn(&str) -> Result<(), String>;

fn embedded_registries() -> [(&'static str, &'static str, IdentifierValidator); 2] {
    [
        (
            CORE_REWARD_ADDRESS_NAMESPACE,
            CORE_REWARD_ADDRESS_REGISTRY_JSON,
            validate_core_reward_address,
        ),
        (
            EXSAT_ACCOUNT_NAMESPACE,
            EXSAT_ACCOUNT_REGISTRY_JSON,
            validate_exsat_account,
        ),
    ]
}

pub async fn seed_protocol_account_identities(
    client: &Client,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<()> {
    for (namespace, json, validator) in embedded_registries() {
        let registry = parse_registry(namespace, json, validator)?;
        let entries: Vec<IdentityRegistryEntry<'_>> =
            registry.entries.iter().map(registry_entry).collect();
        reconcile_identity_registry(client, namespace, &entries, pool_ids_by_slug)
            .await
            .with_context(|| format!("seed {namespace} pool identities"))?;
    }
    Ok(())
}

fn parse_registry(
    namespace: &str,
    json: &str,
    validator: IdentifierValidator,
) -> Result<ProtocolAccountRegistry> {
    let registry: ProtocolAccountRegistry =
        serde_json::from_str(json).with_context(|| format!("parse {namespace} registry JSON"))?;
    validate_identity_registry(
        registry.schema_version,
        registry.entries.iter().map(registry_entry),
        "identifier",
        validator,
        identity_key,
    )
    .map_err(|err| anyhow!("{namespace} registry: {err}"))?;
    parse_registry_date(&registry.generated_on)
        .map_err(|reason| anyhow!("{namespace} registry generated_on: {reason}"))?;
    for entry in &registry.entries {
        validate_evidence(&entry.evidence)
            .map_err(|reason| anyhow!("{namespace} registry {}: {reason}", entry.identifier))?;
    }
    Ok(registry)
}

fn registry_entry(entry: &ProtocolAccountEntry) -> IdentityRegistryEntry<'_> {
    IdentityRegistryEntry {
        identifier: &entry.identifier,
        pool_slug: &entry.pool_slug,
        pool_canonical_name: &entry.pool_canonical_name,
    }
}

fn validate_evidence(evidence: &AssociationEvidence) -> Result<(), String> {
    match evidence {
        AssociationEvidence::FirstPartyRegistration {
            source,
            recorded_on,
        } => {
            if source.trim().is_empty() {
                return Err("first_party_registration has no source".to_owned());
            }
            parse_registry_date(recorded_on).map(drop)
        }
        AssociationEvidence::EmpiricalAssociation {
            carrying_blocks,
            resolved_blocks,
            pool_blocks,
            first_height,
            last_height,
        } => {
            if pool_blocks > resolved_blocks || resolved_blocks > carrying_blocks {
                return Err("block counts must satisfy pool <= resolved <= carrying".to_owned());
            }
            if first_height > last_height {
                return Err("first_height is after last_height".to_owned());
            }
            if *carrying_blocks < MIN_CARRYING_BLOCKS
                || u64::from(*pool_blocks) * 100
                    < u64::from(*resolved_blocks) * u64::from(MIN_POOL_SHARE_PERCENT)
            {
                return Err(format!(
                    "below the observation rule ({MIN_CARRYING_BLOCKS} carrying blocks, \
                     {MIN_POOL_SHARE_PERCENT}% of resolved blocks)"
                ));
            }
            Ok(())
        }
    }
}

/// `0x` plus 40 lowercase hex digits, the form `protocol_commitment_script.account_identifier`
/// gives a declared reward address.
fn validate_core_reward_address(identifier: &str) -> Result<(), String> {
    match identifier.strip_prefix("0x") {
        Some(hex)
            if hex.len() == 40
                && hex
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) =>
        {
            Ok(())
        }
        _ => Err("not 0x plus 40 lowercase hex digits".to_owned()),
    }
}

fn validate_exsat_account(identifier: &str) -> Result<(), String> {
    if is_exsat_account(identifier) {
        Ok(())
    } else {
        Err("not an exSat account name".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_registries_parse_and_meet_their_rules() {
        for (namespace, json, validator) in embedded_registries() {
            let registry = parse_registry(namespace, json, validator).unwrap();
            assert!(!registry.entries.is_empty(), "{namespace} has entries");
        }
    }

    #[test]
    fn rejects_an_empirical_association_below_the_observation_rule() {
        let entry = |pool_blocks: u32| {
            format!(
                r#"{{"schema_version":1,"generated_on":"2026-10-19","entries":[{{
                "identifier":"0x{}","pool_slug":"antpool","pool_canonical_name":"AntPool",
                "evidence":{{"basis":"empirical_association","carrying_blocks":91,
                "resolved_blocks":91,"pool_blocks":{pool_blocks},"first_height":795360,
                "last_height":796544}}}}]}}"#,
                "c6".repeat(20)
            )
        };
        let core = |json: &str| {
            parse_registry(
                CORE_REWARD_ADDRESS_NAMESPACE,
                json,
                validate_core_reward_address,
            )
        };
        assert!(core(&entry(82)).is_ok());
        assert!(core(&entry(76)).is_err());
    }
}
