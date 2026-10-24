//! The block page's `relationship_signals`, built from the rows each path already loaded.

use anyhow::{Context, Result};
use tokio_postgres::Client;

use super::super::shared::{
    CoinbaseEvidence, PoolObject, RelationshipSignals, SignalDraft, choose_coinbase_evidence,
    protocol_account_draft, relationship_signals,
};
use super::detail::ParentCoinbase;
use super::loaders::BlockDetailRow;
use super::{EventDetail, ProtocolCommitments};

/// The parent coinbase the Bitcoin signals read: the block row's complete Core coinbase, else
/// the rows' most complete copy.
pub(super) fn parent_coinbase<'a, R: ParentCoinbase>(
    block: Option<&'a BlockDetailRow>,
    rows: &'a [R],
) -> Option<CoinbaseEvidence<'a>> {
    if let Some(row) = block.filter(|row| row.btc_coinbase_complete) {
        return Some(CoinbaseEvidence {
            script: row.btc_coinbase_script.as_deref(),
            outputs: row.btc_coinbase_outputs.as_deref(),
            outputs_text: None,
        });
    }
    choose_coinbase_evidence(rows.iter().map(|row| {
        (
            row.order(),
            CoinbaseEvidence {
                script: row.parent_coinbase_script(),
                outputs: row.parent_coinbase_outputs(),
                outputs_text: row.parent_coinbase_outputs_text(),
            },
        )
    }))
}

/// Each rendered event's child attribution rows, in the page's event order.
pub(super) fn child_attribution_drafts(
    events: &[EventDetail],
) -> impl Iterator<Item = SignalDraft> + '_ {
    events.iter().flat_map(|event| {
        event.pool_attributions.child_block.iter().map(|row| {
            SignalDraft::child(&event.source, &row.namespace, &row.matched_value, &row.pool)
        })
    })
}

/// The page's signals in entry order: the parent coinbase's, each child's, then each account
/// the coinbase declares.
pub(super) async fn signals(
    client: &Client,
    bitcoin_miner_pool: &PoolObject,
    mut drafts: Vec<SignalDraft>,
    children: impl IntoIterator<Item = SignalDraft>,
    protocol_commitments: Option<&ProtocolCommitments>,
) -> Result<RelationshipSignals> {
    drafts.extend(children);
    drafts.extend(
        protocol_commitments
            .into_iter()
            .flat_map(|commitments| &commitments.entries)
            .filter_map(|entry| {
                protocol_account_draft(
                    &entry.protocol,
                    entry
                        .core_reward_address
                        .as_deref()
                        .or(entry.exsat_account.as_deref()),
                    &entry.declared_account_pool,
                )
            }),
    );
    relationship_signals(client, vec![(bitcoin_miner_pool, drafts)])
        .await?
        .pop()
        .context("one parent's signals")
}
