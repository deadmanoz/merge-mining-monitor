//! Block-detail projection for `/api/v1/block/:hash`.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tokio_postgres::Client;

use super::ProjectionError;
use super::materialize::{source_summary_for_block, source_summary_from_sources};
use super::shared::{
    BlockRow, DisplayMinerBasis, PoolObject, SourceOnlyRow, SourceRecord, SourceSummary,
    TreeCompetition, ensure_unknown_btc_target, load_sources, resolve_display_miner,
    stored_hash_from_display, unknown_pool,
};
use crate::error::ApiError;
use crate::normalize::ParentKind;
use crate::query::kind_as_str;

mod detail;
mod loaders;

use detail::{
    coinbase_tag_from_commitment, coinbase_tag_from_core_block, contributing_event_ids,
    derive_commitment, event_row_from_detail, header_projection, render_event_details,
};
use loaders::{
    BlockDetailRow, load_block_detail, load_competition_detail, load_event_details_by_hash,
    load_event_details_by_ids, load_pool_attributions_by_event, load_proof_details_for_hash,
    load_stale_branch,
};

/// The `/api/v1/block/{hash}` success envelope body. Every field name is the
/// locked JSON wire contract (pinned by fixtures/api/block-*.json).
/// `competition`, `stale_branch`, and `commitment` serialize as `null` (not
/// omitted) when absent.
#[derive(Debug, Clone, Serialize)]
pub struct BlockPayload {
    pub block: ApiBlock,
    pub proofs: Vec<ApiProof>,
    pub event_details: Vec<EventDetail>,
    pub competition: Option<TreeCompetition>,
    pub stale_branch: Option<BlockStaleBranch>,
    /// Parent-level AuxPoW merge-mining commitment. `None` (serialized as
    /// `null`, matching `competition` / `stale_branch`) only when the block has
    /// no recognized AuxPoW-format event.
    pub commitment: Option<Commitment>,
}

