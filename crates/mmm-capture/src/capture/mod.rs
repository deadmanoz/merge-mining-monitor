//! Convert chain-specific evidence into database-ready `merge_mining_event` payloads.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use bitcoin::block::Header;
use bitcoin::consensus::{deserialize, encode, serialize};
use bitcoin::hashes::Hash as _;
use bitcoin::{Address, Network, ScriptBuf};
use serde_json::{Value, json};

use crate::auxpow::{ParsedAuxpowBlock, output_addresses, validates_target};
use crate::child_payout::{
    ChildPayoutParams, PoolIdentityLookup, PoolIdentityRef, child_output_addresses,
    pool_identity_lookup_key,
};
use crate::pool_resolver::{MatchKind, PoolMatch, PoolResolver};

// Persisted `pool_attribution` namespace and `source` literals (frozen schema contract).
pub const BTC_COINBASE_TAG_NAMESPACE: &str = "btc_coinbase_tag";
pub const BTC_PAYOUT_ADDRESS_NAMESPACE: &str = "btc_payout_address";
pub const BTC_POOL_SNAPSHOT_SOURCE: &str = "btc_pool_snapshot";
pub const BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE: &str =
    "btc_pool_snapshot_legacy_child_script";
pub const CHILD_COINBASE_OUTPUT_SOURCE: &str = "child_coinbase_output";
pub const CHILD_PAYOUT_REGISTRY_SOURCE: &str = "child_payout_registry";
pub const RSK_MINER_REGISTRY_SOURCE: &str = "rsk_miner_registry";
pub const RSK_RPC_MINER_SOURCE: &str = "rsk_rpc_miner";

/// BTC parent classification of a capture, stored on `merge_mining_event.btc_parent_kind`.
/// Distinct from `BlockKind` (`block.kind`) because a capture can also find a `Near` header
/// that fails the BTC PoW target, which is recorded only as a `child_block_observation`.
/// Orphan status lives on `block.btc_orphan_class`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentKind {
    Canonical,
    Stale,
    /// Full Bitcoin PoW, but proven consensus-invalid by the pinned invalid-block catalogue.
    InvalidBlock,
    /// Fails the BTC parent target (child-only work); forced by `classify_parent` regardless of proof.
    Near,
    /// Satisfies the BTC target without a canonical/stale height proof yet; never demotes a proven row.
    Unknown,
}

impl ParentKind {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Stale => "stale",
            Self::InvalidBlock => "invalid_block",
            Self::Near => "near",
            Self::Unknown => "unknown",
        }
    }
}

/// External classifier input and sole source of `btc_parent_height` (never inferred from coinbase scripts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassificationProof {
    pub parent_kind: Option<ParentKind>,
    pub parent_height: Option<i32>,
    pub difficulty_epoch_ok: Option<bool>,
    /// True when the verdict may change on retry (tolerated Core lookup error or point-in-time Core absence).
    pub provisional: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeMiningEventPayload {
    pub child_height: Option<i32>,
    pub child_block_hash: Option<Vec<u8>>,
    pub child_header_bytes: Option<Vec<u8>>,
    pub child_block_time: Option<i64>,
    pub child_nbits: Option<u32>,
    /// The child block's predecessor (internal byte order) when the chain reports it
    /// outside an 80-byte header. Observation only: the event does not store it.
    pub child_prev_block_hash: Option<Vec<u8>>,
    pub btc_parent_header_hash: Vec<u8>,
    pub btc_parent_prev_header_hash: Vec<u8>,
    pub btc_parent_header_bytes: Vec<u8>,
    pub btc_parent_header_time: i64,
    pub btc_parent_height: Option<i32>,
    pub btc_parent_kind: ParentKind,
    pub pow_validates_btc_target: bool,
    pub pow_validates_child_target: Option<bool>,
    pub difficulty_epoch_ok: Option<bool>,
    /// Transient flag from [`ClassificationProof::provisional`] so the producer re-observes the height.
    pub classification_provisional: bool,
    pub btc_parent_coinbase_txid: Option<Vec<u8>>,
    pub btc_parent_coinbase_script: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs_text: Option<String>,
    pub btc_parent_coinbase_tx_bytes: Option<Vec<u8>>,
    pub child_coinbase_txid: Option<Vec<u8>>,
    pub child_coinbase_script: Option<Vec<u8>>,
    pub child_coinbase_outputs: Option<Vec<u8>>,
    pub aux_merkle_proof: Option<Vec<u8>>,
    pub pool_attributions: Vec<EventPoolAttribution>,
    pub discovered_at: i64,
    pub confirmed_at: i64,
    pub revoked_at: Option<i64>,
    pub revocation_reason: Option<String>,
    pub historical_provenance: Option<HistoricalEventProvenance>,
}

