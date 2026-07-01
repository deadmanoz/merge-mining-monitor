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
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::{
    ReconcileReadModelConfig, is_reconcile_budget_exhausted, run_reconcile_read_model,
};
use mmm_store::{self, get_source_id, upsert_pool_snapshot};

/// The three fields EVERY producer holds. Chain contexts embed this as `base`
/// and delegate their `source_id()` / `parent_classifier()` accessors to it.
#[derive(Debug)]
pub(crate) struct ProducerContext {
    pool_ids_by_slug: HashMap<String, i64>,
    source_id: i64,
    parent_classifier: ConfiguredParentClassifier,
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
        Ok(Self {
            pool_ids_by_slug,
            source_id,
            parent_classifier,
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
        Self {
            pool_ids_by_slug,
            source_id,
            parent_classifier,
        }
    }

    /// `source.id` for this producer's chain, resolved once at bootstrap and
    /// stamped onto every `merge_mining_event` row it writes.
    pub(crate) fn source_id(&self) -> i64 {
        self.source_id
    }

    /// The injected Bitcoin Core parent classifier. `Disabled` on the
    /// historical-backfill path (no `BITCOIN_RPC_*`); a live classifier on the
    /// poll path.
    pub(crate) fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        &self.parent_classifier
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
/// any chain command runs: the DB connection (`PG*`) and the optional parent
/// classifier (`BITCOIN_RPC_*`). Those env families belong to this module and
/// to `parent_classifier`/`bitcoin_rpc` - never to `src/chains/`.
pub(crate) struct ProducerRuntime {
    pub(crate) pg_client: Client,
    pub(crate) parent_classifier: ConfiguredParentClassifier,
}

impl ProducerRuntime {
    /// Read both env families and connect: errors if `PG*` connect fails or if
    /// `BITCOIN_RPC_*` is set but malformed (an unset RPC yields a `Disabled`
    /// classifier, not an error, which is the historical-backfill default).
    pub(crate) async fn from_env() -> Result<Self> {
        Ok(Self {
            pg_client: connect_from_env().await?,
            parent_classifier: ConfiguredParentClassifier::from_env()?,
        })
    }
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

/// The classifier-enabled backfill warning, character-for-character identical
/// mod the chain name across its four users (Namecoin, Syscoin, Fractal, RSK).
/// A no-op when the classifier is disabled. The per-chain activation-floor
/// warnings are deliberately NOT consolidated here: each carries a
/// chain-specific skip-reason message, so they stay local in their runners.
pub(crate) fn warn_backfill_classifier_enabled(
    chain: &str,
    classifier: &ConfiguredParentClassifier,
) {
    if classifier.is_enabled() {
        warn!(
            "Bitcoin Core classifier is enabled during {} backfill; unset BITCOIN_RPC_URL for faster initial historical loads and run reclassify-unknown-parents afterward",
            chain
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_parts_round_trips_and_accessors_return_supplied_parts() {
        let mut pool_ids_by_slug = HashMap::new();
        pool_ids_by_slug.insert("f2pool".to_owned(), 7);
        pool_ids_by_slug.insert("antpool".to_owned(), 9);

        let ctx = ProducerContext::from_parts(
            pool_ids_by_slug.clone(),
            42,
            ConfiguredParentClassifier::Disabled,
        );

        assert_eq!(ctx.source_id(), 42);
        assert_eq!(ctx.pool_ids_by_slug(), &pool_ids_by_slug);
        assert!(!ctx.parent_classifier().is_enabled());
    }

    #[test]
    fn pool_ids_by_slug_mut_appends_in_place() {
        let mut ctx =
            ProducerContext::from_parts(HashMap::new(), 1, ConfiguredParentClassifier::Disabled);
        ctx.pool_ids_by_slug_mut().insert("btc-com".to_owned(), 3);
        assert_eq!(ctx.pool_ids_by_slug().get("btc-com"), Some(&3));
    }
}
