//! Convert chain-specific evidence into database-ready capture payloads.
//!
//! Producers (Namecoin AuxPoW today, RSK eth_getBlockByNumber payloads next)
//! convert their chain-specific evidence to a [`NormalizedEventEvidence`]
//! intermediate, then call [`build_event_payload_from_evidence`] to produce a
//! [`MergeMiningEventPayload`] for `merge_mining_event`. This keeps the
//! producer table chain-agnostic while letting each chain own its own
//! adapter.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use bitcoin::block::Header;
use bitcoin::consensus::{deserialize, encode, serialize};
use bitcoin::hashes::Hash as _;
use serde_json::{Value, json};

use crate::auxpow::{ParsedAuxpowBlock, output_addresses, validates_target};
use crate::child_payout::{
    ChildPayoutParams, PoolIdentityLookup, PoolIdentityRef, child_output_addresses,
    pool_identity_lookup_key,
};
use crate::pool_resolver::{MatchKind, PoolMatch, PoolResolver};

/// Attribution namespace for a BTC parent coinbase pool-tag match: the
/// `namespace` half of the `(namespace, matched_value)` attribution key,
/// paired with the raw coinbase-tag bytes matched by
/// `PoolResolver::resolve_coinbase_script`. Consumed only inside this module
/// by `from_btc_pool_match`; producers never reference it directly.
pub const BTC_COINBASE_TAG_NAMESPACE: &str = "btc_coinbase_tag";
/// Attribution namespace for a BTC parent payout-address pool match: the
/// `namespace` half of the `(namespace, matched_value)` key, paired with the
/// address string matched by `PoolResolver::resolve_payout_addresses`.
/// Internal to this module's `from_btc_pool_match`; not consumed cross-crate.
pub const BTC_PAYOUT_ADDRESS_NAMESPACE: &str = "btc_payout_address";
/// Source label written into `pool_attribution.source` for a BTC parent pool
/// match resolved from the embedded pool snapshot (coinbase tag or payout
/// address). Part of the persisted attribution contract: `mmm-store`, the
/// reclassify_pools planner, and integration tests match on it verbatim, so
/// the literal is frozen, never renamed casually.
pub const BTC_POOL_SNAPSHOT_SOURCE: &str = "btc_pool_snapshot";
/// Source label for a child-block pool match resolved by reusing the BTC pool
/// snapshot against a Namecoin-style child coinbase script (the legacy
/// pre-payout-address path). Persisted in `pool_attribution.source`;
/// `mmm-producers` reclassify_pools and tests match on it verbatim, so the
/// literal is part of the stored contract.
pub const BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE: &str =
    "btc_pool_snapshot_legacy_child_script";
/// Source label for an UNMAPPED child payout-address attribution: an address
/// decoded from the child coinbase outputs that the embedded identity registry
/// could not resolve to a pool. Paired with `CHILD_PAYOUT_REGISTRY_SOURCE`
/// (the mapped case). `mmm-store`'s source-set classifier, the reclassify
/// planner, and every chain reward-replay match on this literal, so it is a
/// frozen part of the persisted attribution contract.
pub const CHILD_COINBASE_OUTPUT_SOURCE: &str = "child_coinbase_output";
/// Source label for a MAPPED child payout-address attribution: an address that
/// the embedded pool-identity registry resolved to a pool identity. The mapped
/// counterpart to `CHILD_COINBASE_OUTPUT_SOURCE`; the two share a single
/// `(namespace, matched_value)` slot so a later registry update promotes an
/// unmapped row to mapped in place. Frozen persisted label.
pub const CHILD_PAYOUT_REGISTRY_SOURCE: &str = "child_payout_registry";
/// Source label for an RSK miner-address attribution whose hex was resolved
/// through the embedded miner registry (mapped to a pool identity). Selected
/// inside `EventPoolAttribution::rsk_miner_address` on `mapped == true`.
/// Producers reach this label only via that constructor, never by importing
/// the const, so the value stays encapsulated here.
pub const RSK_MINER_REGISTRY_SOURCE: &str = "rsk_miner_registry";
/// Source label for an RSK miner-address attribution taken straight from the
/// RSK RPC miner field with no registry mapping (unmapped). Selected inside
/// `EventPoolAttribution::rsk_miner_address` on `mapped == false`. Reached by
/// producers only via that constructor; kept internal to this module.
pub const RSK_RPC_MINER_SOURCE: &str = "rsk_rpc_miner";

