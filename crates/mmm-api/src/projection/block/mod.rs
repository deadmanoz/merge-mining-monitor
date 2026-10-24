//! Block-detail projection for `/api/v1/block/:hash` (`fixtures/api/block-*.json`).

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
mod near;
mod protocol_commitments;

use near::ObservationDetail;
pub use protocol_commitments::ProtocolCommitments;
use protocol_commitments::load_protocol_commitments;

use detail::{
    coinbase_tag_from_commitment, coinbase_tag_from_core_block, contributing_event_ids,
    derive_commitment, event_row_from_detail, header_projection, render_event_details,
};
use loaders::{
    BlockDetailRow, load_block_detail, load_competition_detail, load_event_details_by_hash,
    load_event_details_by_ids, load_pool_attributions_by_event, load_proof_details_for_hash,
    load_stale_branch,
};

#[derive(Debug, Clone, Serialize)]
pub struct BlockPayload {
    pub block: ApiBlock,
    pub proofs: Vec<ApiProof>,
    pub event_details: Vec<EventDetail>,
    /// A near parent's child block observations (it has no events); omitted when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub observations: Vec<ObservationDetail>,
    pub competition: Option<TreeCompetition>,
    pub stale_branch: Option<BlockStaleBranch>,
    pub commitment: Option<Commitment>,
    /// CORE and exSat coinbase declarations; `null` until derived for the running decoder.
    pub protocol_commitments: Option<ProtocolCommitments>,
}

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
    pub aux_merkle_root: String,
    pub merkle_size: u32,
    pub merkle_nonce: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiBlock {
    pub hash: String,
    pub height: Option<i32>,
    pub kind: &'static str,
    /// `strict_btc_orphan` | `weak_btc_orphan` | `excluded` when `kind='unknown'` and Core-checked.
    pub btc_orphan_class: Option<String>,
    /// Present only when `kind='invalid_block'`.
    pub invalid_block_reason: Option<String>,
    pub coinbase_tag: Option<String>,
    pub header: HeaderProjection,
    pub bitcoin_miner_pool: PoolObject,
    pub display_miner_pool: PoolObject,
    pub display_miner_basis: &'static str,
    pub source_summary: SourceSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct HeaderProjection {
    pub version: i32,
    pub prev_hash: String,
    pub merkle_root: String,
    pub time: u32,
    pub bits: String,
    pub nonce: u32,
}

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

#[derive(Debug, Clone, Serialize)]
pub struct SourceRef {
    pub id: i64,
    pub code: String,
    pub kind: String,
    pub chain: Option<String>,
    pub instance: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct EventDetail {
    pub id: i64,
    pub source: String,
    pub child_chain: Option<String>,
    pub child_height: Option<i32>,
    pub child_block_hash: Option<String>,
    pub child_header_hex: Option<String>,
    pub child_block_time: Option<i64>,
    pub child_nbits: Option<String>,
    /// Set and cleared together when a child chain replaces this event's block at `child_height`.
    pub child_displaced_at: Option<i64>,
    pub child_displaced_by: Option<String>,
    pub btc_parent_header_hash: String,
    pub event_parent_kind: &'static str,
    pub btc_parent_coinbase_txid: Option<String>,
    pub btc_parent_coinbase_script_hex: Option<String>,
    pub btc_parent_coinbase_outputs_hex: Option<String>,
    pub child_coinbase_txid: Option<String>,
    pub child_coinbase_script_hex: Option<String>,
    pub aux_merkle_proof_hex: Option<String>,
    pub chain_id: Option<u32>,
    pub slot_index: Option<u32>,
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct EventPoolAttributions {
    pub btc_parent: Vec<EventPoolAttributionProjection>,
    pub child_block: Vec<EventPoolAttributionProjection>,
}

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

#[derive(Debug, Clone, Serialize)]
pub struct AuxProofDetail {
    /// Classic `CAuxPow::hashBlock` (omitted for Qbit-format proofs, which have no such wire field).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash_block: Option<String>,
    pub coinbase_branch: AuxBranchDetail,
    pub blockchain_branch: AuxBranchDetail,
}

#[derive(Debug, Clone, Serialize)]
pub struct AuxBranchDetail {
    pub index: u32,
    pub siblings: Vec<String>,
}

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

#[derive(Debug, Clone, Serialize)]
pub struct PoolIdentityRef {
    pub id: i64,
    pub namespace: String,
    pub identifier: String,
}

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

/// `/api/v1/block/{hash}` projection entry: the `block` read model, else active events (an
/// `unknown` parent), else near observations (a `near` parent), else not found.
pub async fn block(client: &Client, hash: &str) -> Result<BlockPayload, ProjectionError> {
    let hash_bytes = stored_hash_from_display(hash)?;
    let block_row = load_block_detail(client, &hash_bytes).await?;
    let sources = load_sources(client).await?;

    if let Some(row) = block_row {
        return block_from_read_model(client, hash, row, &sources).await;
    }
    if let Some(payload) = block_from_direct_events(client, hash, &hash_bytes).await? {
        return Ok(payload);
    }
    near::block_from_near_observations(client, hash, &hash_bytes)
        .await?
        .ok_or_else(|| ProjectionError::Api(ApiError::not_found(hash)))
}

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

async fn block_from_read_model(
    client: &Client,
    hash: &str,
    row: BlockDetailRow,
    sources: &HashMap<String, SourceRecord>,
) -> Result<BlockPayload, ProjectionError> {
    let proofs = load_proof_details_for_hash(client, &row.hash).await?;
    let event_details = if matches!(row.kind, ParentKind::Canonical | ParentKind::Stale) {
        let ids = contributing_event_ids(&proofs)?;
        // Proof and event reads are not in one repeatable-read transaction; render the active subset on concurrent reconcile.
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
    let protocol_commitments = load_protocol_commitments(client, &row.hash).await?;
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
            invalid_block_reason: row.invalid_block_reason.clone(),
            coinbase_tag,
            header: header_projection(&row.header_bytes)?,
            bitcoin_miner_pool: row.bitcoin_miner_pool,
            display_miner_pool,
            display_miner_basis: display_miner_basis.as_str(),
            source_summary,
        },
        proofs,
        event_details,
        observations: Vec::new(),
        competition,
        stale_branch,
        commitment,
        protocol_commitments,
    })
}

async fn block_from_direct_events(
    client: &Client,
    hash: &str,
    hash_bytes: &[u8],
) -> Result<Option<BlockPayload>, ProjectionError> {
    let events = load_event_details_by_hash(client, hash_bytes).await?;
    if events.is_empty() {
        return Ok(None);
    }
    // Without a `block` row, an event-backed parent is unknown: a near has no events.
    let kind = ParentKind::Unknown;
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
    let source_summary =
        source_summary_from_sources(events.iter().map(|event| &event.source), true);
    let bitcoin_miner_pool = unknown_pool();
    let header = header_projection(&first.parent_header_bytes)?;
    let commitment = derive_commitment(&events);
    let coinbase_tag = coinbase_tag_from_commitment(commitment.as_ref());
    let event_ids = events.iter().map(|event| event.id).collect::<Vec<_>>();
    let pool_attributions = load_pool_attributions_by_event(client, &event_ids).await?;
    let rendered_events = render_event_details(events, &pool_attributions)?;

    Ok(Some(BlockPayload {
        block: ApiBlock {
            hash: hash.to_owned(),
            height: None,
            kind: kind_as_str(kind),
            // A direct-projected block has no read-model `block` row, so it has
            // no Core-gated orphan class (pending by construction).
            btc_orphan_class: None,
            invalid_block_reason: None,
            coinbase_tag,
            header,
            bitcoin_miner_pool,
            // Direct-event unknown parents are not validated Bitcoin blocks,
            // so we never infer a miner for them: display stays unknown.
            display_miner_pool: unknown_pool(),
            display_miner_basis: DisplayMinerBasis::Unknown.as_str(),
            source_summary,
        },
        proofs: Vec::new(),
        event_details: rendered_events,
        observations: Vec::new(),
        competition: None,
        stale_branch: None,
        commitment,
        // The `block` row and its derived commitments are written together, so a parent
        // without one has none yet.
        protocol_commitments: None,
    }))
}
