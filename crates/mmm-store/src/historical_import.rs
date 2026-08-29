//! Read-only database state used to plan historical publication imports.

use anyhow::{Context, Result};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, Row, RowStream};

/// One stored publication-provenance row with compact hashes for large fields.
#[derive(Debug)]
pub struct HistoricalPublicationStateRow {
    pub event_id: i64,
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
    pub child_height: Option<i32>,
    pub child_block_hash: Option<Vec<u8>>,
    pub child_header_sha256: Option<Vec<u8>>,
    pub child_block_time: Option<i64>,
    pub child_nbits: Option<i64>,
    pub pow_validates_child_target: Option<bool>,
    pub btc_parent_header_hash: Vec<u8>,
    pub btc_parent_prev_header_hash: Vec<u8>,
    pub btc_parent_header_time: i64,
    pub btc_parent_coinbase_txid: Option<Vec<u8>>,
    pub btc_parent_coinbase_script_sha256: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs_sha256: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs_text_sha256: Option<Vec<u8>>,
    pub btc_parent_coinbase_tx_sha256: Option<Vec<u8>>,
    pub revoked_at: Option<i64>,
    pub revocation_reason: Option<String>,
    pub rsk_block_hash: Option<Vec<u8>>,
    pub rsk_height: Option<i32>,
    pub rsk_is_uncle: Option<bool>,
    pub rsk_uncle_index: Option<i32>,
    pub rsk_uncle_parent_height: Option<i32>,
    pub rsk_miner: Option<Vec<u8>>,
    pub rsk_merge_mining_hash: Option<Vec<u8>>,
    pub rsk_merkle_proof_sha256: Option<Vec<u8>>,
    pub rsk_coinbase_tail_sha256: Option<Vec<u8>>,
    pub rsk_proof_format: Option<String>,
    pub error_block_reason: Option<String>,
}

impl HistoricalPublicationStateRow {
    /// Decode the stable projection returned by
    /// [`stream_historical_publication_state`].
    pub fn from_row(row: &Row) -> Self {
        Self {
            chain: row.get(0),
            source_kind: row.get(1),
            source_path: row.get(2),
            source_row_number: row.get(3),
            artifact_scope: row.get(4),
            provenance: row.get(5),
            classification: row.get(6),
            btc_height: row.get(7),
            validation_status: row.get(8),
            btc_stale_relevance: row.get(9),
            relevance_reason: row.get(10),
            child_height: row.get(11),
            child_block_hash: row.get(12),
            child_header_sha256: row.get(13),
            child_block_time: row.get(14),
            child_nbits: row.get(15),
            pow_validates_child_target: row.get(16),
            btc_parent_header_hash: row.get(17),
            btc_parent_prev_header_hash: row.get(18),
            btc_parent_header_time: row.get(19),
            btc_parent_coinbase_txid: row.get(20),
            btc_parent_coinbase_script_sha256: row.get(21),
            btc_parent_coinbase_outputs_sha256: row.get(22),
            btc_parent_coinbase_outputs_text_sha256: row.get(23),
            btc_parent_coinbase_tx_sha256: row.get(24),
            revoked_at: row.get(25),
            revocation_reason: row.get(26),
            rsk_block_hash: row.get(27),
            rsk_height: row.get(28),
            rsk_is_uncle: row.get(29),
            rsk_uncle_index: row.get(30),
            rsk_uncle_parent_height: row.get(31),
            rsk_miner: row.get(32),
            rsk_merge_mining_hash: row.get(33),
            rsk_merkle_proof_sha256: row.get(34),
            rsk_coinbase_tail_sha256: row.get(35),
            rsk_proof_format: row.get(36),
            error_block_reason: row.get(37),
            event_id: row.get(38),
        }
    }
}

/// Stream all non-operator publication provenance across research pins.
pub async fn stream_historical_publication_state(client: &Client) -> Result<RowStream> {
    client
        .query_raw(
            "SELECT p.chain, p.source_kind, p.source_path, p.source_row_number, \
                    p.artifact_scope, p.provenance, p.classification, p.btc_height, \
                    p.validation_status, p.btc_stale_relevance, p.relevance_reason, \
                    e.child_height, e.child_block_hash, sha256(e.child_header_bytes), \
                    e.child_block_time, e.child_nbits, e.pow_validates_child_target, \
                    e.btc_parent_header_hash, e.btc_parent_prev_header_hash, \
                    e.btc_parent_header_time, e.btc_parent_coinbase_txid, \
                    sha256(e.btc_parent_coinbase_script), \
                    sha256(e.btc_parent_coinbase_outputs), \
                    sha256(convert_to(e.btc_parent_coinbase_outputs_text, 'UTF8')), \
                    sha256(e.btc_parent_coinbase_tx_bytes), \
                    e.revoked_at, e.revocation_reason, \
                    r.rsk_block_hash, r.rsk_height, r.is_uncle, r.uncle_index, \
                    r.uncle_parent_height, r.rsk_miner, r.merge_mining_hash, \
                    sha256(r.merkle_proof), sha256(r.coinbase_tail), r.proof_format, \
                    NULL::text, e.id \
             FROM historical_event_provenance p \
             JOIN merge_mining_event e ON e.id = p.event_id \
             LEFT JOIN rsk_merge_mining_evidence r ON r.event_id = e.id \
             WHERE p.publication_ref <> 'operator-csv' \
               AND p.artifact_scope <> 'error-block-observations' \
             ORDER BY p.chain, p.source_path, p.source_row_number, p.publication_ref",
            std::iter::empty::<&(dyn ToSql + Sync)>(),
        )
        .await
        .context("stream historical publication state")
}