/// Publication-row provenance attached to an imported event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalEventProvenance {
    pub publication_ref: String,
    pub chain: String,
    pub source_kind: String,
    pub source_path: String,
    pub source_row_number: i64,
    pub artifact_scope: String,
    pub provenance: String,
    pub classification: String,
    pub btc_height: Option<i32>,
    pub validation_status: Option<String>,
    pub btc_stale_relevance: Option<String>,
    pub relevance_reason: Option<String>,
}

/// Ordered pool attributions for an event (parent first, then child script, then child payout addresses).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPoolAttributions {
    pub attributions: Vec<EventPoolAttribution>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolAttributionSide {
    BtcParent,
    ChildBlock,
}

impl PoolAttributionSide {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::BtcParent => "btc_parent",
            Self::ChildBlock => "child_block",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolAttributionConfidence {
    High,
    Medium,
    Low,
}

impl PoolAttributionConfidence {
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// One `pool_attribution` row keyed by `(namespace, matched_value)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPoolAttribution {
    pub side: PoolAttributionSide,
    pub namespace: &'static str,
    pub match_kind: &'static str,
    pub matched_value: String,
    pub pool_id: Option<i64>,
    pub pool_identity_id: Option<i64>,
    pub source: &'static str,
    pub confidence: PoolAttributionConfidence,
    pub details: Value,
}

impl EventPoolAttribution {
    pub fn from_btc_parent_pool_match(pool_match: &PoolMatch<'_>, pool_id: i64) -> Self {
        Self::from_btc_pool_match(
            PoolAttributionSide::BtcParent,
            pool_match,
            Some(pool_id),
            BTC_POOL_SNAPSHOT_SOURCE,
        )
    }

    pub fn from_legacy_child_pool_match(pool_match: &PoolMatch<'_>, pool_id: i64) -> Self {
        Self::from_btc_pool_match(
            PoolAttributionSide::ChildBlock,
            pool_match,
            Some(pool_id),
            BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE,
        )
    }

    pub fn rsk_miner_address(
        miner_address: String,
        pool_id: Option<i64>,
        pool_identity_id: Option<i64>,
        mapped: bool,
    ) -> Self {
        Self {
            side: PoolAttributionSide::ChildBlock,
            namespace: crate::pool_resolver::RSK_MINER_ADDRESS_NAMESPACE,
            match_kind: "miner_address",
            matched_value: miner_address,
            pool_id,
            pool_identity_id,
            source: if mapped {
                RSK_MINER_REGISTRY_SOURCE
            } else {
                RSK_RPC_MINER_SOURCE
            },
            confidence: PoolAttributionConfidence::High,
            details: json!({}),
        }
    }

    /// Mapped addresses tag `CHILD_PAYOUT_REGISTRY_SOURCE`; unmapped addresses tag
    /// `CHILD_COINBASE_OUTPUT_SOURCE` on the same `(namespace, matched_value)` slot for in-place promotion.
    pub fn child_payout_address(
        params: ChildPayoutParams,
        address: String,
        identity: Option<PoolIdentityRef>,
    ) -> Self {
        let mapped = identity.is_some();
        Self {
            side: PoolAttributionSide::ChildBlock,
            namespace: params.namespace,
            match_kind: "payout_address",
            matched_value: address,
            pool_id: identity.map(|identity| identity.pool_id),
            pool_identity_id: identity.map(|identity| identity.pool_identity_id),
            source: if mapped {
                CHILD_PAYOUT_REGISTRY_SOURCE
            } else {
                CHILD_COINBASE_OUTPUT_SOURCE
            },
            confidence: match identity.map(|identity| identity.payout_confidence) {
                Some(crate::child_payout::ChildPayoutConfidence::High) => {
                    PoolAttributionConfidence::High
                }
                _ => PoolAttributionConfidence::Medium,
            },
            details: json!({ "address_source": "child_coinbase_outputs" }),
        }
    }

