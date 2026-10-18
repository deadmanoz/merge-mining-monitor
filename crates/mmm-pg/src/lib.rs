//! Postgres connection configuration and connector (`PGHOST`/`PGPORT`/`PGUSER`/`PGPASSWORD`/`PGDATABASE`).
//! Contains no SQL or domain types; shared by direct clients and the API pool.

use std::env;

use anyhow::{Context, Result};
use tokio_postgres::{Client, Config, NoTls};
use tracing::error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgConfig {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub database: String,
}

impl PgConfig {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            host: env::var("PGHOST").unwrap_or_else(|_| "localhost".to_owned()),
            port: env::var("PGPORT")
                .unwrap_or_else(|_| "55432".to_owned())
                .parse()
                .context("PGPORT must be a u16")?,
            user: env::var("PGUSER").unwrap_or_else(|_| "mmm".to_owned()),
            password: env::var("PGPASSWORD").ok(),
            database: env::var("PGDATABASE").unwrap_or_else(|_| "mmm".to_owned()),
        })
    }

    /// Build the `tokio_postgres::Config` for this connection.
    pub fn to_tokio_config(&self) -> Config {
        let mut pg = Config::new();
        pg.host(&self.host)
            .port(self.port)
            .user(&self.user)
            .dbname(&self.database);
        if let Some(password) = &self.password {
            pg.password(password);
        }
        pg
    }
}

pub async fn connect(config: &PgConfig) -> Result<Client> {
    let (client, connection) = config
        .to_tokio_config()
        .connect(NoTls)
        .await
        .context("connect to Postgres")?;
    tokio::spawn(async move {
        if let Err(err) = connection.await {
            error!(error = %err, "Postgres connection task failed");
        }
    });

    Ok(client)
}
