use anyhow::{Context, Result};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use std::str::FromStr;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!(
        version = merge_mining_monitor::VERSION,
        "merge-mining-monitor starting"
    );

    let mut args = std::env::args();
    let _program = args.next();
    let command = args.next();
    match command.as_deref() {
        Some(producer) if mmm_producers::is_producer_command(producer) => {
            mmm_producers::run_producer_command(producer, args).await?;
        }
        Some("backfill-hathor-cache") => {
            mmm_producers::run_hathor_cache_command(args).await?;
        }
        Some("import-dataset") => cmd_import_dataset(args).await?,
        Some("import-all") => cmd_import_all(args).await?,
        Some("import-known-stales") => cmd_import_known_stales(args).await?,
        Some("reclassify-unknown-parents") => cmd_reclassify_unknown_parents(args).await?,
        Some("reclassify-parent") => cmd_reclassify_parent(args).await?,
        Some("reclassify-known-stales") => cmd_reclassify_known_stales(args).await?,
        Some("reclassify-pools") => cmd_reclassify_pools(args).await?,
        Some("sync-bitcoin-core") => cmd_sync_bitcoin_core(args).await?,
        Some("reconcile-read-model") => cmd_reconcile_read_model(args).await?,
        Some("revoke-merge-mining-event") => cmd_revoke_merge_mining_event(args).await?,
        Some("restore-merge-mining-event") => cmd_restore_merge_mining_event(args).await?,
        Some("serve") => {
            let config = mmm_api::ServeConfig::from_env()?;
            mmm_api::serve(config).await?;
        }
        Some(other) => {
            anyhow::bail!(mmm_producers::unknown_command_message(other));
        }
        None => {
            info!("{}", mmm_producers::no_command_help());
        }
    }

    Ok(())
}

async fn cmd_import_all(args: std::env::Args) -> Result<()> {
    let config = mmm_producers::HistoricalImportAllConfig::from_args(args)?;
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    let summary =
        mmm_producers::run_historical_import_all(&mut pg_client, &classifier, &config).await?;
    summary.print();
    Ok(())
}

async fn cmd_import_dataset(args: std::env::Args) -> Result<()> {
    let config = mmm_producers::HistoricalImportConfig::from_args(args)?;
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    let summary =
        mmm_producers::run_historical_import(&mut pg_client, &classifier, &config).await?;
    summary.print();

    Ok(())
}

async fn cmd_import_known_stales(args: std::env::Args) -> Result<()> {
    let config = mmm_producers::KnownStaleImportConfig::from_args(args)?;
    let (mut pg_client, _) = mmm_producers::connect_core_required_from_env().await?;
    let summary = mmm_producers::run_import_known_stales(&mut pg_client, &config).await?;
    summary.print();

    Ok(())
}

async fn cmd_reclassify_known_stales(args: std::env::Args) -> Result<()> {
    let config = mmm_read_model::ReclassifyKnownStalesConfig::from_args(args)?;
    let (mut pg_client, _) = mmm_producers::connect_core_required_from_env().await?;
    let summary = mmm_read_model::run_reclassify_known_stales(&mut pg_client, config).await?;
    summary.print();

    Ok(())
}

async fn cmd_reclassify_unknown_parents(args: std::env::Args) -> Result<()> {
    let config = mmm_read_model::ReclassifyUnknownParentsConfig::from_args(args)?;
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    let count =
        mmm_read_model::run_reclassify_unknown_parents(&mut pg_client, &classifier, config).await?;
    info!(count, "reclassified unknown Bitcoin parent headers");

    Ok(())
}

async fn cmd_reclassify_parent(mut args: std::env::Args) -> Result<()> {
    let hash = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: reclassify-parent <bitcoin-header-hash>"))?;
    if args.next().is_some() {
        anyhow::bail!("usage: reclassify-parent <bitcoin-header-hash>");
    }
    let hash =
        BlockHash::from_str(&hash).context("bitcoin-header-hash must be a valid block hash")?;
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    let stats =
        mmm_read_model::run_reclassify_parent(&mut pg_client, &classifier, &hash.to_byte_array())
            .await?;
    info!(
        hash = %hash,
        parents_reconciled = stats.parents_reconciled,
        descendants_reconciled = stats.descendants_reconciled,
        "reclassified Bitcoin parent"
    );

    Ok(())
}

