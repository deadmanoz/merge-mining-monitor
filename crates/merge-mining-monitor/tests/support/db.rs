//! Shared Postgres test harness: each test runs in an isolated throwaway
//! schema with all `migrations/` applied and Bitcoin's epoch-header history
//! seeded, then drops the schema on teardown regardless of test outcome.
//! Returns owned `(Client, String)` so `GenericClient` helpers need no reborrow.

use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use mmm_pg::{PgConfig, connect};
use mmm_store::{BitcoinCoreHeader, record_bitcoin_core_header};
use tokio_postgres::Client;

pub async fn new_test_db() -> Result<(Client, String)> {
    let client = connect(&PgConfig::from_env()?).await?;
    let schema = unique_schema();
    apply_migrations(&client, &schema).await?;
    seed_bitcoin_epoch_history(&client).await?;
    Ok((client, schema))
}

pub async fn teardown_test_db(client: &Client, schema: &str, result: Result<()>) -> Result<()> {
    drop_schema(client, schema).await?;
    result
}

#[macro_export]
macro_rules! run_db_test {
    ($client:ident, $body:block) => {{
        let ($client, schema) = $crate::support::db::new_test_db().await?;
        let test_result = async $body.await;
        $crate::support::db::teardown_test_db(&$client, &schema, test_result).await
    }};
    ($client:ident, $schema:ident, $body:block) => {{
        let ($client, $schema) = $crate::support::db::new_test_db().await?;
        let test_result = async $body.await;
        $crate::support::db::teardown_test_db(&$client, &$schema, test_result).await
    }};
}

#[macro_export]
macro_rules! run_mut_db_test {
    ($client:ident, $body:block) => {{
        let (mut $client, schema) = $crate::support::db::new_test_db().await?;
        let test_result = async $body.await;
        $crate::support::db::teardown_test_db(&$client, &schema, test_result).await
    }};
    ($client:ident, $schema:ident, $body:block) => {{
        let (mut $client, $schema) = $crate::support::db::new_test_db().await?;
        let test_result = async $body.await;
        $crate::support::db::teardown_test_db(&$client, &$schema, test_result).await
    }};
}

pub async fn connect_to_schema(schema: &str) -> Result<Client> {
    let client = connect(&PgConfig::from_env()?).await?;
    client
        .batch_execute(&format!("SET search_path TO {schema}, public;"))
        .await?;
    Ok(client)
}

pub async fn create_schema(client: &Client, schema: &str) -> Result<()> {
    client
        .batch_execute(&format!(
            "CREATE SCHEMA {schema}; \
             SET search_path TO {schema}, public; \
             CREATE TABLE schema_migrations ( \
               version TEXT PRIMARY KEY, \
               applied_at TIMESTAMPTZ NOT NULL DEFAULT now() \
             );"
        ))
        .await?;
    Ok(())
}

fn migration_paths() -> Result<Vec<std::path::PathBuf>> {
    migration_paths_from(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../migrations"))
}

fn migration_paths_from(dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>> {
    let mut migrations = fs::read_dir(dir)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    migrations.retain(|path| path.extension().is_some_and(|extension| extension == "sql"));
    migrations.sort();
    Ok(migrations)
}

pub async fn apply_migrations(client: &Client, schema: &str) -> Result<()> {
    create_schema(client, schema).await?;

    apply_migration_paths(client, schema, migration_paths()?).await?;

    client
        .batch_execute(&format!("SET search_path TO {schema}, public;"))
        .await?;
    Ok(())
}

/// Seed `fixtures/bitcoin/epoch-headers.json` into `bitcoin_core_header` so
/// tests capture against production difficulty history.
async fn seed_bitcoin_epoch_history(client: &Client) -> Result<()> {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/bitcoin/epoch-headers.json"
    )))
    .context("parse the Bitcoin epoch-history fixture")?;
    let mut heights = Vec::new();
    let mut times = Vec::new();
    let mut bits = Vec::new();
    for row in fixture["headers"]
        .as_array()
        .context("epoch-history fixture has no headers")?
    {
        heights.push(i32::try_from(
            row["height"].as_i64().context("header height")?,
        )?);
        times.push(row["time"].as_i64().context("header time")?);
        bits.push(i64::from(u32::from_str_radix(
            row["bits"].as_str().context("header bits")?,
            16,
        )?));
    }
    let hashes = heights
        .iter()
        .map(|&height| core_header_hash(height))
        .collect::<Vec<_>>();
    let finals = heights
        .iter()
        .map(|height| height % mmm_capture::nbits_table::DAA_EPOCH_INTERVAL == 0)
        .collect::<Vec<_>>();
    client
        .execute(
            "INSERT INTO bitcoin_core_header (height, block_hash, block_time, bits, is_final) \
             SELECT * FROM unnest($1::int4[], $2::bytea[], $3::int8[], $4::int8[], $5::bool[])",
            &[&heights, &hashes, &times, &bits, &finals],
        )
        .await
        .context("seed the Bitcoin epoch history")?;
    client
        .execute(
            "UPDATE bitcoin_core_header_cache_state SET horizon_time = $1 WHERE singleton",
            &[&times.iter().copied().max().unwrap_or_default()],
        )
        .await
        .context("seed the Bitcoin epoch-history horizon time")?;
    Ok(())
}

