//! The block page for a near parent: a parent that misses Bitcoin's target, recorded only by
//! the child block observations that name it.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::{Client, Row};

use super::super::ProjectionError;
use super::super::materialize::source_summary_from_sources;
use super::super::shared::{
    DisplayMinerBasis, PoolObject, SourceRecord, display_child_block_hash, pool_from_columns,
    unknown_pool,
};
use super::detail::{
    ParentCoinbase, coinbase_tag_from_commitment, derive_commitment, display_hash_opt,
    header_projection,
};
use super::{ApiBlock, BlockPayload, PoolIdentityRef};
use crate::normalize::ParentKind;
use crate::query::kind_as_str;

/// One child block's observation of a near parent.
#[derive(Debug, Clone, Serialize)]
pub struct ObservationDetail {
    pub observation_id: i64,
    pub source: String,
    pub child_chain: Option<String>,
    pub child_height: Option<i32>,
    pub child_block_hash: Option<String>,
    pub child_header_hex: Option<String>,
    pub child_block_time: Option<i64>,
    pub child_nbits: Option<String>,
    /// `canonical` | `uncle` where the child chain reports it.
    pub child_chain_role: Option<String>,
    pub uncle_index: Option<i32>,
    pub uncle_parent_height: Option<i32>,
    /// Set and cleared together when the child chain replaces this block at `child_height`.
    pub child_displaced_at: Option<i64>,
    pub child_displaced_by: Option<String>,
    pub btc_parent_coinbase_txid: Option<String>,
    pub btc_parent_coinbase_script_hex: Option<String>,
    pub btc_parent_coinbase_outputs_hex: Option<String>,
    pub child_coinbase_txid: Option<String>,
    pub child_coinbase_script_hex: Option<String>,
    pub child_coinbase_outputs_hex: Option<String>,
    pub pow_validates_child_target: Option<bool>,
    pub recorded_at: i64,
    pub child_identities: Vec<ChildIdentityProjection>,
    pub child_pool: PoolObject,
    pub btc_parent_pool: PoolObject,
}

/// A raw child-side identifier the block carries, with the registry entry it resolves to.
#[derive(Debug, Clone, Serialize)]
pub struct ChildIdentityProjection {
    pub namespace: String,
    pub identifier: String,
    pub pool_identity: Option<PoolIdentityRef>,
    pub pool: PoolObject,
}

struct NearObservationRow {
    id: i64,
    source: SourceRecord,
    child_height: Option<i32>,
    child_block_hash: Option<Vec<u8>>,
    child_header_bytes: Option<Vec<u8>>,
    child_block_time: Option<i64>,
    child_nbits: Option<i64>,
    child_chain_role: Option<String>,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
    child_displaced_at: Option<i64>,
    child_displaced_by: Option<Vec<u8>>,
    parent_header_bytes: Vec<u8>,
    btc_parent_coinbase_txid: Option<Vec<u8>>,
    btc_parent_coinbase_script: Option<Vec<u8>>,
    btc_parent_coinbase_outputs: Option<Vec<u8>>,
    child_coinbase_txid: Option<Vec<u8>>,
    child_coinbase_script: Option<Vec<u8>>,
    child_coinbase_outputs: Option<Vec<u8>>,
    pow_validates_child_target: Option<bool>,
    recorded_at: i64,
    child_pool: PoolObject,
    btc_parent_pool: PoolObject,
    child_identity_ids: Vec<i64>,
}

impl ParentCoinbase for NearObservationRow {
    fn order(&self) -> i64 {
        self.id
    }
    fn chain(&self) -> Option<&str> {
        self.source.chain.as_deref()
    }
    fn parent_coinbase_txid(&self) -> Option<&[u8]> {
        self.btc_parent_coinbase_txid.as_deref()
    }
    fn parent_coinbase_script(&self) -> Option<&[u8]> {
        self.btc_parent_coinbase_script.as_deref()
    }
}

