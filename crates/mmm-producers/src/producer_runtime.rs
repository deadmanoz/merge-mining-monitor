//! Shared producer runtime: common bootstrap, DB/Core connection, and
//! post-backfill repair for chain producers.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use tokio_postgres::{Client, Transaction};
use tracing::{info, warn};

use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::capture::MergeMiningEventPayload;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{
    CaptureOutcome, ReconcileReadModelConfig, capture_in_txn, is_reconcile_budget_exhausted,
    requested_rebuilds_current, run_reconcile_read_model,
};
use mmm_store::{self, CaptureWrite, get_source_id, upsert_pool_snapshot};

use crate::bitcoin_epoch_cache::refresh_bitcoin_core_header_cache;

#[derive(Debug)]
pub(crate) struct ProducerContext {
    pool_ids_by_slug: HashMap<String, i64>,
    source_id: i64,
    parent_classifier: ConfiguredParentClassifier,
    non_bitcoin_parents: AtomicUsize,
}

impl ProducerContext {
    /// Borrows `resolver` so BTC-coinbase-attributing contexts can retain it
    /// while RSK (which attributes by miner address) drops it.
    pub(crate) async fn bootstrap_with(
        client: &Client,
        source_code: &str,
        resolver: &PoolResolver,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Result<Self> {
        let pool_ids_by_slug = upsert_pool_snapshot(client, resolver.snapshot()).await?;
        let source_id = get_source_id(client, source_code).await?;
        Ok(Self {
            pool_ids_by_slug,
            source_id,
            parent_classifier,
            non_bitcoin_parents: AtomicUsize::new(0),
        })
    }

    #[cfg(any(test, feature = "db-integration"))]
    pub(crate) fn from_parts(
        pool_ids_by_slug: HashMap<String, i64>,
        source_id: i64,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Self {
        Self {
            pool_ids_by_slug,
            source_id,
            parent_classifier,
            non_bitcoin_parents: AtomicUsize::new(0),
        }
    }

    pub(crate) fn source_id(&self) -> i64 {
        self.source_id
    }

    pub(crate) fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        &self.parent_classifier
    }

    pub(crate) async fn capture<F>(
        &self,
        client: &mut Client,
        payload: &mut MergeMiningEventPayload,
        chain_label: &str,
        upsert: F,
    ) -> Result<CaptureOutcome>
    where
        F: AsyncFn(&Transaction<'_>, i64, &MergeMiningEventPayload) -> Result<CaptureWrite>,
    {
        let outcome = capture_in_txn(
            client,
            self.source_id,
            &self.parent_classifier,
            payload,
            chain_label,
            upsert,
        )
        .await?;
        let parent = || {
            BlockHash::from_slice(&payload.btc_parent_header_hash).map_or_else(
                |_| hex::encode(&payload.btc_parent_header_hash),
                |hash| hash.to_string(),
            )
        };
        match outcome {
            CaptureOutcome::Written(_) => {}
            CaptureOutcome::NotBitcoin { reason, .. } => {
                self.non_bitcoin_parents.fetch_add(1, Ordering::Relaxed);
                info!(
                    chain = chain_label,
                    child_height = payload.child_height,
                    parent = parent(),
                    reason,
                    "AuxPoW parent is not a Bitcoin header; no evidence recorded"
                );
            }
            CaptureOutcome::LineagePending(reason) => info!(
                chain = chain_label,
                child_height = payload.child_height,
                parent = parent(),
                reason,
                "AuxPoW parent lineage waits on the next Bitcoin epoch in the Core header cache; held"
            ),
        }
        Ok(outcome)
    }

    pub(crate) fn non_bitcoin_parents(&self) -> usize {
        self.non_bitcoin_parents.load(Ordering::Relaxed)
    }

    pub(crate) async fn refresh_core_header_cache(&self, client: &mut Client) -> Result<()> {
        refresh_bitcoin_core_header_cache(client, &self.parent_classifier).await?;
        Ok(())
    }

    pub(crate) fn pool_ids_by_slug(&self) -> &HashMap<String, i64> {
        &self.pool_ids_by_slug
    }

    pub(crate) fn pool_ids_by_slug_mut(&mut self) -> &mut HashMap<String, i64> {
        &mut self.pool_ids_by_slug
    }
}

pub(crate) struct ProducerRuntime {
    pub(crate) pg_client: Client,
    pub(crate) parent_classifier: ConfiguredParentClassifier,
}

impl ProducerRuntime {
    pub(crate) async fn from_env_live() -> Result<Self> {
        Ok(Self {
            pg_client: connect_from_env().await?,
            parent_classifier: ConfiguredParentClassifier::from_env_required_live()?,
        })
    }

    pub(crate) async fn from_env() -> Result<Self> {
        let (pg_client, parent_classifier) = connect_core_required_from_env().await?;
        Ok(Self {
            pg_client,
            parent_classifier,
        })
    }
}

/// Connect Postgres, require a fresh Bitcoin Core node, and synchronize the
/// sparse Core-header cache.
pub async fn connect_core_required_from_env() -> Result<(Client, ConfiguredParentClassifier)> {
    let mut pg_client = connect_from_env().await?;
    let parent_classifier = bootstrap_core(&mut pg_client).await?;
    Ok((pg_client, parent_classifier))
}

/// Require a fresh Bitcoin Core node and synchronize the sparse Core-header cache.
pub async fn bootstrap_core(pg_client: &mut Client) -> Result<ConfiguredParentClassifier> {
    let parent_classifier = ConfiguredParentClassifier::from_env_required()?;
    refresh_bitcoin_core_header_cache(pg_client, &parent_classifier).await?;
    Ok(parent_classifier)
}

/// `reconcile-read-model`: `bootstrap` contacts Bitcoin Core first, except for `--if-needed`
/// rebuilds that the recorded decoder version and relationship fingerprint already make
/// unnecessary, and runs that do not [`ReconcileReadModelConfig::needs_bitcoin_core`].
pub async fn run_reconcile_read_model_command(
    pg_client: &mut Client,
    config: ReconcileReadModelConfig,
    bootstrap: impl AsyncFnOnce(&mut Client) -> Result<ConfiguredParentClassifier>,
) -> Result<usize> {
    // Return on a skip instead of letting the library check again: the version could change
    // in between.
    if requested_rebuilds_current(pg_client, &config).await? {
        return Ok(0);
    }
    let parent_classifier = if config.needs_bitcoin_core() {
        bootstrap(pg_client).await?
    } else {
        ConfiguredParentClassifier::Disabled
    };
    if config.rebuild_relationship_signals {
        // The rebuild resolves snapshot slugs through `pool`, so a new snapshot's pools must
        // exist before it records the snapshot's fingerprint.
        upsert_pool_snapshot(pg_client, PoolResolver::from_default_snapshot()?.snapshot()).await?;
    }
    run_reconcile_read_model(pg_client, &parent_classifier, config).await
}

/// Connect Postgres from the `PG*` environment without contacting Bitcoin Core.
pub async fn connect_from_env() -> Result<Client> {
    let config = mmm_pg::PgConfig::from_env()?;
    mmm_pg::connect(&config).await
}

/// Post-backfill read-model repair over `[start_height, end_height]`. Budget
/// exhaustion logs a warning rather than failing the backfill.
pub(crate) async fn run_post_backfill_repair(
    client: &mut Client,
    classifier: &ConfiguredParentClassifier,
    source_code: Option<&str>,
    start_height: i32,
    end_height: i32,
    context_label: &str,
) -> Result<()> {
    match run_reconcile_read_model(
        client,
        classifier,
        ReconcileReadModelConfig {
            start_height: Some(start_height),
            end_height: Some(end_height),
            source_code: source_code.map(str::to_owned),
            ..ReconcileReadModelConfig::default()
        },
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(err) if is_reconcile_budget_exhausted(&err) => {
            warn!(
                error = %err,
                "read-model repair budget exhausted after {context_label}; rerun reconcile-read-model --missing-only to finish repair"
            );
            Ok(())
        }
        Err(err) => Err(err).context(format!("repair read model after {context_label}")),
    }
}

pub(crate) async fn warn_if_empty_known_stale_membership<C: tokio_postgres::GenericClient>(
    client: &C,
    context: &str,
) -> anyhow::Result<()> {
    if mmm_store::count_known_stale_blocks(client).await? == 0 {
        warn!(
            context,
            "known_stale_block is EMPTY; known stales cannot be excluded and may be \
             labelled strict/weak. Import the upstream stale-blocks dataset with \
             import-known-stales."
        );
    }
    Ok(())
}
