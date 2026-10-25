//! Tree nodes' `relationship`: whether a visible block's signals name more than one pool, read
//! from the stored `relationship_summary` the mixed-signal navigator steps through, so the
//! outline, the highlight and the stepper agree. A node without a `block` row has no summary
//! and is not mixed.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::Client;

use super::super::shared::{PoolObject, pool_from_columns, stored_hash_from_display};
use super::TreeNode;

/// A tree node's summary of its relationship signals, by the block page's rule.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TreeRelationship {
    pub mixed: bool,
    pub pools: Vec<PoolObject>,
}

/// Every node's stored summary, one row per pool in stored (reference-first) order.
const STORED_SUMMARY_SQL: &str = "\
SELECT s.btc_header_hash, s.mixed, p.id, p.slug, p.canonical_name \
  FROM relationship_summary s \
  LEFT JOIN LATERAL unnest(s.pool_ids) WITH ORDINALITY AS member(pool_id, ord) ON TRUE \
  LEFT JOIN pool p ON p.id = member.pool_id \
 WHERE s.btc_header_hash = ANY($1::bytea[]) \
 ORDER BY s.btc_header_hash, member.ord";

/// Give every node its stored relationship summary in one statement, whatever the node count.
pub(super) async fn attach_relationship(client: &Client, nodes: &mut [TreeNode]) -> Result<()> {
    if nodes.is_empty() {
        return Ok(());
    }
    let hashes = nodes
        .iter()
        .map(|node| stored_hash_from_display(&node.hash))
        .collect::<Result<Vec<_>>>()?;
    let mut summaries = HashMap::<Vec<u8>, TreeRelationship>::new();
    for row in client
        .query(STORED_SUMMARY_SQL, &[&hashes])
        .await
        .context("load tree relationship summaries")?
    {
        let summary = summaries.entry(row.get(0)).or_default();
        summary.mixed = row.get(1);
        if let Some(id) = row.get::<_, Option<i64>>(2) {
            summary
                .pools
                .push(pool_from_columns(Some(id), row.get(3), row.get(4)));
        }
    }
    for (node, hash) in nodes.iter_mut().zip(&hashes) {
        node.relationship = summaries.remove(hash).unwrap_or_default();
    }
    Ok(())
}
