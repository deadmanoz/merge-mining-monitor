//! Shared Postgres test harness for the DB integration binaries.
//!
//! One copy of the schema/migration lifecycle that `tests/db_integration.rs`
//! and `tests/api_db_integration.rs` previously duplicated word-for-word:
//! every test runs in its own throwaway schema, applies the full `migrations/`
//! directory in-process, and drops the schema on the way out (pass or fail).
//!
//! The standard prelude/teardown pair is `new_test_db` + `teardown_test_db`:
//!
//! ```ignore
//! let (mut client, schema) = new_test_db().await?;
//!
//! let test_result = async { /* test body */ }.await;
//!
//! teardown_test_db(&client, &schema, test_result).await
//! ```
//!
//! The client and schema are handed over OWNED, not behind references,
//! because the `mmm-store` row helpers are generic over
//! `tokio_postgres::GenericClient` and generic call sites do not deref-coerce
//! a `&&mut Client` the way concrete `&Client` parameters do - owned locals
//! keep every existing test body compiling unchanged.
//!
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use mmm_pg::{PgConfig, connect};
use mmm_store::{BitcoinCoreHeader, record_bitcoin_core_header};
use tokio_postgres::Client;

/// Standard test prelude: connect from env (`PgConfig::from_env`), create a
/// unique schema, apply every migration, and hand both back owned.
pub async fn new_test_db() -> Result<(Client, String)> {
    let client = connect(&PgConfig::from_env()?).await?;
    let schema = unique_schema();
    apply_migrations(&client, &schema).await?;
    seed_bitcoin_epoch_history(&client).await?;
    Ok((client, schema))
}

/// Standard teardown: drop the schema, then propagate the body result.
///
/// Matches the original inline sequence exactly - the schema is dropped even
/// when the body failed, and a drop error surfaces (taking precedence, as
/// `drop_schema(...).await?;` did before).
pub async fn teardown_test_db(client: &Client, schema: &str, result: Result<()>) -> Result<()> {
    drop_schema(client, schema).await?;
    result
}

/// Run a DB-backed test body in a freshly migrated throwaway schema, always
/// dropping the schema before propagating the body result.
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

/// Mutable variant for tests that pass the client into APIs requiring
/// `&mut Client`.
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

/// Open an additional connection with the search path already pointing at an
/// existing test schema (spawned tasks, fake chain pollers).
pub async fn connect_to_schema(schema: &str) -> Result<Client> {
    let client = connect(&PgConfig::from_env()?).await?;
    client
        .batch_execute(&format!("SET search_path TO {schema}, public;"))
        .await?;
    Ok(client)
}

/// Create `schema`, point the session's search path at it, and create the
/// in-schema `schema_migrations` bookkeeping table.
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

/// Sorted `.sql` migration paths from the repo's `migrations/` directory.
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

/// Create `schema` and apply every migration into it, recording versions in
/// the in-schema `schema_migrations` table (the standard full prelude).
pub async fn apply_migrations(client: &Client, schema: &str) -> Result<()> {
    create_schema(client, schema).await?;

    apply_migration_paths(client, schema, migration_paths()?).await?;

    client
        .batch_execute(&format!("SET search_path TO {schema}, public;"))
        .await?;
    Ok(())
}

/// Seed Bitcoin's real retarget history (`fixtures/bitcoin/epoch-headers.json`:
/// every epoch boundary plus the tip at 967,961) as the Core header cache, so
/// every test captures against the difficulty history production decides
/// lineage by. A real Bitcoin fixture parent passes the lineage gate; a
/// synthetic one has to look like Bitcoin (a placed prev, or its epoch's
/// bits), or the capture refuses it. The rows carry synthetic hashes: the
/// fixture records heights, times and bits only.
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

/// Empty the Core header cache the harness seeds, for tests of the cache
/// machinery itself and tests whose fake Core chain the cache refresh
/// follows.
pub async fn clear_bitcoin_history(client: &Client) -> Result<()> {
    client
        .batch_execute(
            "DELETE FROM bitcoin_core_header; \
             UPDATE bitcoin_core_header_cache_state SET horizon_time = 0 WHERE singleton;",
        )
        .await
        .context("clear the seeded Bitcoin history")
}

/// Replace the seeded Bitcoin history with a synthetic one, for tests whose
/// fixture parents are synthetic: a header that meets its own target can only
/// be built with easy bits, so in this history every epoch from genesis to a
/// horizon at `horizon_height` and `horizon_time` carries `bits`. Genesis has
/// the all-zero hash the synthetic fixtures build on, so a fixture parent on
/// it has a placed prev.
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

/// Drop the test schema and everything in it.
pub async fn drop_schema(client: &Client, schema: &str) -> Result<()> {
    client
        .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE;"))
        .await?;
    Ok(())
}

/// Collision-proof schema name: PID + nanoseconds + per-process counter.
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

/// `(child_block_hash, child_displaced_by, revoked_at)` for every event at
/// the height, ordered by hash: the child-side state a displacement test
/// checks after each observation.
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

/// Advisory locks this session still holds: zero after a processed height.
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
