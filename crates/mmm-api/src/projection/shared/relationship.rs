//! The API side of relationship signals (`mmm_capture::relationship`): the API pool object as a
//! signal pool, and snapshot slugs resolved against the `pool` table once per request.

use std::collections::HashMap;

use anyhow::{Context, Result};
use mmm_capture::relationship::{self, SignalPool, finish};
use tokio_postgres::Client;

use super::{PoolObject, pool_from_columns, unknown_pool};

pub(crate) use mmm_capture::relationship::{
    CoinbaseEvidence, bitcoin_drafts, child_coinbase_tag_draft, choose_coinbase_evidence,
    default_pool_resolver, protocol_account_draft,
};

pub type RelationshipSignals = relationship::RelationshipSignals<PoolObject>;
pub(crate) type SignalDraft = relationship::SignalDraft<PoolObject>;

impl SignalPool for PoolObject {
    fn pool_id(&self) -> Option<i64> {
        self.id.filter(|_| self.known)
    }

    fn unknown() -> Self {
        unknown_pool()
    }
}

/// Each parent's signals, given its Bitcoin miner and drafts, in at most one statement: the
/// snapshot slugs no known miner or signal pool already names.
pub(crate) async fn relationship_signals(
    client: &Client,
    parents: Vec<(&PoolObject, Vec<SignalDraft>)>,
) -> Result<Vec<RelationshipSignals>> {
    let mut pools_by_slug = HashMap::new();
    let mut missing = Vec::new();
    for (miner, drafts) in &parents {
        for pool in std::iter::once(*miner)
            .chain(drafts.iter().filter_map(SignalDraft::known_pool))
            .filter(|pool| pool.known)
        {
            if let Some(slug) = &pool.slug {
                pools_by_slug.insert(slug.clone(), pool.clone());
            }
        }
        missing.extend(
            drafts
                .iter()
                .filter_map(SignalDraft::snapshot_slug)
                .map(str::to_owned),
        );
    }
    missing.retain(|slug| !pools_by_slug.contains_key(slug));
    missing.sort();
    missing.dedup();
    pools_by_slug.extend(load_pools_by_slug(client, &missing).await?);
    Ok(parents
        .into_iter()
        .map(|(miner, drafts)| finish(miner, drafts, &pools_by_slug))
        .collect())
}

/// The `pool` rows for snapshot slugs, in one statement (none when there are no slugs).
async fn load_pools_by_slug(
    client: &Client,
    slugs: &[String],
) -> Result<HashMap<String, PoolObject>> {
    if slugs.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = client
        .query(
            "SELECT id, slug, canonical_name FROM pool WHERE slug = ANY($1::text[])",
            &[&slugs],
        )
        .await
        .context("load pools by snapshot slug")?;
    Ok(rows
        .iter()
        .map(|row| {
            let slug: String = row.get(1);
            (
                slug.clone(),
                pool_from_columns(row.get(0), Some(slug), row.get(2)),
            )
        })
        .collect())
}