async fn cmd_reclassify_pools(args: std::env::Args) -> Result<()> {
    let config = mmm_producers::ReclassifyPoolsConfig::from_args(args)?;
    let (mut pg_client, _) = mmm_producers::connect_core_required_from_env().await?;
    let stats = mmm_producers::run_reclassify_pools(&mut pg_client, config).await?;
    info!(
        parent_pool_updates = stats.parent_pool_updates,
        child_pool_updates = stats.child_pool_updates,
        parents_reconciled = stats.parents_reconciled,
        rsk_miner_attribution_updates = stats.rsk_miner_attribution_updates,
        rsk_miner_sidecar_late_fills = stats.rsk_miner_sidecar_late_fills,
        "re-resolved historical pool attributions"
    );

    Ok(())
}

async fn cmd_sync_bitcoin_core(args: std::env::Args) -> Result<()> {
    let config = mmm_producers::BitcoinCoreSyncConfig::from_args(args)?;
    let rpc = bitcoin_core_rpc_for_command("sync-bitcoin-core")?;
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    if config.follow {
        // Long-lived managed-service daemon: catch up to tip then follow
        // it, with its own SIGINT/SIGTERM handling. Returns only on a
        // clean shutdown or a fatal backbone integrity error.
        mmm_producers::run_sync_bitcoin_core_follow(&mut pg_client, &rpc, &classifier, config)
            .await?;
    } else {
        let stats = mmm_producers::run_sync_bitcoin_core(&mut pg_client, &rpc, config).await?;
        info!(
            attempted = stats.attempted,
            completed = stats.completed,
            skipped_complete = stats.skipped_complete,
            coinbase_failed = stats.coinbase_failed,
            "synced Bitcoin Core backbone"
        );
    }

    Ok(())
}

async fn cmd_reconcile_read_model(args: std::env::Args) -> Result<()> {
    let config = mmm_read_model::ReconcileReadModelConfig::from_args(args)?;
    // Core is deliberately mandatory even for source-health-only maintenance:
    // bootstrap keeps the persisted cache current before read-model maintenance.
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    let count =
        mmm_read_model::run_reconcile_read_model(&mut pg_client, &classifier, config).await?;
    info!(count, "reconciled read-model work items");

    Ok(())
}

async fn cmd_revoke_merge_mining_event(mut args: std::env::Args) -> Result<()> {
    let event_id = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: revoke-merge-mining-event <event-id> <reason>"))?
        .parse()
        .context("event-id must be a valid i64")?;
    if event_id <= 0 {
        anyhow::bail!("event-id must be positive");
    }
    let reason = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: revoke-merge-mining-event <event-id> <reason>"))?;
    if reason.trim().is_empty() {
        anyhow::bail!("reason must be non-empty");
    }
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    mmm_read_model::revoke_merge_mining_event(&mut pg_client, event_id, &reason, &classifier)
        .await?;
    info!(event_id, "revoked merge_mining_event");

    Ok(())
}

async fn cmd_restore_merge_mining_event(mut args: std::env::Args) -> Result<()> {
    let event_id = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: restore-merge-mining-event <event-id>"))?
        .parse()
        .context("event-id must be a valid i64")?;
    if event_id <= 0 {
        anyhow::bail!("event-id must be positive");
    }
    let (mut pg_client, classifier) = mmm_producers::connect_core_required_from_env().await?;
    mmm_read_model::restore_merge_mining_event(&mut pg_client, event_id, &classifier).await?;
    info!(event_id, "restored merge_mining_event");

    Ok(())
}

fn bitcoin_core_rpc_for_command(command: &str) -> Result<mmm_bitcoin_core::BitcoinCoreRpcClient> {
    let rpc_url = std::env::var("BITCOIN_RPC_URL").context("read BITCOIN_RPC_URL")?;
    if rpc_url.trim().is_empty() {
        anyhow::bail!("BITCOIN_RPC_URL must be non-empty for {command}");
    }
    mmm_bitcoin_core::BitcoinCoreRpcClient::from_env_url(&rpc_url)
}
