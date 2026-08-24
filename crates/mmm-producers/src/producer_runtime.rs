//! Shared producer runtime: the common bootstrap, DB-connect, post-backfill
//! repair, and warning wiring that every chain producer (Namecoin, RSK,
//! Syscoin, Fractal, Hathor, Elastos) previously duplicated across
//! `*_capture.rs`, `main.rs`, and `backfill.rs`.
//!
//! The chain capture contexts COMPOSE [`ProducerContext`] (embed it as `base`
//! and delegate the common setup) rather than each re-deriving the pool
//! snapshot, source id, and classifier. The [`PoolResolver`] is intentionally
//! NOT a field here: only the five BTC-coinbase-attributing producers (the
//! Namecoin family) keep a resolver and use it per height
//! (`capture::resolve_event_pools`), while RSK attributes by miner-address
//! identity and holds none. So [`ProducerContext::bootstrap_with`] BORROWS a
//! resolver and the caller decides whether to keep it.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio_postgres::Client;
use tracing::warn;

use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::nbits_table::NbitsTable;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{
    ReconcileReadModelConfig, is_reconcile_budget_exhausted, run_reconcile_read_model,
};
use mmm_store::{self, get_source_id, upsert_pool_snapshot};

use crate::bitcoin_epoch_cache::refresh_bitcoin_core_header_cache;

/// The shared fields EVERY producer holds. Chain contexts embed this as `base`
/// and delegate their `source_id()` / `parent_classifier()` accessors to it.
#[derive(Debug)]
pub(crate) struct ProducerContext {
    pool_ids_by_slug: HashMap<String, i64>,
    source_id: i64,
    parent_classifier: ConfiguredParentClassifier,
    nbits_table: NbitsTable,
}

