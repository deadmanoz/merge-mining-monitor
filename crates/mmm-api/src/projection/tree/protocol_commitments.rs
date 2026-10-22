//! Tree nodes' `protocol_commitments`: each visible header's CORE and exSat declarations, for
//! the block badge and card. The block page says why a header has none; the tree only lists them.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::Client;

use mmm_capture::protocol_commitment::DECODER_VERSION;

use super::super::shared::{
    PROTOCOL_COMMITMENT_JOIN_SQL, PROTOCOL_COMMITMENTS_READY_SQL, PoolObject, pool_from_columns,
    stored_hash_from_display,
};
use super::TreeNode;

#[derive(Debug, Clone, Serialize)]
pub struct TreeProtocolCommitment {
    pub protocol: String,
    pub vout: i32,
    pub decode_status: String,
    /// The CORE reward address or exSat account; `null` for a declaration that is not valid.
    pub declared_account: Option<String>,
    /// The pool a registry associates with the declared account, never the block's miner.
    pub declared_account_pool: PoolObject,
}

/// Leaves every node `None` until the rebuild has recorded the running decoder's version, then
/// gives every node its declarations (empty when none were derived) from one query.
pub(super) async fn attach_protocol_commitments(
    client: &Client,
    nodes: &mut [TreeNode],
) -> Result<()> {
    let hashes = nodes
        .iter()
        .map(|node| stored_hash_from_display(&node.hash))
        .collect::<Result<Vec<_>>>()?;
    let rows = client
        .query(
            &format!(
                "WITH gate AS ( \
                 SELECT {PROTOCOL_COMMITMENTS_READY_SQL} AS ready \
                   FROM read_model_invariant WHERE id = TRUE \
             ) \
             SELECT g.ready, c.btc_header_hash, c.vout, s.protocol, s.decode_status, \
                    s.account_identifier, p.id, p.slug, p.canonical_name \
               FROM gate g \
               LEFT JOIN protocol_commitment c ON g.ready AND c.btc_header_hash = ANY($1::bytea[]) \
               {PROTOCOL_COMMITMENT_JOIN_SQL} \
              ORDER BY c.vout"
            ),
            &[&hashes, &DECODER_VERSION],
        )
        .await
        .context("load tree protocol commitments")?;
    let first = rows
        .first()
        .context("read_model_invariant row missing; apply database migrations")?;
    if !first.get::<_, bool>(0) {
        return Ok(());
    }
    let mut by_hash = HashMap::<Vec<u8>, Vec<TreeProtocolCommitment>>::new();
    for row in &rows {
        let Some(hash) = row.get::<_, Option<Vec<u8>>>(1) else {
            continue;
        };
        by_hash
            .entry(hash)
            .or_default()
            .push(TreeProtocolCommitment {
                vout: row.get(2),
                protocol: row.get(3),
                decode_status: row.get(4),
                declared_account: row.get(5),
                declared_account_pool: pool_from_columns(row.get(6), row.get(7), row.get(8)),
            });
    }
    for (node, hash) in nodes.iter_mut().zip(&hashes) {
        node.protocol_commitments = Some(by_hash.remove(hash).unwrap_or_default());
    }
    Ok(())
}