/// Parent-level merge-mining commitment for `/block/:hash`. `format` is always
/// one of the three string values when this is `Some`; `marker` is non-null only
/// for a Namecoin-family parent whose coinbase scriptSig yields a `fabe6d6d`
/// marker. The coinbase fields come from the single representative row
/// `derive_commitment` chose (the marker-source row when a marker decodes).
#[derive(Debug, Clone, Serialize)]
pub struct Commitment {
    pub format: &'static str,
    pub parent_coinbase_txid: Option<String>,
    pub parent_coinbase_script_hex: Option<String>,
    pub marker: Option<AuxMarkerProjection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuxMarkerProjection {
    pub magic_present: bool,
    /// Reversed/display hash order (the reverse of the raw scriptSig bytes),
    /// matching every other API hash field.
    pub aux_merkle_root: String,
    pub merkle_size: u32,
    pub merkle_nonce: u32,
}

/// The `block` member of `BlockPayload`. Field names are the locked wire
/// contract (pinned by block-*.json). `btc_orphan_class` and `coinbase_tag`
/// already carry their derivation contract below; the remaining fields are
/// direct read-model projections.
#[derive(Debug, Clone, Serialize)]
pub struct ApiBlock {
    pub hash: String,
    pub height: Option<i32>,
    pub kind: &'static str,
    /// Derived refinement of `kind='unknown'` (see `block.btc_orphan_class`):
    /// `strict_btc_orphan` / `weak_btc_orphan` / `btc_stale_excluded`, or `null`
    /// for canonical/stale blocks and for pending/never-Core-checked unknowns.
    pub btc_orphan_class: Option<String>,
    /// Printable raw tag runs from the commitment representative's Bitcoin
    /// coinbase scriptSig, or `null` when that representative has no recoverable
    /// coinbase script.
    pub coinbase_tag: Option<String>,
    pub header: HeaderProjection,
    pub bitcoin_miner_pool: PoolObject,
    /// Best-available miner: equals `bitcoin_miner_pool` when
    /// `display_miner_basis` is `bitcoin_coinbase`, otherwise the chain-agnostic
    /// child-inferred pool (or the unknown sentinel). Lets the UI label an
    /// RSK-only stale block with its merge-miner while `bitcoin_miner_pool`
    /// stays a strict Bitcoin-coinbase fact.
    pub display_miner_pool: PoolObject,
    /// `bitcoin_coinbase` | `child_inferred` | `unknown` (see
    /// `DisplayMinerBasis`).
    pub display_miner_basis: &'static str,
    pub source_summary: SourceSummary,
}

/// The decoded Bitcoin parent header for the wire `header` object (built by
/// `header_projection`). `prev_hash` / `merkle_root` are display-order hex
/// (rust-bitcoin `Display` reverses); `bits` is the 8-hex-digit compact target.
/// Pinned by block-*.json.
#[derive(Debug, Clone, Serialize)]
pub struct HeaderProjection {
    pub version: i32,
    pub prev_hash: String,
    pub merkle_root: String,
    pub time: u32,
    pub bits: String,
    pub nonce: u32,
}

/// One active attestation proof in `BlockPayload.proofs`. Field names are the
/// locked wire contract (block-*.json). Only non-revoked proofs reach here
/// (loader filters `revoked_at IS NULL`); `evidence` is the opaque per-proof
/// JSON blob.
#[derive(Debug, Clone, Serialize)]
pub struct ApiProof {
    pub kind: String,
    pub source: SourceRef,
    pub discovered_at: i64,
    pub confirmed_at: i64,
    pub revoked_at: Option<i64>,
    pub revocation_reason: Option<String>,
    pub pow_validates_btc_target: bool,
    pub evidence: Value,
}

/// The `source` object embedded in `ApiProof`. Wire field names locked
/// (block-*.json). `instance` distinguishes multiple sources of the same
/// code/kind.
#[derive(Debug, Clone, Serialize)]
pub struct SourceRef {
    pub id: i64,
    pub code: String,
    pub kind: String,
    pub chain: Option<String>,
    pub instance: Option<String>,
}

/// One merge_mining_event row in `BlockPayload.event_details`. Field names are
/// the locked wire contract (block-*.json). Several fields carry their own
/// derivation contract below (chain_id / slot_index / aux_proof are
/// Namecoin-family-only and parent-match-gated). Rendered and sorted by
/// `render_event_details`.
#[derive(Debug, Clone, Serialize)]
pub struct EventDetail {
    pub id: i64,
    pub source: String,
    pub child_chain: Option<String>,
    pub child_height: i32,
    pub child_block_hash: String,
    pub child_block_time: i64,
    pub btc_parent_header_hash: String,
    pub event_parent_kind: &'static str,
    pub btc_parent_coinbase_txid: Option<String>,
    pub btc_parent_coinbase_script_hex: Option<String>,
    pub btc_parent_coinbase_outputs_hex: Option<String>,
    pub child_coinbase_txid: Option<String>,
    pub child_coinbase_script_hex: Option<String>,
    pub aux_merkle_proof_hex: Option<String>,
    /// Reference AuxPoW chain id (Namecoin = 1; cite-or-null for others).
    /// Namecoin-family events only; `null` otherwise.
    pub chain_id: Option<u32>,
    /// This chain's slot (`nChainIndex`) in the parent's aux merkle tree,
    /// decoded from the stored CAuxPow blob. Namecoin-family events only, and
    /// only when the blob's embedded parent header matches this event's parent.
    pub slot_index: Option<u32>,
    /// The decoded CAuxPow merkle proofs (a human breakdown of the raw
    /// `aux_merkle_proof_hex` bytes). Same gate as `slot_index`.
    pub aux_proof: Option<AuxProofDetail>,
    pub rsk: Option<RskEventDetail>,
    pub pow_validates_btc_target: bool,
    pub pow_validates_child_target: Option<bool>,
    pub difficulty_epoch_ok: Option<bool>,
    pub event_discovered_at: i64,
    pub event_confirmed_at: i64,
    pub event_revoked_at: Option<i64>,
    pub event_revocation_reason: Option<String>,
    pub child_miner_pool: PoolObject,
    pub pool_attributions: EventPoolAttributions,
}

/// Per-event pool attributions split by side: `btc_parent` (parent coinbase) vs
/// `child_block` (child coinbase / payout registry). Wire field names locked
/// (block-*.json). `Default` (both empty) is the value for an event with no
/// attribution rows.
#[derive(Debug, Clone, Default, Serialize)]
pub struct EventPoolAttributions {
    pub btc_parent: Vec<EventPoolAttributionProjection>,
    pub child_block: Vec<EventPoolAttributionProjection>,
}

/// One pool-attribution match inside `EventPoolAttributions`. Wire field names
/// locked (block-*.json). `pool` resolves via
/// `COALESCE(pool_identity.pool_id, attribution.pool_id)`; `pool_identity` is
/// non-null only for identity-backed matches.
#[derive(Debug, Clone, Serialize)]
pub struct EventPoolAttributionProjection {
    pub namespace: String,
    pub match_kind: String,
    pub matched_value: String,
    pub pool: PoolObject,
    pub pool_identity: Option<PoolIdentityRef>,
    pub source: String,
    pub confidence: String,
    pub details: Value,
}

/// The decoded CAuxPow merkle proofs for one auxiliary block: the redundant
/// `hash_block` plus the two branches (`coinbase_branch` from the coinbase
/// txid to the parent transaction merkle root; `blockchain_branch` from the aux
/// block hash to the marker's `aux_merkle_root`). Replaces the opaque proof-byte
/// hex in the UI. All hashes are display-order hex.
#[derive(Debug, Clone, Serialize)]
pub struct AuxProofDetail {
    /// `CAuxPow::hashBlock`: a redundant convenience hash the verifier ignores,
    /// conventionally all-zero for Namecoin-family blocks. NOT the actual parent
    /// block hash (that is `block.hash` / `event.btc_parent_header_hash`).
    pub hash_block: String,
    pub coinbase_branch: AuxBranchDetail,
    pub blockchain_branch: AuxBranchDetail,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuxBranchDetail {
    /// The branch side-mask / index (`nIndex`); for `blockchain_branch` this is
    /// the chain's slot index. Non-negative (the decoder rejects negatives).
    pub index: u32,
    pub siblings: Vec<String>,
}

/// RSK merge-mining evidence for an `EventDetail` (RSK events only; `null`
/// otherwise). Wire field names locked (block-rsk-near.json,
/// block-rsk-uncle-near.json). `merge_mining_hash` / proof hex are encoded in
/// stored byte order (RSKIP-92 midstate form, no Display reversal);
/// `uncle_referencing_height` maps the sidecar's `uncle_parent_height`.
#[derive(Debug, Clone, Serialize)]
pub struct RskEventDetail {
    pub block_hash: String,
    pub height: i32,
    pub is_uncle: bool,
    pub uncle_index: Option<i32>,
    pub miner_address: String,
    pub pool_identity: Option<PoolIdentityRef>,
    pub merge_mining_hash: String,
    pub merkle_proof_hex: Option<String>,
    pub coinbase_tail_hex: Option<String>,
    pub proof_format: String,
    pub uncle_referencing_height: Option<i32>,
}

/// A resolved pool-identity reference (id + namespace + identifier) embedded in
/// attribution and RSK projections. Wire field names locked (block-rsk-*.json).
/// Reused across loaders.rs and detail.rs; `identifier` is lowercased for RSK
/// miner addresses.
#[derive(Debug, Clone, Serialize)]
pub struct PoolIdentityRef {
    pub id: i64,
    pub namespace: String,
    pub identifier: String,
}

/// The `stale_branch` member of `BlockPayload`, present only for
/// `kind='stale'`. Wire field names locked (block-one-block-stale.json,
/// block-multi-block-stale-branch.json). Members are height-ordered,
/// root-to-tip; `position` is one of root_and_tip/root/tip/interior relative to
/// the selected block. Built by `load_stale_branch`.
#[derive(Debug, Clone, Serialize)]
pub struct BlockStaleBranch {
    pub branch_id: String,
    pub root_hash: String,
    pub tip_hash: String,
    pub member_hashes: Vec<String>,
    pub canonical_competitor_hashes: Vec<String>,
    pub btc_height_min: i32,
    pub btc_height_max: i32,
    pub depth: usize,
    pub position: &'static str,
    pub parent_stale_hash: Option<String>,
    pub child_stale_hashes: Vec<String>,
}

/// `/api/v1/block/{hash}` projection entry. Routes on read-model presence: a
/// `block` row hydrates the full canonical/stale/unknown payload
/// (`block_from_read_model`); absence falls through to direct
/// merge_mining_event projection (`block_from_direct_events`, near/unknown).
/// `hash` is display-order hex, decoded to stored byte order via
/// `stored_hash_from_display` before any lookup. Pinned by
/// fixtures/api/block-*.json.
pub async fn block(client: &Client, hash: &str) -> Result<BlockPayload, ProjectionError> {
    let hash_bytes = stored_hash_from_display(hash)?;
    let block_row = load_block_detail(client, &hash_bytes).await?;
    let sources = load_sources(client).await?;

    match block_row {
        Some(row) => block_from_read_model(client, hash, row, &sources).await,
        None => block_from_direct_events(client, hash, &hash_bytes).await,
    }
}

/// Assemble the parent-projection source summary for the block detail view
/// from the read-model row plus its event/proof/observation hydrations.
fn block_source_summary(
    row: &BlockDetailRow,
    event_details: &[loaders::EventDetailRow],
    proofs: &[ApiProof],
    sources: &HashMap<String, SourceRecord>,
) -> SourceSummary {
    let block_row = BlockRow {
        hash: row.hash.clone(),
        prev_hash: row.prev_hash.clone(),
        height: row.height,
        kind: row.kind,
        btc_orphan_class: row.btc_orphan_class.clone(),
        header_time: row.header_time,
        bitcoin_miner_pool: row.bitcoin_miner_pool.clone(),
        live_observed: row.live_observed,
        core_attested: row.core_attested,
        pow_validated: row.pow_validated,
    };
    let event_rows = event_details
        .iter()
        .map(event_row_from_detail)
        .collect::<Vec<_>>();
    let proof_sources = proofs
        .iter()
        .map(|proof| SourceOnlyRow {
            parent_hash: row.hash.clone(),
            source: SourceRecord {
                id: proof.source.id,
                code: proof.source.code.clone(),
                kind: proof.source.kind.clone(),
                chain: proof.source.chain.clone(),
            },
        })
        .collect::<Vec<_>>();
    source_summary_for_block(&block_row, &event_rows, &proof_sources, sources, &[])
}

/// Hydrate `BlockPayload` for a block that has a read-model `block` row.
/// Canonical/stale events are gathered via proof `contributing_event_ids`;
/// unknown via direct hash lookup. The proof/event reads are NOT one
/// repeatable-read transaction (see inline note), so a mid-read reconcile
/// renders the active subset rather than 500-ing. competition/stale_branch only
/// populate for stale; coinbase_tag prefers the Core-attested stored script
/// before the commitment fallback.
async fn block_from_read_model(
    client: &Client,
    hash: &str,
    row: BlockDetailRow,
    sources: &HashMap<String, SourceRecord>,
) -> Result<BlockPayload, ProjectionError> {
    let proofs = load_proof_details_for_hash(client, &row.hash).await?;
    let event_details = if matches!(row.kind, ParentKind::Canonical | ParentKind::Stale) {
        let ids = contributing_event_ids(&proofs)?;
        // Revoke/restore reconciles proofs and events transactionally, but this
        // read path is not itself a repeatable-read transaction. If a concurrent
        // reconcile lands between the proof read and event hydration, render the
        // active event subset rather than turning a transient snapshot skew into
        // a 500.
        load_event_details_by_ids(client, &ids).await?
    } else {
        load_event_details_by_hash(client, &row.hash).await?
    };
    ensure_unknown_btc_target(
        row.kind,
        event_details
            .iter()
            .map(|event| event.pow_validates_btc_target),
        "active block-backed unknown event fails Bitcoin target",
    )
    .map_err(ProjectionError::Internal)?;

    let source_summary = block_source_summary(&row, &event_details, &proofs, sources);
    let competition = if row.kind == ParentKind::Stale {
        load_competition_detail(client, &row).await?
    } else {
        None
    };
    let stale_branch = if row.kind == ParentKind::Stale {
        Some(load_stale_branch(client, &row.hash).await?)
    } else {
        None
    };
    let commitment = derive_commitment(&event_details);
    let coinbase_tag = coinbase_tag_from_core_block(&row)
        .or_else(|| coinbase_tag_from_commitment(commitment.as_ref()));
    let event_ids = event_details
        .iter()
        .map(|event| event.id)
        .collect::<Vec<_>>();
    let pool_attributions = load_pool_attributions_by_event(client, &event_ids).await?;
    let event_details = render_event_details(event_details, &pool_attributions)?;
    let (display_miner_pool, display_miner_basis) = resolve_display_miner(
        &row.bitcoin_miner_pool,
        event_details.iter().map(|event| &event.child_miner_pool),
    );

    Ok(BlockPayload {
        block: ApiBlock {
            hash: hash.to_owned(),
            height: row.height,
            kind: kind_as_str(row.kind),
            btc_orphan_class: row.btc_orphan_class.clone(),
            coinbase_tag,
            header: header_projection(&row.header_bytes)?,
            bitcoin_miner_pool: row.bitcoin_miner_pool,
            display_miner_pool,
            display_miner_basis: display_miner_basis.as_str(),
            source_summary,
        },
        proofs,
        event_details,
        competition,
        stale_branch,
        commitment,
    })
}

/// Hydrate `BlockPayload` for a hash with merge_mining_event rows but no
/// read-model `block` row (near/unknown). 404s when no events exist. `kind` is
/// unknown if any event is unknown, else near;
/// height/orphan_class/proofs/competition/stale_branch are all
/// absent by construction. Header comes from the lowest-id event's stored
/// parent header.
async fn block_from_direct_events(
    client: &Client,
    hash: &str,
    hash_bytes: &[u8],
) -> Result<BlockPayload, ProjectionError> {
    let events = load_event_details_by_hash(client, hash_bytes).await?;
    if events.is_empty() {
        return Err(ProjectionError::Api(ApiError::not_found(hash)));
    }
    let kind = if events.iter().any(|event| event.kind == ParentKind::Unknown) {
        ParentKind::Unknown
    } else {
        ParentKind::Near
    };
    ensure_unknown_btc_target(
        kind,
        events.iter().map(|event| event.pow_validates_btc_target),
        "active direct unknown event fails Bitcoin target",
    )
    .map_err(ProjectionError::Internal)?;
    let first = events
        .iter()
        .min_by_key(|event| event.id)
        .expect("empty handled above");
    let source_summary = source_summary_from_sources(
        events.iter().map(|event| &event.source),
        kind != ParentKind::Near,
    );
    let bitcoin_miner_pool = unknown_pool();
    let header = header_projection(&first.parent_header_bytes)?;
    let commitment = derive_commitment(&events);
    let coinbase_tag = coinbase_tag_from_commitment(commitment.as_ref());
    let event_ids = events.iter().map(|event| event.id).collect::<Vec<_>>();
    let pool_attributions = load_pool_attributions_by_event(client, &event_ids).await?;
    let rendered_events = render_event_details(events, &pool_attributions)?;

    Ok(BlockPayload {
        block: ApiBlock {
            hash: hash.to_owned(),
            height: None,
            kind: kind_as_str(kind),
            // A direct-projected block has no read-model `block` row, so it has
            // no Core-gated orphan class (pending by construction).
            btc_orphan_class: None,
            coinbase_tag,
            header,
            bitcoin_miner_pool,
            // Near/unknown direct-event parents are not validated Bitcoin blocks,
            // so we never infer a miner for them: display stays unknown.
            display_miner_pool: unknown_pool(),
            display_miner_basis: DisplayMinerBasis::Unknown.as_str(),
            source_summary,
        },
        proofs: Vec::new(),
        event_details: rendered_events,
        competition: None,
        stale_branch: None,
        commitment,
    })
}
