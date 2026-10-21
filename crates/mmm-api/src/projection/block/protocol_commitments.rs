//! The block page's `protocol_commitments`: the CORE and exSat declarations the read model
//! derived for the header, served only once its rebuild has recorded the running decoder's
//! version.

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::Client;

use mmm_capture::protocol_commitment::DECODER_VERSION;

use super::super::shared::{PoolObject, pool_from_columns};

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolCommitments {
    /// `decoded`, `undecodable` (stored outputs do not deserialize) or `unavailable` (none
    /// stored); entries are listed only when `decoded`, so an empty list there is an absence.
    pub outputs: &'static str,
    pub entries: Vec<ProtocolCommitmentEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolCommitmentEntry {
    pub protocol: String,
    pub vout: i32,
    pub decode_status: String,
    pub malformed_reason: Option<String>,
    pub version: Option<i16>,
    pub script_hex: String,
    pub evidence_source: String,
    /// CORE: whether Core DAO's relayer reads this output; `null` for exSat.
    pub upstream_selected: Option<bool>,
    pub core_validator: Option<String>,
    pub core_reward_address: Option<String>,
    pub core_binding_hash: Option<String>,
    pub exsat_account: Option<String>,
    /// The pool a registry associates with the declared account, never the block's miner.
    pub declared_account_pool: PoolObject,
}

/// `None` while the derived rows are not authoritative for the running decoder. The `stored`
/// probes name the same evidence as mmm-read-model's refresh (`EVIDENCE_SQL`): keep them in step.
pub(super) async fn load_protocol_commitments(
    client: &Client,
    hash: &[u8],
) -> Result<Option<ProtocolCommitments>> {
    let rows = client
        .query(
            "WITH header AS ( \
                 SELECT inv.protocol_commitments_decoder_version IS NOT DISTINCT FROM $2 AS ready, \
                        EXISTS (SELECT 1 FROM protocol_commitment_undecodable \
                                 WHERE btc_header_hash = $1) AS undecodable, \
                        EXISTS (SELECT 1 FROM block \
                                 WHERE btc_header_hash = $1 \
                                   AND btc_coinbase_outputs IS NOT NULL) \
                        OR EXISTS (SELECT 1 FROM merge_mining_event \
                                    WHERE btc_parent_header_hash = $1 AND revoked_at IS NULL \
                                      AND btc_parent_coinbase_outputs IS NOT NULL) \
                        OR EXISTS (SELECT 1 FROM child_block_observation \
                                    WHERE observation_kind = 'near' \
                                      AND btc_parent_header_hash = $1 \
                                      AND btc_parent_coinbase_outputs IS NOT NULL) AS stored \
                   FROM read_model_invariant inv WHERE inv.id = TRUE \
             ) \
             SELECT h.ready, h.undecodable, h.stored, \
                    c.vout, s.protocol, s.decode_status, s.malformed_reason, s.version, s.script, \
                    c.evidence_source, c.upstream_selected, s.core_validator, \
                    s.core_reward_address, s.core_binding_hash, s.exsat_account, \
                    p.id, p.slug, p.canonical_name \
               FROM header h \
               LEFT JOIN protocol_commitment c ON c.btc_header_hash = $1 \
               LEFT JOIN protocol_commitment_script s ON s.id = c.script_id \
               LEFT JOIN pool_identity i \
                      ON (i.namespace, i.identifier) = (s.account_namespace, s.account_identifier) \
               LEFT JOIN pool p ON p.id = i.pool_id \
              ORDER BY c.vout",
            &[&hash, &DECODER_VERSION],
        )
        .await
        .context("load protocol commitments")?;
    let first = rows
        .first()
        .context("read_model_invariant row missing; apply database migrations")?;
    if !first.get::<_, bool>(0) {
        return Ok(None);
    }
    let outputs = if first.get(1) {
        "undecodable"
    } else if first.get(2) {
        "decoded"
    } else {
        "unavailable"
    };
    let entries = if outputs == "decoded" {
        rows.iter()
            .filter_map(|row| {
                Some(ProtocolCommitmentEntry {
                    vout: row.get::<_, Option<i32>>(3)?,
                    protocol: row.get(4),
                    decode_status: row.get(5),
                    malformed_reason: row.get(6),
                    version: row.get(7),
                    script_hex: hex::encode(row.get::<_, Vec<u8>>(8)),
                    evidence_source: row.get(9),
                    upstream_selected: row.get(10),
                    core_validator: row.get::<_, Option<Vec<u8>>>(11).map(evm_address),
                    core_reward_address: row.get::<_, Option<Vec<u8>>>(12).map(evm_address),
                    core_binding_hash: row.get::<_, Option<Vec<u8>>>(13).map(hex::encode),
                    exsat_account: row.get(14),
                    declared_account_pool: pool_from_columns(row.get(15), row.get(16), row.get(17)),
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    Ok(Some(ProtocolCommitments { outputs, entries }))
}

fn evm_address(bytes: Vec<u8>) -> String {
    format!("0x{}", hex::encode(bytes))
}