    fn from_btc_pool_match(
        side: PoolAttributionSide,
        pool_match: &PoolMatch<'_>,
        pool_id: Option<i64>,
        source: &'static str,
    ) -> Self {
        let (namespace, match_kind) = match pool_match.matched_by {
            MatchKind::CoinbaseTag => (BTC_COINBASE_TAG_NAMESPACE, "coinbase_tag"),
            MatchKind::PayoutAddress => (BTC_PAYOUT_ADDRESS_NAMESPACE, "payout_address"),
        };
        Self {
            side,
            namespace,
            match_kind,
            matched_value: pool_match.matched_value.to_owned(),
            pool_id,
            pool_identity_id: None,
            source,
            confidence: PoolAttributionConfidence::High,
            details: json!({}),
        }
    }
}

mod observation;
mod sidecars;

pub use observation::{ChildBlockObservation, ChildIdentity, NearParentEvidence, ObservationKind};
pub use sidecars::{
    HATHOR_PROOF_FORMAT_RFC0006, HathorEvidencePayload, RSK_PROOF_FORMAT_OPAQUE, RskEvidencePayload,
};

/// Chain-agnostic capture evidence consumed by [`build_event_payload_from_evidence`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedEventEvidence {
    pub child_height: Option<i32>,
    pub child_block_hash: Option<Vec<u8>>,
    pub child_header_bytes: Option<Vec<u8>>,
    pub child_block_time: Option<i64>,
    pub child_nbits: Option<u32>,
    /// The child block's predecessor (internal byte order) when the chain reports it
    /// outside an 80-byte header. Observation only: the event does not store it.
    pub child_prev_block_hash: Option<Vec<u8>>,
    pub btc_parent_header: Header,
    pub pow_validates_child_target: Option<bool>,
    pub btc_parent_coinbase_txid: Option<Vec<u8>>,
    pub btc_parent_coinbase_script: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs_text: Option<String>,
    pub btc_parent_coinbase_tx_bytes: Option<Vec<u8>>,
    pub child_coinbase_txid: Option<Vec<u8>>,
    pub child_coinbase_script: Option<Vec<u8>>,
    pub child_coinbase_outputs: Option<Vec<u8>>,
    pub aux_merkle_proof: Option<Vec<u8>>,
}

pub fn build_event_payload(
    parsed: &ParsedAuxpowBlock,
    child_height_hint: Option<i32>,
    pool_attributions: ResolvedPoolAttributions,
    proof: ClassificationProof,
    observed_at_epoch: i64,
) -> Result<MergeMiningEventPayload> {
    let evidence = namecoin_evidence(parsed, child_height_hint)?;
    build_event_payload_from_evidence(evidence, pool_attributions, proof, observed_at_epoch)
}