/// BTC parent classification stored on `merge_mining_event.btc_parent_kind`.
/// Orphan-ness is deliberately NOT a variant here. `Near` means the claimed
/// parent header fails the BTC PoW target (a child-only header, not a real
/// BTC block); `Unknown` means PoW passes but no canonical/stale proof is
/// available yet; `Canonical`/`Stale` require an external height proof.
/// Orphan status is the derived `block.btc_orphan_class`, set only on a
/// Core-absence verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentKind {
    Canonical,
    Stale,
    /// PoW does NOT satisfy the BTC parent target: the header is a
    /// child-chain-only artifact, not a BTC block. `classify_parent` forces
    /// this whenever `pow_validates_btc_target` is false, regardless of any
    /// supplied proof.
    Near,
    /// PoW satisfies the BTC target but no canonical/stale height proof exists
    /// yet. Transient: a later classifier pass may promote it. Per the project
    /// invariant a transient `unknown` must never demote a proven
    /// canonical/stale row.
    Unknown,
}

impl ParentKind {
    /// Stable DB string for `merge_mining_event.btc_parent_kind`. The mapping
    /// is part of the persisted schema contract: these literals must match the
    /// column CHECK constraint and the store/read-model parsers, so they are
    /// frozen.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::Canonical => "canonical",
            Self::Stale => "stale",
            Self::Near => "near",
            Self::Unknown => "unknown",
        }
    }
}

/// External-classifier input to [`build_event_payload_from_evidence`] /
/// [`apply_classification_proof`]. The ONLY sanctioned source of
/// `btc_parent_height`: height is read from here, never inferred from coinbase
/// format (pre-BIP34 scripts can look height-like). `parent_kind == None` means
/// no proof yet (Unknown when PoW is valid); a Canonical/Stale proof requires
/// `parent_height` to be Some or `classify_parent` errors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClassificationProof {
    pub parent_kind: Option<ParentKind>,
    pub parent_height: Option<i32>,
    pub difficulty_epoch_ok: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeMiningEventPayload {
    pub child_height: i32,
    pub child_block_hash: Vec<u8>,
    pub child_block_time: i64,
    pub btc_parent_header_hash: Vec<u8>,
    pub btc_parent_prev_header_hash: Vec<u8>,
    pub btc_parent_header_bytes: Vec<u8>,
    pub btc_parent_header_time: i64,
    pub btc_parent_height: Option<i32>,
    pub btc_parent_kind: ParentKind,
    pub pow_validates_btc_target: bool,
    pub pow_validates_child_target: Option<bool>,
    pub difficulty_epoch_ok: Option<bool>,
    pub btc_parent_coinbase_txid: Option<Vec<u8>>,
    pub btc_parent_coinbase_script: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs: Option<Vec<u8>>,
    pub child_coinbase_txid: Option<Vec<u8>>,
    pub child_coinbase_script: Option<Vec<u8>>,
    pub child_coinbase_outputs: Option<Vec<u8>>,
    pub aux_merkle_proof: Option<Vec<u8>>,
    pub pool_attributions: Vec<EventPoolAttribution>,
    pub discovered_at: i64,
    pub confirmed_at: i64,
    pub revoked_at: Option<i64>,
    pub revocation_reason: Option<String>,
}

/// Ordered set of pool attributions for one event, produced by the
/// `resolve_event_pools*` family and folded into
/// `MergeMiningEventPayload.pool_attributions`. Order is meaningful: parent
/// attribution first, then child script, then child payout addresses, matching
/// the resolver call order in `resolve_event_pools_with_child_payout`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolvedPoolAttributions {
    pub attributions: Vec<EventPoolAttribution>,
}

/// Which side of the merge-mined pair an attribution describes: the BTC parent
/// coinbase (`BtcParent`) or the child block's own coinbase (`ChildBlock`).
/// `as_db_str` is the persisted `pool_attribution.side` value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolAttributionSide {
    BtcParent,
    ChildBlock,
}