impl NearObservationRow {
    fn from_row(row: &Row) -> Self {
        Self {
            id: row.get(0),
            source: SourceRecord {
                id: row.get(1),
                code: row.get(2),
                kind: row.get(3),
                chain: row.get(4),
            },
            child_height: row.get(5),
            child_block_hash: row.get(6),
            child_header_bytes: row.get(7),
            child_block_time: row.get(8),
            child_nbits: row.get(9),
            child_chain_role: row.get(10),
            uncle_index: row.get(11),
            uncle_parent_height: row.get(12),
            child_displaced_at: row.get(13),
            child_displaced_by: row.get(14),
            parent_header_bytes: row.get(15),
            btc_parent_coinbase_txid: row.get(16),
            btc_parent_coinbase_script: row.get(17),
            btc_parent_coinbase_outputs: row.get(18),
            child_coinbase_txid: row.get(19),
            child_coinbase_script: row.get(20),
            child_coinbase_outputs: row.get(21),
            pow_validates_child_target: row.get(22),
            recorded_at: row.get(23),
            child_pool: pool_from_columns(row.get(24), row.get(25), row.get(26)),
            btc_parent_pool: pool_from_columns(row.get(27), row.get(28), row.get(29)),
            child_identity_ids: row.get(30),
        }
    }
}

/// The page for a parent only near observations name, or `None` when there are none.
pub(super) async fn block_from_near_observations(
    client: &Client,
    hash: &str,
    hash_bytes: &[u8],
) -> Result<Option<BlockPayload>, ProjectionError> {
    let rows = load_near_observations(client, hash_bytes).await?;
    let Some(first) = rows.iter().min_by_key(|row| row.id) else {
        return Ok(None);
    };
    let header = header_projection(&first.parent_header_bytes)?;
    let commitment = derive_commitment(&rows);
    let coinbase_tag = coinbase_tag_from_commitment(commitment.as_ref());
    let bitcoin_miner_pool = rows
        .iter()
        .map(|row| &row.btc_parent_pool)
        .find(|pool| pool.known)
        .cloned()
        .unwrap_or_else(unknown_pool);
    let source_summary = source_summary_from_sources(rows.iter().map(|row| &row.source), false);
    let identities = load_child_identities(client, &rows).await?;
    let observations = render_observations(rows, &identities)?;
    Ok(Some(BlockPayload {
        block: ApiBlock {
            hash: hash.to_owned(),
            height: None,
            kind: kind_as_str(ParentKind::Near),
            btc_orphan_class: None,
            error_block_reason: None,
            coinbase_tag,
            header,
            bitcoin_miner_pool,
            // A near is not a valid Bitcoin block, so no miner is inferred for display.
            display_miner_pool: unknown_pool(),
            display_miner_basis: DisplayMinerBasis::Unknown.as_str(),
            source_summary,
        },
        proofs: Vec::new(),
        event_details: Vec::new(),
        observations,
        competition: None,
        stale_branch: None,
        commitment,
    }))
}

async fn load_near_observations(client: &Client, hash: &[u8]) -> Result<Vec<NearObservationRow>> {
    let rows = client
        .query(
            "SELECT o.id, s.id, s.code, s.kind, s.chain, \
                    o.child_height, o.child_block_hash, o.child_header_bytes, \
                    o.child_block_time, o.child_nbits, \
                    o.child_chain_role, o.uncle_index, o.uncle_parent_height, \
                    o.child_displaced_at, o.child_displaced_by, o.btc_parent_header_bytes, \
                    o.btc_parent_coinbase_txid, o.btc_parent_coinbase_script, \
                    o.btc_parent_coinbase_outputs, \
                    o.child_coinbase_txid, o.child_coinbase_script, o.child_coinbase_outputs, \
                    o.pow_validates_child_target, o.recorded_at, \
                    cp.id, cp.slug, cp.canonical_name, bp.id, bp.slug, bp.canonical_name, \
                    o.child_identity_ids \
             FROM child_block_observation o \
             JOIN source s ON s.id = o.source_id \
             LEFT JOIN pool cp ON cp.id = o.child_pool_id \
             LEFT JOIN pool bp ON bp.id = o.btc_parent_pool_id \
             WHERE o.btc_parent_header_hash = $1 AND o.observation_kind = 'near' \
             ORDER BY o.recorded_at, s.code, o.child_height NULLS LAST, \
                      o.child_block_hash NULLS LAST, o.id",
            &[&hash],
        )
        .await
        .context("load near observations by parent hash")?;
    Ok(rows.iter().map(NearObservationRow::from_row).collect())
}