/// Stream the small retained error-observation provenance set, including the
/// catalogue reason currently materialized on its parent block.
pub async fn stream_historical_error_observation_state(client: &Client) -> Result<RowStream> {
    client
        .query_raw(
            "SELECT p.chain, p.source_kind, p.source_path, p.source_row_number, \
                    p.artifact_scope, p.provenance, p.classification, p.btc_height, \
                    p.validation_status, p.btc_stale_relevance, p.relevance_reason, \
                    e.child_height, e.child_block_hash, sha256(e.child_header_bytes), \
                    e.child_block_time, e.child_nbits, e.pow_validates_child_target, \
                    e.btc_parent_header_hash, e.btc_parent_prev_header_hash, \
                    e.btc_parent_header_time, e.btc_parent_coinbase_txid, \
                    sha256(e.btc_parent_coinbase_script), \
                    sha256(e.btc_parent_coinbase_outputs), \
                    sha256(convert_to(e.btc_parent_coinbase_outputs_text, 'UTF8')), \
                    sha256(e.btc_parent_coinbase_tx_bytes), \
                    e.revoked_at, e.revocation_reason, \
                    r.rsk_block_hash, r.rsk_height, r.is_uncle, r.uncle_index, \
                    r.uncle_parent_height, r.rsk_miner, r.merge_mining_hash, \
                    sha256(r.merkle_proof), sha256(r.coinbase_tail), r.proof_format, \
                    b.error_block_reason, e.id \
             FROM historical_event_provenance p \
             JOIN merge_mining_event e ON e.id = p.event_id \
             LEFT JOIN rsk_merge_mining_evidence r ON r.event_id = e.id \
             LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
             WHERE p.publication_ref <> 'operator-csv' \
               AND p.artifact_scope = 'error-block-observations' \
             ORDER BY p.chain, p.source_path, p.source_row_number, p.publication_ref",
            std::iter::empty::<&(dyn ToSql + Sync)>(),
        )
        .await
        .context("stream historical error-observation state")
}

/// One base event used to detect authoritative extras without provenance.
#[derive(Debug)]
pub struct HistoricalBaseEventRow {
    pub event_id: i64,
    pub chain: String,
    pub child_height: Option<i32>,
    pub child_block_hash: Option<Vec<u8>>,
    pub btc_parent_header_hash: Vec<u8>,
}

impl HistoricalBaseEventRow {
    pub fn from_row(row: &Row) -> Self {
        Self {
            chain: row.get(0),
            child_height: row.get(1),
            child_block_hash: row.get(2),
            btc_parent_header_hash: row.get(3),
            event_id: row.get(4),
        }
    }
}

/// Stream every event owned by authoritative historical/partial sources.
/// The caller removes the bounded set of retained error-observation event IDs
/// before comparing this authoritative base set.
pub async fn stream_authoritative_historical_base_events(
    client: &Client,
    source_ids: &[i64],
) -> Result<RowStream> {
    let params: [&(dyn ToSql + Sync); 1] = [&source_ids];
    client
        .query_raw(
            "SELECT s.chain, e.child_height, e.child_block_hash, \
                    e.btc_parent_header_hash, e.id \
             FROM merge_mining_event e \
             JOIN source s ON s.id = e.source_id \
             WHERE e.source_id = ANY($1) \
             ORDER BY s.chain, e.id",
            params,
        )
        .await
        .context("stream authoritative historical base events")
}

/// Derived historical work that must be completed even when publication rows
/// already match the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoricalFinalizationState {
    pub reconcile_pending: bool,
    pub source_health_ready: bool,
    pub published_stale_pending: bool,
}

impl HistoricalFinalizationState {
    pub const fn required(self) -> bool {
        self.reconcile_pending || !self.source_health_ready || self.published_stale_pending
    }
}

pub async fn load_historical_finalization_state(
    client: &Client,
) -> Result<HistoricalFinalizationState> {
    let row = client
        .query_one(
            "SELECT \
                EXISTS (SELECT 1 FROM historical_reconcile_queue), \
                (SELECT source_health_ready FROM read_model_invariant WHERE id = TRUE), \
                EXISTS ( \
                    SELECT 1 \
                    FROM merge_mining_event e \
                    JOIN historical_event_provenance p ON p.event_id = e.id \
                    LEFT JOIN block b ON b.btc_header_hash = e.btc_parent_header_hash \
                    WHERE e.revoked_at IS NULL \
                      AND p.relevance_reason IN ( \
                          'valid_direct_stale', 'valid_stale_descendant' \
                      ) \
                      AND (b.kind IS NULL OR b.kind = 'unknown') \
                )",
            &[],
        )
        .await
        .context("load historical finalization state")?;
    Ok(HistoricalFinalizationState {
        reconcile_pending: row.get(0),
        source_health_ready: row.get(1),
        published_stale_pending: row.get(2),
    })
}