impl PoolAttributionSide {
    /// Persisted `pool_attribution.side` literal ("btc_parent"/"child_block").
    /// Frozen: must match the column CHECK constraint and store/read-model
    /// parsers.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::BtcParent => "btc_parent",
            Self::ChildBlock => "child_block",
        }
    }
}

/// Confidence tier recorded on `pool_attribution.confidence`. High for
/// snapshot/registry-backed matches (coinbase tag, payout address, RSK miner
/// registry), Medium for child payout-address heuristics. `Low` is reserved
/// (no current producer emits it). `as_db_str` is the persisted literal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolAttributionConfidence {
    High,
    Medium,
    /// Lowest confidence tier. Part of the schema's allowed `confidence` set;
    /// no capture-side path constructs it today, kept for the persisted-value
    /// contract and external producers.
    Low,
}

impl PoolAttributionConfidence {
    /// Persisted `pool_attribution.confidence` literal ("high"/"medium"/"low").
    /// Frozen to match the column CHECK constraint and parsers.
    pub fn as_db_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }
}

/// One persisted `pool_attribution` row in flight. The attribution key is
/// `(namespace, matched_value)`: the namespace classifies the match domain
/// (coinbase tag, payout address, RSK miner address) and `matched_value` is
/// the raw matched bytes/string. `pool_id` / `pool_identity_id` are the
/// resolved targets (NULL until a registry resolves them); `source` records
/// which evidence path produced the match. Construct via the named
/// constructors rather than building literals so the namespace/source/
/// confidence stay consistent.
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
    /// Build a `BtcParent`-side attribution from a resolved snapshot
    /// `PoolMatch`, tagging it with `BTC_POOL_SNAPSHOT_SOURCE` and High
    /// confidence. `namespace` and `match_kind` are derived from
    /// `pool_match.matched_by` (coinbase tag vs payout address) inside
    /// `from_btc_pool_match`.
    pub fn from_btc_parent_pool_match(pool_match: &PoolMatch<'_>, pool_id: i64) -> Self {
        Self::from_btc_pool_match(
            PoolAttributionSide::BtcParent,
            pool_match,
            Some(pool_id),
            BTC_POOL_SNAPSHOT_SOURCE,
        )
    }

    /// Build a `ChildBlock`-side attribution from a snapshot `PoolMatch`
    /// against a child coinbase script (the legacy pre-payout-address path),
    /// tagged `BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE`, High confidence.
    pub fn from_legacy_child_pool_match(pool_match: &PoolMatch<'_>, pool_id: i64) -> Self {
        Self::from_btc_pool_match(
            PoolAttributionSide::ChildBlock,
            pool_match,
            Some(pool_id),
            BTC_POOL_SNAPSHOT_LEGACY_CHILD_SCRIPT_SOURCE,
        )
    }

    /// Build an RSK miner-address attribution (`ChildBlock` side,
    /// `RSK_MINER_ADDRESS_NAMESPACE`, High confidence). `mapped` selects the
    /// source label: `RSK_MINER_REGISTRY_SOURCE` when the miner hex resolved
    /// through the embedded registry, else `RSK_RPC_MINER_SOURCE`. This is the
    /// encapsulation boundary: producers call this constructor and never touch
    /// the source consts directly.
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

    /// Build a `ChildBlock` payout-address attribution (Medium confidence)
    /// from a decoded child coinbase output address. Presence of `identity`
    /// decides mapped vs unmapped: mapped fills `pool_id`/`pool_identity_id`
    /// and tags `CHILD_PAYOUT_REGISTRY_SOURCE`, unmapped leaves them NULL and
    /// tags `CHILD_COINBASE_OUTPUT_SOURCE` so a later registry update can
    /// promote the same `(namespace, matched_value)` slot in place.
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
            confidence: PoolAttributionConfidence::Medium,
            details: json!({ "address_source": "child_coinbase_outputs" }),
        }
    }

    /// Shared core for the two BTC snapshot constructors. Derives
    /// `(namespace, match_kind)` from `pool_match.matched_by`: `CoinbaseTag`
    /// maps to `(BTC_COINBASE_TAG_NAMESPACE, "coinbase_tag")`, `PayoutAddress`
    /// to `(BTC_PAYOUT_ADDRESS_NAMESPACE, "payout_address")`. High confidence,
    /// `pool_identity_id` always None (BTC snapshot matches resolve a pool, not
    /// an identity). `matched_value` is copied from the match verbatim.
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

