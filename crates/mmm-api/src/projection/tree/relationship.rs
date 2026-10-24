//! Tree nodes' `relationship`: whether a visible block's signals name more than one pool, by the
//! block page's rule. It reads every active event whatever the Source filter, because a block's
//! relationships do not depend on what the view filters.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;
use tokio_postgres::Client;

use super::super::shared::{
    COINBASE_EVIDENCE_TIER_SQL, CoinbaseEvidence, PoolObject, RelationshipSignals, SignalDraft,
    bitcoin_drafts, pool_from_columns, protocol_account_draft, relationship_signals,
    stored_hash_from_display,
};
use super::TreeNode;

/// A tree node's summary of its relationship signals, by the block page's rule.
#[derive(Debug, Clone, Default, Serialize)]
pub struct TreeRelationship {
    pub mixed: bool,
    pub pools: Vec<PoolObject>,
}

/// Every node's child attribution rows, in the block page's event order (`event_detail_sql`),
/// then attribution order.
const CHILD_ATTRIBUTIONS_SQL: &str = "\
SELECT e.btc_parent_header_hash, s.code, a.namespace, a.matched_value, \
       p.id, p.slug, p.canonical_name \
  FROM merge_mining_event e \
  JOIN source s ON s.id = e.source_id \
  JOIN event_pool_attribution a ON a.event_id = e.id AND a.side = 'child_block' \
  LEFT JOIN pool_identity pi ON pi.id = a.pool_identity_id \
  LEFT JOIN pool p ON p.id = COALESCE(pi.pool_id, a.pool_id) \
 WHERE e.btc_parent_header_hash = ANY($1::bytea[]) AND e.revoked_at IS NULL \
 ORDER BY e.btc_parent_header_hash, e.confirmed_at, s.code, e.child_height NULLS LAST, \
          e.child_block_hash NULLS LAST, e.id, a.id";

/// Every node's parent coinbase copy: the block row's complete Core coinbase, else its events'
/// most complete copy.
fn parent_coinbase_sql() -> String {
    format!(
        "SELECT node.hash, COALESCE(b.btc_coinbase_script, ev.script), \
                COALESCE(b.btc_coinbase_outputs, ev.outputs), ev.outputs_text \
           FROM unnest($1::bytea[]) AS node(hash) \
           LEFT JOIN block b \
                  ON b.btc_header_hash = node.hash AND b.btc_coinbase_status = 'complete' \
           LEFT JOIN LATERAL ( \
               SELECT e.btc_parent_coinbase_script AS script, \
                      e.btc_parent_coinbase_outputs AS outputs, \
                      e.btc_parent_coinbase_outputs_text AS outputs_text \
                 FROM merge_mining_event e \
                WHERE b.btc_header_hash IS NULL AND e.btc_parent_header_hash = node.hash \
                  AND e.revoked_at IS NULL \
                  AND (e.btc_parent_coinbase_script IS NOT NULL \
                       OR e.btc_parent_coinbase_outputs IS NOT NULL \
                       OR e.btc_parent_coinbase_outputs_text IS NOT NULL) \
                ORDER BY {COINBASE_EVIDENCE_TIER_SQL}, e.id LIMIT 1) ev ON TRUE"
    )
}

/// Give every node its relationship summary: two statements, and a third only for snapshot
/// pools no node already names, whatever the node count.
pub(super) async fn attach_relationship(client: &Client, nodes: &mut [TreeNode]) -> Result<()> {
    if nodes.is_empty() {
        return Ok(());
    }
    let hashes = nodes
        .iter()
        .map(|node| stored_hash_from_display(&node.hash))
        .collect::<Result<Vec<_>>>()?;
    let coinbase_rows = client
        .query(&parent_coinbase_sql(), &[&hashes])
        .await
        .context("load tree parent coinbases")?;
    let coinbases = coinbase_rows
        .iter()
        .map(|row| {
            let copy = CoinbaseEvidence {
                script: row.get(1),
                outputs: row.get(2),
                outputs_text: row.get(3),
            };
            (row.get::<_, &[u8]>(0), copy)
        })
        .collect::<HashMap<_, _>>();
    let mut children = HashMap::<Vec<u8>, Vec<SignalDraft>>::new();
    for row in client
        .query(CHILD_ATTRIBUTIONS_SQL, &[&hashes])
        .await
        .context("load tree child attributions")?
    {
        children
            .entry(row.get(0))
            .or_default()
            .push(SignalDraft::child(
                row.get(1),
                row.get(2),
                row.get(3),
                &pool_from_columns(row.get(4), row.get(5), row.get(6)),
            ));
    }
    let parents =
        nodes
            .iter()
            .zip(&hashes)
            .map(|(node, hash)| {
                let mut drafts = bitcoin_drafts(coinbases.get(hash.as_slice()).copied());
                drafts.extend(children.remove(hash).unwrap_or_default());
                drafts.extend(node.protocol_commitments.iter().flatten().filter_map(
                    |commitment| {
                        protocol_account_draft(
                            &commitment.protocol,
                            commitment.declared_account.as_deref(),
                            &commitment.declared_account_pool,
                        )
                    },
                ));
                (&node.bitcoin_miner_pool, drafts)
            })
            .collect();
    let summaries = relationship_signals(client, parents).await?;
    for (node, RelationshipSignals { mixed, pools, .. }) in nodes.iter_mut().zip(summaries) {
        node.relationship = TreeRelationship { mixed, pools };
    }
    Ok(())
}