/// Build a `merge_mining_event` payload from normalized evidence.
/// `btc_parent_height` comes strictly from [`ClassificationProof`] or the invalid-block catalogue.
pub fn build_event_payload_from_evidence(
    evidence: NormalizedEventEvidence,
    pool_attributions: ResolvedPoolAttributions,
    proof: ClassificationProof,
    observed_at_epoch: i64,
) -> Result<MergeMiningEventPayload> {
    let parent_hash = evidence.btc_parent_header.block_hash();
    let pow_validates_btc_target = validates_target(parent_hash, evidence.btc_parent_header.bits);
    let (proven_kind, btc_parent_height) = invalid_block_proof(
        &parent_hash.to_byte_array(),
        pow_validates_btc_target,
        proof,
    );

    let btc_parent_kind =
        classify_parent(pow_validates_btc_target, proven_kind, btc_parent_height)?;

    Ok(MergeMiningEventPayload {
        child_height: evidence.child_height,
        child_block_hash: evidence.child_block_hash,
        child_header_bytes: evidence.child_header_bytes,
        child_block_time: evidence.child_block_time,
        child_nbits: evidence.child_nbits,
        child_prev_block_hash: evidence.child_prev_block_hash,
        btc_parent_header_hash: parent_hash.to_byte_array().to_vec(),
        btc_parent_prev_header_hash: evidence
            .btc_parent_header
            .prev_blockhash
            .to_byte_array()
            .to_vec(),
        btc_parent_header_bytes: serialize(&evidence.btc_parent_header),
        btc_parent_header_time: evidence.btc_parent_header.time as i64,
        btc_parent_height,
        btc_parent_kind,
        pow_validates_btc_target,
        pow_validates_child_target: evidence.pow_validates_child_target,
        difficulty_epoch_ok: proof.difficulty_epoch_ok,
        classification_provisional: false,
        btc_parent_coinbase_txid: evidence.btc_parent_coinbase_txid,
        btc_parent_coinbase_script: evidence.btc_parent_coinbase_script,
        btc_parent_coinbase_outputs: evidence.btc_parent_coinbase_outputs,
        btc_parent_coinbase_outputs_text: evidence.btc_parent_coinbase_outputs_text,
        btc_parent_coinbase_tx_bytes: evidence.btc_parent_coinbase_tx_bytes,
        child_coinbase_txid: evidence.child_coinbase_txid,
        child_coinbase_script: evidence.child_coinbase_script,
        child_coinbase_outputs: evidence.child_coinbase_outputs,
        aux_merkle_proof: evidence.aux_merkle_proof,
        pool_attributions: pool_attributions.attributions,
        discovered_at: observed_at_epoch,
        confirmed_at: observed_at_epoch,
        revoked_at: None,
        revocation_reason: None,
        historical_provenance: None,
    })
}

/// Re-apply an external [`ClassificationProof`] to an already-built payload in place.
pub fn apply_classification_proof(
    payload: &mut MergeMiningEventPayload,
    proof: ClassificationProof,
) -> Result<()> {
    let (proven_kind, btc_parent_height) = invalid_block_proof(
        &payload.btc_parent_header_hash,
        payload.pow_validates_btc_target,
        proof,
    );
    payload.btc_parent_kind = classify_parent(
        payload.pow_validates_btc_target,
        proven_kind,
        btc_parent_height,
    )?;
    payload.btc_parent_height = btc_parent_height;
    payload.difficulty_epoch_ok = proof.difficulty_epoch_ok;
    payload.classification_provisional = proof.provisional;
    Ok(())
}

fn namecoin_evidence(
    parsed: &ParsedAuxpowBlock,
    child_height_hint: Option<i32>,
) -> Result<NormalizedEventEvidence> {
    let child_height = child_height_hint
        .or(parsed.child_height)
        .ok_or_else(|| anyhow::anyhow!("AuxPoW child height is unavailable"))?;

    let pow_validates_child_target = Some(validates_target(
        parsed.parent_header.hash(),
        parsed.child_header.bits(),
    ));

    Ok(NormalizedEventEvidence {
        child_height: Some(child_height),
        child_block_hash: Some(parsed.child_header.hash().to_byte_array().to_vec()),
        child_header_bytes: Some(parsed.child_header.consensus_bytes()),
        child_block_time: Some(parsed.child_header.time() as i64),
        child_nbits: Some(parsed.child_header.bits().to_consensus()),
        child_prev_block_hash: None,
        btc_parent_header: parsed.parent_header.header,
        pow_validates_child_target,
        btc_parent_coinbase_txid: Some(parsed.parent_coinbase_txid.to_byte_array().to_vec()),
        btc_parent_coinbase_script: Some(parsed.parent_coinbase_script.clone()),
        btc_parent_coinbase_outputs: Some(serialize(&parsed.parent_coinbase_outputs)),
        btc_parent_coinbase_outputs_text: None,
        btc_parent_coinbase_tx_bytes: None,
        child_coinbase_txid: parsed
            .child_coinbase_txid
            .map(|hash| hash.to_byte_array().to_vec()),
        child_coinbase_script: parsed.child_coinbase_script.clone(),
        child_coinbase_outputs: (!parsed.child_coinbase_outputs.is_empty())
            .then(|| serialize(&parsed.child_coinbase_outputs)),
        aux_merkle_proof: Some(parsed.auxpow_bytes.clone()),
    })
}