mod sidecars;

pub use sidecars::{
    ELASTOS_REVOKE_CLASSIFIER_CONFLICT, ELASTOS_REVOKE_NON_BTC, HATHOR_PROOF_FORMAT_RFC0006,
    HATHOR_REVOKE_NBITS_CONFLICT, HATHOR_REVOKE_NON_BTC, HATHOR_REVOKE_SUPERSEDED,
    HATHOR_REVOKE_VOIDED, HathorEvidencePayload, RSK_PROOF_FORMAT_OPAQUE, RskEvidencePayload,
};

/// Chain-agnostic capture evidence consumed by
/// [`build_event_payload_from_evidence`]. Producers populate this from their
/// chain-specific raw inputs (Namecoin AuxPoW payload, RSK
/// `eth_getBlockByNumber` response, future chains, ...) and the helper
/// fills in derived fields (PoW validation against the BTC parent header
/// `nBits`, byte-array hash projections) consistently across chains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedEventEvidence {
    pub child_height: i32,
    pub child_block_hash: Vec<u8>,
    pub child_block_time: i64,
    pub btc_parent_header: Header,
    pub pow_validates_child_target: Option<bool>,
    pub btc_parent_coinbase_txid: Option<Vec<u8>>,
    pub btc_parent_coinbase_script: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs: Option<Vec<u8>>,
    pub child_coinbase_txid: Option<Vec<u8>>,
    pub child_coinbase_script: Option<Vec<u8>>,
    pub child_coinbase_outputs: Option<Vec<u8>>,
    pub aux_merkle_proof: Option<Vec<u8>>,
}

/// Namecoin adapter wrapper that produces a [`NormalizedEventEvidence`] from
/// a [`ParsedAuxpowBlock`]. Keeps the existing call sites (Namecoin capture,
/// fixture tests) on a stable signature.
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

/// Build a `merge_mining_event` payload from already-normalised evidence.
/// Bitcoin parent height comes only from [`ClassificationProof`]. Do not infer
/// it from parent coinbase format; pre-BIP34 scripts can look height-like.
pub fn build_event_payload_from_evidence(
    evidence: NormalizedEventEvidence,
    pool_attributions: ResolvedPoolAttributions,
    proof: ClassificationProof,
    observed_at_epoch: i64,
) -> Result<MergeMiningEventPayload> {
    let parent_hash = evidence.btc_parent_header.block_hash();
    let pow_validates_btc_target = validates_target(parent_hash, evidence.btc_parent_header.bits);
    let btc_parent_height = match proof.parent_kind {
        Some(ParentKind::Canonical | ParentKind::Stale) => proof.parent_height,
        _ => None,
    };

    let btc_parent_kind = classify_parent(
        pow_validates_btc_target,
        proof.parent_kind,
        btc_parent_height,
    )?;

    Ok(MergeMiningEventPayload {
        child_height: evidence.child_height,
        child_block_hash: evidence.child_block_hash,
        child_block_time: evidence.child_block_time,
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
        btc_parent_coinbase_txid: evidence.btc_parent_coinbase_txid,
        btc_parent_coinbase_script: evidence.btc_parent_coinbase_script,
        btc_parent_coinbase_outputs: evidence.btc_parent_coinbase_outputs,
        child_coinbase_txid: evidence.child_coinbase_txid,
        child_coinbase_script: evidence.child_coinbase_script,
        child_coinbase_outputs: evidence.child_coinbase_outputs,
        aux_merkle_proof: evidence.aux_merkle_proof,
        pool_attributions: pool_attributions.attributions,
        discovered_at: observed_at_epoch,
        confirmed_at: observed_at_epoch,
        revoked_at: None,
        revocation_reason: None,
    })
}

