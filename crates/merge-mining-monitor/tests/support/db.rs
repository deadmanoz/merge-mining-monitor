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
use tokio_postgres::Client;

/// Standard test prelude: connect from env (`PgConfig::from_env`), create a
/// unique schema, apply every migration, and hand both back owned.
pub async fn new_test_db() -> Result<(Client, String)> {
    let client = connect(&PgConfig::from_env()?).await?;
    let schema = unique_schema();
    apply_migrations(&client, &schema).await?;
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