/// Resolve AuxPoW attributions in persisted order: BTC parent, legacy child script, then child payout addresses.
pub fn resolve_event_pools_with_child_payout(
    parsed: &ParsedAuxpowBlock,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
    child_payout_params: Option<ChildPayoutParams>,
    child_payout_identities: Option<&PoolIdentityLookup>,
) -> ResolvedPoolAttributions {
    let parent_attribution = resolve_parent_pool_attribution_from_coinbase(
        &parsed.parent_coinbase_script,
        &parsed.parent_coinbase_output_addresses,
        resolver,
        pool_ids_by_slug,
    );
    let mut attributions = Vec::new();
    if let Some(attribution) = parent_attribution.as_ref() {
        attributions.push(attribution.clone());
    }
    if let Some(attribution) = resolve_child_pool_attribution_from_script(
        parsed.child_coinbase_script.as_deref(),
        resolver,
        pool_ids_by_slug,
    ) {
        attributions.push(attribution);
    }
    if let Some(params) = child_payout_params {
        attributions.extend(resolve_child_payout_attributions(
            &parsed.child_coinbase_outputs,
            params,
            child_payout_identities,
        ));
    }

    ResolvedPoolAttributions { attributions }
}

pub fn resolve_child_payout_attributions(
    child_coinbase_outputs: &[crate::auxpow::TxOut],
    params: ChildPayoutParams,
    child_payout_identities: Option<&PoolIdentityLookup>,
) -> Vec<EventPoolAttribution> {
    child_output_addresses(child_coinbase_outputs, params)
        .into_iter()
        .map(|address| {
            let identity = child_payout_identities.and_then(|identities| {
                identities
                    .get(&pool_identity_lookup_key(params.namespace, &address))
                    .copied()
            });
            EventPoolAttribution::child_payout_address(params, address, identity)
        })
        .collect()
}

pub fn now_epoch_seconds() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}

/// Reconcile offline PoW validation against an external proof into [`ParentKind`].
fn classify_parent(
    pow_validates_btc_target: bool,
    proven_kind: Option<ParentKind>,
    proven_height: Option<i32>,
) -> Result<ParentKind> {
    if !pow_validates_btc_target {
        return Ok(ParentKind::Near);
    }

    match proven_kind {
        Some(ParentKind::Near) => bail!("near parent proof conflicts with BTC target validation"),
        Some(ParentKind::Canonical | ParentKind::Stale | ParentKind::InvalidBlock)
            if proven_height.is_none() =>
        {
            bail!("canonical/stale/invalid-block parent proof requires btc_parent_height")
        }
        Some(kind) => Ok(kind),
        None => Ok(ParentKind::Unknown),
    }
}

/// Override any optional Core proof when a PoW-valid parent matches the pinned invalid-block catalogue.
fn invalid_block_proof(
    parent_hash: &[u8],
    pow_validates_btc_target: bool,
    proof: ClassificationProof,
) -> (Option<ParentKind>, Option<i32>) {
    if pow_validates_btc_target
        && let Some(invalid_block) = crate::invalid_blocks::lookup(parent_hash)
    {
        return (Some(ParentKind::InvalidBlock), Some(invalid_block.height));
    }
    let height = match proof.parent_kind {
        Some(ParentKind::Canonical | ParentKind::Stale | ParentKind::InvalidBlock) => {
            proof.parent_height
        }
        Some(ParentKind::Near | ParentKind::Unknown) | None => None,
    };
    (proof.parent_kind, height)
}