impl ProducerContext {
    /// DB-touching bootstrap with the resolver + classifier INJECTED. Borrows
    /// the resolver (does not store or consume it) so the caller keeps
    /// ownership: the five Namecoin-family contexts retain it in their own
    /// field, RSK lets it drop. This is the shared body of every context's
    /// `new_with_classifier` (the `from_default_snapshot` +
    /// `upsert_pool_snapshot` + `get_source_id` sequence), generalized over
    /// `source_code`.
    pub(crate) async fn bootstrap_with(
        client: &Client,
        source_code: &str,
        resolver: &PoolResolver,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Result<Self> {
        let pool_ids_by_slug = upsert_pool_snapshot(client, resolver.snapshot()).await?;
        let source_id = get_source_id(client, source_code).await?;
        let nbits_table = mmm_store::load_bitcoin_core_nbits_table(client).await?;
        Ok(Self {
            pool_ids_by_slug,
            source_id,
            parent_classifier,
            nbits_table,
        })
    }

    /// Build a context from already-resolved parts. Mirrors the per-chain
    /// test-only `from_parts` constructors; takes no resolver (the resolver is not stored
    /// here), so a resolver-free caller (RSK) can build the embedded base from
    /// arguments it already has.
    #[cfg(any(test, feature = "db-integration"))]
    pub(crate) fn from_parts(
        pool_ids_by_slug: HashMap<String, i64>,
        source_id: i64,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Self {
        let nbits_table = NbitsTable::from_bitcoin_core_headers(&[
            mmm_capture::nbits_table::BitcoinEpochHeader {
                height: 0,
                block_time: 1,
                bits: 0x1d00_ffff,
            },
        ])
        .expect("the minimal test Core header cache is valid");
        Self {
            pool_ids_by_slug,
            source_id,
            parent_classifier,
            nbits_table,
        }
    }

    /// `source.id` for this producer's chain, resolved once at bootstrap and
    /// stamped onto every `merge_mining_event` row it writes.
    pub(crate) fn source_id(&self) -> i64 {
        self.source_id
    }

    /// The injected Bitcoin Core parent classifier. Production runtimes always
    /// provide the Core-backed variant; `Disabled` is only for library tests.
    pub(crate) fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        &self.parent_classifier
    }

    pub(crate) fn nbits_table(&self) -> &NbitsTable {
        &self.nbits_table
    }

    /// Refresh the in-memory view from the required Core node.
    pub(crate) async fn refresh_nbits_table(&mut self, client: &mut Client) -> Result<()> {
        self.nbits_table =
            refresh_bitcoin_core_header_cache(client, &self.parent_classifier).await?;
        Ok(())
    }

    /// Snapshot of `pool.slug -> pool.id` taken at bootstrap, the map capture
    /// uses to attribute an event to a pool id without a per-height DB round
    /// trip. Read-only; RSK extends its copy via `pool_ids_by_slug_mut`.
    pub(crate) fn pool_ids_by_slug(&self) -> &HashMap<String, i64> {
        &self.pool_ids_by_slug
    }

    /// Mutable access for the RSK extension, which appends RSK-only slugs to the
    /// pool map after bootstrap. The read accessor stays `&`, so the field is
    /// otherwise private.
    pub(crate) fn pool_ids_by_slug_mut(&mut self) -> &mut HashMap<String, i64> {
        &mut self.pool_ids_by_slug
    }
}

/// The non-chain runtime a producer command needs, built in `main.rs` BEFORE
/// any chain command runs: the DB connection (`PG*`) and required Bitcoin Core
/// classifier (`BITCOIN_RPC_*`). Those env families belong to this module and
/// to `parent_classifier`/`bitcoin_rpc` - never to `src/chains/`.
pub(crate) struct ProducerRuntime {
    pub(crate) pg_client: Client,
    pub(crate) parent_classifier: ConfiguredParentClassifier,
}

impl ProducerRuntime {
    /// Read both env families, require a fresh Core tip, and refresh the header
    /// cache before a chain command can classify a parent.
    pub(crate) async fn from_env() -> Result<Self> {
        let (pg_client, parent_classifier) = connect_core_required_from_env().await?;
        Ok(Self {
            pg_client,
            parent_classifier,
        })
    }
}

/// Connect Postgres and require a fresh Bitcoin Core node, then synchronize the
/// sparse Core-header cache used by nBits classification in this command.
pub async fn connect_core_required_from_env() -> Result<(Client, ConfiguredParentClassifier)> {
    let mut pg_client = connect_from_env().await?;
    let parent_classifier = ConfiguredParentClassifier::from_env_required()?;
    refresh_bitcoin_core_header_cache(&mut pg_client, &parent_classifier).await?;
    Ok((pg_client, parent_classifier))
}

/// `PgConfig::from_env` + `connect`, the two-line DB setup the 12 poll/backfill
/// subcommand arms in `main.rs` previously repeated.
pub async fn connect_from_env() -> Result<Client> {
    let config = mmm_pg::PgConfig::from_env()?;
    mmm_pg::connect(&config).await
}

/// Post-backfill read-model repair, with the shared budget-exhausted policy
/// folded in. Wraps [`run_reconcile_read_model`] over the bounded height window
/// and an optional source scope: Namecoin passes `None` (its post-backfill repair
/// is global today), the other chains pass their own source code. A
/// reconcile-budget-exhausted error is downgraded to a warning and swallowed (the
/// operator reruns `reconcile-read-model --missing-only`); any other reconcile
/// error is fatal and gains a `repair read model after {context_label}` context.
/// `context_label` is the full operation phrase the call sites previously baked
/// into their own match arms (e.g. `"RSK backfill"`, `"Hathor cache ingest"`,
/// `"Namecoin backfill"`); it keeps the human-readable attribution after folding
/// the (formerly per-runner) match here, which also moves the warning's emitting
/// callsite metadata (its tracing `module_path` / `file:line`) into this helper.
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

/// Degraded-state guard shared by every producer entry path (live pollers,
/// bounded backfills, and the Hathor cache import): warn loudly when the
/// known-stale membership is empty, since capture/classification can then
/// label a catalogued stale strict/weak. Mirrors the reclassify/reconcile
/// warnings; import the upstream dataset with import-known-stales.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_ids_by_slug_mut_appends_in_place() {
        let mut ctx =
            ProducerContext::from_parts(HashMap::new(), 1, ConfiguredParentClassifier::Disabled);
        ctx.pool_ids_by_slug_mut().insert("btc-com".to_owned(), 3);
        assert_eq!(ctx.pool_ids_by_slug().get("btc-com"), Some(&3));
    }
}