/// Re-apply an external [`ClassificationProof`] to an already-built payload,
/// updating `btc_parent_kind`, `btc_parent_height`, and `difficulty_epoch_ok`
/// in place. Used by the read-model on reclassification/repair. Same invariant
/// as the builder: height is taken from the proof only (and only kept for
/// Canonical/Stale), never inferred; a Near proof against valid BTC PoW or a
/// Canonical/Stale proof without height errors via `classify_parent`.
pub fn apply_classification_proof(
    payload: &mut MergeMiningEventPayload,
    proof: ClassificationProof,
) -> Result<()> {
    let btc_parent_height = match proof.parent_kind {
        Some(ParentKind::Canonical | ParentKind::Stale) => proof.parent_height,
        _ => None,
    };
    payload.btc_parent_kind = classify_parent(
        payload.pow_validates_btc_target,
        proof.parent_kind,
        btc_parent_height,
    )?;
    payload.btc_parent_height = btc_parent_height;
    payload.difficulty_epoch_ok = proof.difficulty_epoch_ok;
    Ok(())
}

/// Project a [`ParsedAuxpowBlock`] into the chain-agnostic
/// [`NormalizedEventEvidence`]. `child_height` falls back: explicit hint, then
/// the parsed child height, else error. All hashes are stored as
/// `to_byte_array()` (wire/internal byte order), never reversed.
/// `child_coinbase_outputs` are consensus-serialized only when non-empty (None
/// means no child outputs were parsed). `pow_validates_child_target` checks the
/// BTC parent hash against the CHILD header's bits.
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
        child_height,
        child_block_hash: parsed.child_header.hash().to_byte_array().to_vec(),
        child_block_time: parsed.child_header.time() as i64,
        btc_parent_header: parsed.parent_header.header,
        pow_validates_child_target,
        btc_parent_coinbase_txid: Some(parsed.parent_coinbase_txid.to_byte_array().to_vec()),
        btc_parent_coinbase_script: Some(parsed.parent_coinbase_script.clone()),
        btc_parent_coinbase_outputs: Some(serialize(&parsed.parent_coinbase_outputs)),
        child_coinbase_txid: parsed
            .child_coinbase_txid
            .map(|hash| hash.to_byte_array().to_vec()),
        child_coinbase_script: parsed.child_coinbase_script.clone(),
        child_coinbase_outputs: (!parsed.child_coinbase_outputs.is_empty())
            .then(|| serialize(&parsed.child_coinbase_outputs)),
        aux_merkle_proof: Some(parsed.auxpow_bytes.clone()),
    })
}

/// Test-support wrapper for resolving AuxPoW pool attributions without the
/// child-payout pass. Production capture uses [`resolve_event_pools_with_child_payout`].
#[cfg(any(test, feature = "test-support"))]
pub fn resolve_event_pools(
    parsed: &ParsedAuxpowBlock,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> ResolvedPoolAttributions {
    resolve_event_pools_with_child_payout(parsed, resolver, pool_ids_by_slug, None, None)
}

/// Full AuxPoW pool resolution. Emits attributions in a fixed order: BTC
/// parent (coinbase tag, falling back to payout address), then the legacy child
/// coinbase-script match, then, when `child_payout_params` is supplied, one
/// attribution per decoded child coinbase payout address (mapped via
/// `child_payout_identities`, else unmapped). Order matters: it is the
/// persisted `pool_attribution` ordering.
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
    if let Some(attribution) = resolve_child_pool(parsed, resolver, pool_ids_by_slug) {
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

/// Decode child coinbase payout addresses (`child_output_addresses` with the
/// given [`ChildPayoutParams`] network/namespace) and turn each into an
/// [`EventPoolAttribution`], resolving against `child_payout_identities` by the
/// `(namespace, address)` lookup key. Used both during capture and by the
/// child-payout reclassify pass. Address byte-decoding semantics live in
/// `child_payout`; this fn only maps addresses to attributions.
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

/// Wall-clock UNIX seconds as i64, for `discovered_at` / `confirmed_at` /
/// observation timestamps. Errors if the clock is before the epoch or the value
/// overflows i64. The single sanctioned now-seconds helper; producers call it
/// so capture timestamps stay consistent.
pub fn now_epoch_seconds() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .try_into()?)
}

/// Reconcile offline PoW validation against an external classifier proof into a
/// [`ParentKind`]. Rules: PoW invalid means `Near` unconditionally (a
/// child-only header). PoW valid: a proven `Near` conflicts (error); a
/// Canonical/Stale proof without a height errors; any other proven kind is
/// taken as-is; absent proof means `Unknown`. This is the single chokepoint
/// enforcing the height-required and no-near-with-valid-PoW invariants.
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
        Some(ParentKind::Canonical | ParentKind::Stale) if proven_height.is_none() => {
            bail!("canonical/stale parent proof requires btc_parent_height")
        }
        Some(kind) => Ok(kind),
        None => Ok(ParentKind::Unknown),
    }
}