/// Resolve BTC parent pool attribution (coinbase script first, then payout addresses).
pub fn resolve_parent_pool_attribution_from_coinbase(
    parent_coinbase_script: &[u8],
    parent_coinbase_output_addresses: &[String],
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> Option<EventPoolAttribution> {
    resolve_btc_pool_match_from_coinbase(
        parent_coinbase_script,
        parent_coinbase_output_addresses,
        resolver,
    )
    .and_then(|pool_match| {
        pool_ids_by_slug
            .get(&pool_match.pool.slug)
            .copied()
            .map(|pool_id| EventPoolAttribution::from_btc_parent_pool_match(&pool_match, pool_id))
    })
}

/// Try coinbase script match first; deserialize `serialized_outputs` only if script fails to match.
pub fn resolve_parent_pool_attribution_from_serialized_coinbase_outputs(
    parent_coinbase_script: &[u8],
    serialized_outputs: Option<&[u8]>,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> std::result::Result<Option<EventPoolAttribution>, encode::Error> {
    if let Some(attribution) = resolve_parent_pool_attribution_from_coinbase(
        parent_coinbase_script,
        &[],
        resolver,
        pool_ids_by_slug,
    ) {
        return Ok(Some(attribution));
    }

    let Some(outputs_bytes) = serialized_outputs else {
        return Ok(None);
    };
    let outputs: Vec<crate::auxpow::TxOut> = deserialize(outputs_bytes)?;
    let addresses = output_addresses(&outputs);
    Ok(resolve_parent_pool_attribution_from_coinbase(
        parent_coinbase_script,
        &addresses,
        resolver,
        pool_ids_by_slug,
    ))
}

/// Decode normalized publication parent-output text (`;`/`|`-delimited scriptPubKey hex or address,
/// optionally paired with a `:` value; skips `OP_RETURN`/`nonstandard:`) into Bitcoin mainnet addresses.
pub fn published_parent_coinbase_output_addresses(value: &str) -> Vec<String> {
    let mut addresses = Vec::new();
    for entry in value.split([';', '|']).map(str::trim) {
        if entry.is_empty() || entry.starts_with("OP_RETURN") || entry.starts_with("nonstandard:") {
            continue;
        }
        let address = entry
            .split_once(':')
            .and_then(|(left, right)| {
                published_parent_output_address(left.trim())
                    .or_else(|| published_parent_output_address(right.trim()))
            })
            .or_else(|| published_parent_output_address(entry));
        let Some(address) = address else { continue };
        if !addresses.contains(&address) {
            addresses.push(address);
        }
    }
    addresses
}

fn published_parent_output_address(candidate: &str) -> Option<String> {
    if candidate.is_empty() {
        return None;
    }
    if let Ok(address) = Address::from_str(candidate) {
        return address
            .require_network(Network::Bitcoin)
            .ok()
            .map(|address| address.to_string());
    }
    if !candidate.len().is_multiple_of(2) || !candidate.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return None;
    }
    let script = ScriptBuf::from_bytes(hex::decode(candidate).ok()?);
    Address::from_script(&script, Network::Bitcoin)
        .ok()
        .map(|address| address.to_string())
}

fn resolve_btc_pool_match_from_coinbase<'a>(
    parent_coinbase_script: &'a [u8],
    parent_coinbase_output_addresses: &'a [String],
    resolver: &'a PoolResolver,
) -> Option<PoolMatch<'a>> {
    resolver
        .resolve_coinbase_script(parent_coinbase_script)
        .or_else(|| {
            resolver.resolve_payout_addresses(
                parent_coinbase_output_addresses.iter().map(String::as_str),
            )
        })
}

pub fn resolve_child_pool_attribution_from_script(
    script: Option<&[u8]>,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> Option<EventPoolAttribution> {
    let script_match = script.and_then(|script| resolver.resolve_coinbase_script(script));

    script_match.as_ref().and_then(|pool_match| {
        pool_ids_by_slug
            .get(&pool_match.pool.slug)
            .copied()
            .map(|pool_id| EventPoolAttribution::from_legacy_child_pool_match(pool_match, pool_id))
    })
}

#[cfg(test)]
mod tests;