/// Each referenced child identity with the registry entry and pool it resolves to.
async fn load_child_identities(
    client: &Client,
    rows: &[NearObservationRow],
) -> Result<HashMap<i64, ChildIdentityProjection>> {
    let ids = rows
        .iter()
        .flat_map(|row| row.child_identity_ids.iter().copied())
        .collect::<Vec<_>>();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = client
        .query(
            "SELECT ci.id, ci.namespace, ci.identifier, pi.id, \
                    p.id, p.slug, p.canonical_name \
             FROM child_miner_identity ci \
             LEFT JOIN pool_identity pi \
               ON pi.namespace = ci.namespace AND pi.identifier = ci.identifier \
             LEFT JOIN pool p ON p.id = pi.pool_id \
             WHERE ci.id = ANY($1::bigint[])",
            &[&ids],
        )
        .await
        .context("load near observation child identities")?;
    Ok(rows
        .iter()
        .map(|row| {
            let namespace: String = row.get(1);
            let identifier: String = row.get(2);
            let pool_identity = row.get::<_, Option<i64>>(3).map(|id| PoolIdentityRef {
                id,
                namespace: namespace.clone(),
                identifier: identifier.clone(),
            });
            let identity = ChildIdentityProjection {
                namespace,
                identifier,
                pool_identity,
                pool: pool_from_columns(row.get(4), row.get(5), row.get(6)),
            };
            (row.get(0), identity)
        })
        .collect())
}

fn render_observations(
    rows: Vec<NearObservationRow>,
    identities: &HashMap<i64, ChildIdentityProjection>,
) -> Result<Vec<ObservationDetail>> {
    rows.into_iter()
        .map(|row| {
            let display = |hash: Option<&[u8]>| {
                hash.map(|bytes| display_child_block_hash(&row.source.code, bytes))
                    .transpose()
            };
            let mut child_identities = row
                .child_identity_ids
                .iter()
                .filter_map(|id| identities.get(id).cloned())
                .collect::<Vec<_>>();
            child_identities.sort_by(|a, b| {
                (a.namespace.as_str(), a.identifier.as_str())
                    .cmp(&(b.namespace.as_str(), b.identifier.as_str()))
            });
            Ok(ObservationDetail {
                observation_id: row.id,
                child_block_hash: display(row.child_block_hash.as_deref())?,
                child_displaced_by: display(row.child_displaced_by.as_deref())?,
                source: row.source.code,
                child_chain: row.source.chain,
                child_height: row.child_height,
                child_header_hex: row.child_header_bytes.map(hex::encode),
                child_block_time: row.child_block_time,
                child_nbits: row.child_nbits.map(|nbits| format!("{nbits:08x}")),
                child_chain_role: row.child_chain_role,
                uncle_index: row.uncle_index,
                uncle_parent_height: row.uncle_parent_height,
                child_displaced_at: row.child_displaced_at,
                btc_parent_coinbase_txid: display_hash_opt(row.btc_parent_coinbase_txid)?,
                btc_parent_coinbase_script_hex: row.btc_parent_coinbase_script.map(hex::encode),
                btc_parent_coinbase_outputs_hex: row.btc_parent_coinbase_outputs.map(hex::encode),
                child_coinbase_txid: display_hash_opt(row.child_coinbase_txid)?,
                child_coinbase_script_hex: row.child_coinbase_script.map(hex::encode),
                child_coinbase_outputs_hex: row.child_coinbase_outputs.map(hex::encode),
                pow_validates_child_target: row.pow_validates_child_target,
                recorded_at: row.recorded_at,
                child_identities,
                child_pool: row.child_pool,
                btc_parent_pool: row.btc_parent_pool,
            })
        })
        .collect()
}