/// Resolve a BTC parent-side pool attribution from a coinbase script
/// (preferred) or, failing that, the parent's output payout addresses, then map
/// the matched pool slug to a `pool_id` via `pool_ids_by_slug` (None if the
/// slug is unknown). The shared parent-attribution helper: AuxPoW capture,
/// Elastos/Hathor capture, the historical ingest runner, and the reclassify
/// planner all call it.
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

/// Resolve a BTC parent-side pool attribution from a coinbase script and an
/// optional consensus-serialized `Vec<TxOut>`. This preserves the live capture
/// precedence: try the coinbase script first, and deserialize parent outputs
/// only when the script does not identify a persisted pool. Callers keep
/// corrupt-output logging local so they can attach operation-specific context.
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

/// Resolve a coinbase to a `PoolMatch`: try the coinbase script first
/// (`resolve_coinbase_script`), then fall back to the parent's payout addresses
/// (`resolve_payout_addresses`). Returns the match WITHOUT mapping to a
/// `pool_id`; the caller resolves the slug. The script-then-address precedence
/// is required for correctness. Internal to this module.
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

/// Resolve the legacy child-block pool attribution by matching the child
/// coinbase script against the resolver (script match only; payout addresses
/// are handled separately by `resolve_child_payout_attributions`). Maps the
/// matched slug to a `pool_id` and tags it with the legacy-child-script source.
fn resolve_child_pool(
    parsed: &ParsedAuxpowBlock,
    resolver: &PoolResolver,
    pool_ids_by_slug: &HashMap<String, i64>,
) -> Option<EventPoolAttribution> {
    let script_match = parsed
        .child_coinbase_script
        .as_deref()
        .and_then(|script| resolver.resolve_coinbase_script(script));

    script_match.as_ref().and_then(|pool_match| {
        pool_ids_by_slug
            .get(&pool_match.pool.slug)
            .copied()
            .map(|pool_id| EventPoolAttribution::from_legacy_child_pool_match(pool_match, pool_id))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_when_pow_valid_without_proof() {
        assert_eq!(
            classify_parent(true, None, None).unwrap(),
            ParentKind::Unknown
        );
    }

    #[test]
    fn near_when_pow_invalid_even_without_height() {
        assert_eq!(
            classify_parent(false, None, None).unwrap(),
            ParentKind::Near
        );
    }

    #[test]
    fn canonical_proof_requires_height() {
        let err = classify_parent(true, Some(ParentKind::Canonical), None).unwrap_err();
        assert!(err.to_string().contains("requires btc_parent_height"));
    }

    #[test]
    fn classification_proof_updates_payload_height_from_proof_only() {
        let mut payload = MergeMiningEventPayload {
            child_height: 1,
            child_block_hash: vec![1; 32],
            child_block_time: 1,
            btc_parent_header_hash: vec![2; 32],
            btc_parent_prev_header_hash: vec![3; 32],
            btc_parent_header_bytes: vec![4; 80],
            btc_parent_header_time: 1,
            btc_parent_height: None,
            btc_parent_kind: ParentKind::Unknown,
            pow_validates_btc_target: true,
            pow_validates_child_target: Some(true),
            difficulty_epoch_ok: None,
            btc_parent_coinbase_txid: None,
            btc_parent_coinbase_script: None,
            btc_parent_coinbase_outputs: None,
            child_coinbase_txid: None,
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            aux_merkle_proof: None,
            pool_attributions: Vec::new(),
            discovered_at: 10,
            confirmed_at: 10,
            revoked_at: None,
            revocation_reason: None,
        };

        apply_classification_proof(
            &mut payload,
            ClassificationProof {
                parent_kind: Some(ParentKind::Canonical),
                parent_height: Some(840_000),
                difficulty_epoch_ok: Some(true),
            },
        )
        .unwrap();

        assert_eq!(payload.btc_parent_kind, ParentKind::Canonical);
        assert_eq!(payload.btc_parent_height, Some(840_000));
        assert_eq!(payload.difficulty_epoch_ok, Some(true));
    }
}
