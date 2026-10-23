//! Fenced operational status. Never use these writes inside evidence transactions.

use anyhow::{Result, ensure};
use tokio_postgres::Client;

#[derive(Debug, Clone, Copy)]
pub struct ProducerRun {
    pub source_id: i64,
    pub run_id: i64,
}

pub struct RuntimeTransition {
    pub phase: &'static str,
    pub state: &'static str,
    pub dependency: Option<&'static str>,
    pub reason_code: Option<&'static str>,
    pub failures: i64,
    pub next_retry_at: Option<i64>,
}

pub async fn start_producer_run(client: &Client, source_id: i64) -> Result<ProducerRun> {
    let row = client
        .query_one(
            "INSERT INTO producer_runtime_state (source_id, phase, state) \
         VALUES ($1, 'startup', 'recovering') ON CONFLICT (source_id) DO UPDATE SET \
         run_id = DEFAULT, phase = 'startup', state = 'recovering', \
         dependency = NULL, reason_code = NULL, failure_since = NULL, \
         consecutive_failures = 0, last_attempt_at = NULL, next_retry_at = NULL, \
         heartbeat_at = EXCLUDED.heartbeat_at RETURNING run_id",
            &[&source_id],
        )
        .await?;
    Ok(ProducerRun {
        source_id,
        run_id: row.get(0),
    })
}

pub async fn heartbeat_producer_run(client: &Client, run: ProducerRun) -> Result<()> {
    fenced(client.execute(
        "UPDATE producer_runtime_state SET heartbeat_at = extract(epoch FROM clock_timestamp())::bigint \
         WHERE source_id = $1 AND run_id = $2", &[&run.source_id, &run.run_id],
    ).await?)
}

pub async fn attempt_producer_run(client: &Client, run: ProducerRun) -> Result<()> {
    fenced(client.execute(
        "UPDATE producer_runtime_state SET last_attempt_at = extract(epoch FROM clock_timestamp())::bigint, \
         next_retry_at = NULL WHERE source_id = $1 AND run_id = $2", &[&run.source_id, &run.run_id],
    ).await?)
}

pub async fn transition_producer_run(
    client: &Client,
    run: ProducerRun,
    next: &RuntimeTransition,
) -> Result<()> {
    fenced(client.execute(
        "UPDATE producer_runtime_state SET phase = $3, state = $4, dependency = $5, reason_code = $6, \
         consecutive_failures = $7, next_retry_at = $8, \
         failure_since = CASE WHEN $4 = 'healthy' THEN NULL \
             WHEN $4 IN ('waiting', 'failed') THEN coalesce(failure_since, extract(epoch FROM clock_timestamp())::bigint) \
             ELSE failure_since END, \
         last_success_at = CASE WHEN $4 = 'healthy' THEN extract(epoch FROM clock_timestamp())::bigint ELSE last_success_at END, \
         heartbeat_at = extract(epoch FROM clock_timestamp())::bigint \
         WHERE source_id = $1 AND run_id = $2",
        &[&run.source_id, &run.run_id, &next.phase, &next.state, &next.dependency,
          &next.reason_code, &next.failures, &next.next_retry_at],
    ).await?)
}

fn fenced(rows: u64) -> Result<()> {
    ensure!(rows == 1, "producer run superseded by a newer owner");
    Ok(())
}