pub async fn clear_bitcoin_history(client: &Client) -> Result<()> {
    client
        .batch_execute(
            "DELETE FROM bitcoin_core_header; \
             UPDATE bitcoin_core_header_cache_state SET horizon_time = 0 WHERE singleton;",
        )
        .await
        .context("clear the seeded Bitcoin history")
}

/// Replace the seeded Bitcoin history with synthetic easy-target epochs for
/// tests whose fixture parents use synthetic headers.
pub async fn seed_synthetic_bitcoin_history(
    client: &Client,
    horizon_height: i32,
    horizon_time: i64,
    bits: u32,
) -> Result<()> {
    clear_bitcoin_history(client).await?;
    let epoch_height = mmm_capture::nbits_table::daa_epoch_start(horizon_height);
    for height in (0..=epoch_height).step_by(mmm_capture::nbits_table::DAA_EPOCH_INTERVAL as usize)
    {
        record_bitcoin_core_header(
            client,
            &BitcoinCoreHeader {
                height,
                block_hash: core_header_hash(height),
                block_time: i64::from(height) + 1,
                bits,
            },
        )
        .await?;
    }
    if horizon_height > epoch_height {
        record_bitcoin_core_header(
            client,
            &BitcoinCoreHeader {
                height: horizon_height,
                block_hash: core_header_hash(horizon_height),
                block_time: horizon_time,
                bits,
            },
        )
        .await?;
    }
    Ok(())
}

fn core_header_hash(height: i32) -> Vec<u8> {
    let mut hash = vec![0; 32];
    hash[28..].copy_from_slice(&height.to_be_bytes());
    hash
}

async fn apply_migration_paths(
    client: &Client,
    schema: &str,
    migrations: Vec<std::path::PathBuf>,
) -> Result<()> {
    for migration in migrations {
        let version = migration
            .file_stem()
            .and_then(|stem| stem.to_str())
            .context("migration filename is not valid UTF-8")?
            .to_owned();
        let sql = fs::read_to_string(&migration)
            .with_context(|| format!("read migration {}", migration.display()))?;
        client
            .batch_execute(&format!("SET search_path TO {schema}, public; {sql}"))
            .await
            .with_context(|| format!("apply migration {version}"))?;
        client
            .execute(
                "INSERT INTO schema_migrations(version) VALUES ($1)",
                &[&version],
            )
            .await?;
    }
    Ok(())
}

pub async fn drop_schema(client: &Client, schema: &str) -> Result<()> {
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;"))
        .await?;
    Ok(())
}

pub fn unique_schema() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("test_{}_{}_{}", std::process::id(), nanos, counter)
}

pub async fn displacement_at(
    client: &Client,
    source_id: i64,
    height: i32,
) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>, Option<i64>)>> {
    let rows = client
        .query(
            "SELECT child_block_hash, child_displaced_by, revoked_at \
             FROM merge_mining_event \
             WHERE source_id = $1 AND child_height = $2 \
             ORDER BY child_block_hash",
            &[&source_id, &height],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect())
}

pub async fn advisory_locks_held(client: &Client) -> Result<i64> {
    Ok(client
        .query_one(
            "SELECT count(*) FROM pg_locks \
             WHERE locktype = 'advisory' AND pid = pg_backend_pid()",
            &[],
        )
        .await?
        .get(0))
}
