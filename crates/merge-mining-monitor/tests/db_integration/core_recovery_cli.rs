use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::{Result, ensure};
use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use tokio_postgres::Client;

use crate::support::live_rpc::RpcFixture;

struct Producer(Child);
impl Drop for Producer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(config: &mmm_pg::PgConfig, rpc: &RpcFixture, args: &[&str]) -> Result<Producer> {
    let log = std::fs::File::create(
        std::env::temp_dir().join(format!("{}-{}.log", config.database, args[0])),
    )?;
    Ok(Producer(
        Command::new(env!("CARGO_BIN_EXE_merge-mining-monitor"))
            .args(args)
            .env("PGHOST", &config.host)
            .env("PGPORT", config.port.to_string())
            .env("PGUSER", &config.user)
            .env("PGPASSWORD", config.password.as_deref().unwrap_or(""))
            .env("PGDATABASE", &config.database)
            .env("BITCOIN_RPC_URL", &rpc.url)
            .env("BITCOIN_RPC_USER", "fixture")
            .env("BITCOIN_RPC_PASSWORD", "fixture")
            .env("NAMECOIN_RPC_URL", format!("{}/child", rpc.url))
            .env("NAMECOIN_RPC_USER", "fixture")
            .env("NAMECOIN_RPC_PASSWORD", "fixture")
            .env("NAMECOIN_POLL_INTERVAL_SECONDS", "1")
            .env("HATHOR_RPC_URL", format!("{}/hathor", rpc.url))
            .env("HATHOR_RPC_FALLBACK_URL", "")
            .env("BITCOIN_CORE_SYNC_FOLLOW_INTERVAL_SECS", "1")
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log))
            .spawn()?,
    ))
}

async fn until(mut check: impl AsyncFnMut() -> Result<bool>, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if check().await? {
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "live CLI condition timed out");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn stop(producer: &mut Producer) -> Result<Duration> {
    let started = Instant::now();
    ensure!(
        Command::new("kill")
            .args(["-TERM", &producer.0.id().to_string()])
            .status()?
            .success(),
        "send SIGTERM"
    );
    until(
        async || Ok(producer.0.try_wait()?.is_some()),
        Duration::from_secs(25),
    )
    .await?;
    ensure!(
        producer.0.wait()?.success(),
        "producer exited with a failure after SIGTERM"
    );
    Ok(started.elapsed())
}

async fn with_database(test: impl AsyncFnOnce(&mmm_pg::PgConfig) -> Result<()>) -> Result<()> {
    let mut config = mmm_pg::PgConfig::from_env()?;
    let admin = mmm_pg::connect(&config).await?;
    config.database = format!(
        "mmm_fbsk_{}_{}",
        std::process::id(),
        mmm_capture::capture::now_epoch_seconds()?
    );
    admin
        .batch_execute(&format!("CREATE DATABASE {}", config.database))
        .await?;
    let result = AssertUnwindSafe(async {
        let output = Command::new("./scripts/migrate-safe.sh")
            .arg("dev")
            .current_dir(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .env("PGHOST", &config.host)
            .env("PGPORT", config.port.to_string())
            .env("PGUSER", &config.user)
            .env("PGPASSWORD", config.password.as_deref().unwrap_or(""))
            .env("PGDATABASE", &config.database)
            .output()?;
        ensure!(
            output.status.success(),
            "backup-protected fixture migration: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        test(&config).await
    })
    .catch_unwind()
    .await;
    admin
        .batch_execute(&format!("DROP DATABASE {} WITH (FORCE)", config.database))
        .await?;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[tokio::test]
async fn cli_producers_survive_six_outages_recover_and_renew_during_long_work() -> Result<()> {
    with_database(exercise).await
}

#[tokio::test]
async fn cli_core_failed_cycle_keeps_outage_state_until_capture_succeeds() -> Result<()> {
    with_database(async |config| {
        let client = mmm_pg::connect(config).await?;
        let rpc = RpcFixture::start().await?;
        let mut core = start(config, &rpc, &["sync-bitcoin-core", "--follow"])?;
        until(async || Ok(client.query_one("SELECT count(*) FROM producer_runtime_state WHERE state='healthy'", &[]).await?.get::<_,i64>(0) == 1), Duration::from_secs(15)).await?;

        rpc.state.mode.store(1, Ordering::Relaxed);
        until(async || Ok(client.query_one("SELECT state='waiting' FROM producer_runtime_state", &[]).await?.get(0)), Duration::from_secs(10)).await?;
        let before = client.query_one("SELECT last_success_at, consecutive_failures FROM producer_runtime_state", &[]).await?;
        // Fail publication after an otherwise caught-up follower tick. The real
        // process must retain the outage count and last completed cycle.
        client.batch_execute("CREATE FUNCTION fail_cache_publication() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture publication failure'; END $$;
            CREATE TRIGGER fail_cache_publication BEFORE UPDATE ON bitcoin_core_header_cache_state FOR EACH ROW
            WHEN (NEW.publication_revision <> OLD.publication_revision)
            EXECUTE FUNCTION fail_cache_publication();").await?;
        rpc.state.mode.store(0, Ordering::Relaxed);
        until(async || Ok(client.query_one("SELECT state='recovering' FROM producer_runtime_state", &[]).await?.get(0)), Duration::from_secs(15)).await?;
        let after = client.query_one("SELECT last_success_at, consecutive_failures FROM producer_runtime_state", &[]).await?;
        assert_eq!(after.get::<_,Option<i64>>(0), before.get::<_,Option<i64>>(0));
        assert_eq!(after.get::<_,i64>(1), before.get::<_,i64>(1));

        client.batch_execute("DROP TRIGGER fail_cache_publication ON bitcoin_core_header_cache_state; DROP FUNCTION fail_cache_publication()").await?;
        until(async || Ok(client.query_one("SELECT state='healthy' AND consecutive_failures=0 AND last_success_at > $1 FROM producer_runtime_state", &[&before.get::<_,Option<i64>>(0)]).await?.get(0)), Duration::from_secs(15)).await?;
        stop(&mut core).await?;
        Ok(())
    }).await
}

async fn exercise(config: &mmm_pg::PgConfig) -> Result<()> {
    let client = mmm_pg::connect(config).await?;
    let rpc = RpcFixture::start().await?;
    rpc.state.mode.store(1, Ordering::Relaxed);
    let delay = std::env::var("MMM_TEST_RPC_DELAY_MS")
        .unwrap_or_else(|_| "0".into())
        .parse()?;
    rpc.state.delay_ms.store(delay, Ordering::Relaxed);
    let mut child = start(config, &rpc, &["poll-namecoin"])?;
    let mut core = start(config, &rpc, &["sync-bitcoin-core", "--follow"])?;
    let started = Instant::now();
    until(async || {
        ensure!(child.0.try_wait()?.is_none() && core.0.try_wait()?.is_none(), "producer exited during Core outage; inspect {} logs in temp directory", config.database);
        let row = client.query_one("SELECT count(*) FROM producer_runtime_state WHERE state = 'waiting' AND consecutive_failures >= 6", &[]).await?;
        Ok(row.get::<_,i64>(0) == 2)
    }, Duration::from_secs(360)).await?;
    assert_eq!(
        rpc.count("child"),
        0,
        "failed Core readiness must prevent child scans"
    );
    assert_eq!(
        client
            .query_one("SELECT count(*) FROM poll_cursor", &[])
            .await?
            .get::<_, i64>(0),
        0
    );
    let failures: i64 = client
        .query_one(
            "SELECT sum(consecutive_failures)::bigint FROM producer_runtime_state",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(rpc.count("getblockchaininfo"), usize::try_from(failures)?);
    assert_eq!(
        rpc.state.requests.lock().unwrap().len(),
        usize::try_from(failures)?
    );
    println!(
        "outage receipt: {failures} probes, no child/cache RPCs, delay_ms={delay}, elapsed={:?}",
        started.elapsed()
    );
    rpc.state.mode.store(0, Ordering::Relaxed);
    until(async || {
        ensure!(child.0.try_wait()?.is_none() && core.0.try_wait()?.is_none(), "producer exited during recovery; inspect {} logs", config.database);
        Ok(client.query_one("SELECT count(*) FROM producer_runtime_state WHERE state = 'healthy' AND consecutive_failures = 0", &[]).await?.get::<_,i64>(0) == 2)
    }, Duration::from_secs(210)).await?;
    println!(
        "recovery receipt: elapsed={:?}, Core RPCs={}, child calls={}",
        started.elapsed(),
        rpc.count("getblockchaininfo")
            + rpc.count("getblockcount")
            + rpc.count("getblockhash")
            + rpc.count("getblockheader")
            + rpc.count("getblock"),
        rpc.count("child")
    );
    println!("Core SIGTERM receipt: {:?}", stop(&mut core).await?);
    long_tick_heartbeat(&client, &rpc, &mut child).await
}

async fn long_tick_heartbeat(
    client: &Client,
    rpc: &RpcFixture,
    child: &mut Producer,
) -> Result<()> {
    // Fail attempt metadata before the long tick, then restore that write path.
    client
        .batch_execute(
            "CREATE SEQUENCE attempt_failures;
        CREATE FUNCTION fail_attempt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        PERFORM nextval('attempt_failures'); RAISE EXCEPTION 'fixture attempt failure'; END $$;
        CREATE TRIGGER fail_attempt BEFORE UPDATE ON producer_runtime_state FOR EACH ROW
        WHEN (NEW.last_attempt_at IS DISTINCT FROM OLD.last_attempt_at)
        EXECUTE FUNCTION fail_attempt();",
        )
        .await?;
    until(
        async || {
            Ok(client
                .query_one("SELECT is_called FROM attempt_failures", &[])
                .await?
                .get(0))
        },
        Duration::from_secs(10),
    )
    .await?;
    rpc.state.tip.store(100_000, Ordering::Relaxed);
    rpc.state.delay_ms.store(5_000, Ordering::Relaxed);
    // Let any previous tick finish before measuring the long snapshot.
    until(
        async || {
            Ok(rpc
                .state
                .requests
                .lock()
                .unwrap()
                .iter()
                .any(|(_, method, params)| {
                    method == "getblockhash"
                        && params[0].as_i64()
                            == Some(i64::from(mmm_capture::nbits_table::DAA_EPOCH_INTERVAL))
                }))
        },
        Duration::from_secs(90),
    )
    .await?;
    println!(
        "attempt metadata failures before long tick: {}",
        client
            .query_one("SELECT last_value FROM attempt_failures", &[])
            .await?
            .get::<_, i64>(0)
    );
    client.batch_execute("DROP TRIGGER fail_attempt ON producer_runtime_state; DROP FUNCTION fail_attempt(); DROP SEQUENCE attempt_failures").await?;
    // Sequence increments survive rollback, so only the first heartbeat fails.
    client
        .batch_execute(
            "CREATE SEQUENCE heartbeat_attempts;
        CREATE FUNCTION fail_first_heartbeat() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN
        IF nextval('heartbeat_attempts')=1 THEN RAISE EXCEPTION 'fixture heartbeat failure'; END IF;
        RETURN NEW; END $$;
        CREATE TRIGGER fail_first_heartbeat BEFORE UPDATE ON producer_runtime_state FOR EACH ROW
        WHEN (NEW.heartbeat_at <> OLD.heartbeat_at) EXECUTE FUNCTION fail_first_heartbeat();",
        )
        .await?;
    let before = client.query_one("SELECT heartbeat_at, last_success_at FROM producer_runtime_state r JOIN source s ON s.id=r.source_id WHERE s.code='auxpow:namecoin'", &[]).await?;
    // A real cache snapshot takes over 90s while each RPC remains within its own deadline.
    tokio::time::sleep(Duration::from_secs(95)).await;
    ensure!(
        child.0.try_wait()?.is_none(),
        "child exited during long cache preparation"
    );
    let after = client.query_one("SELECT heartbeat_at, last_success_at FROM producer_runtime_state r JOIN source s ON s.id=r.source_id WHERE s.code='auxpow:namecoin'", &[]).await?;
    let attempts = client
        .query_one("SELECT last_value, is_called FROM heartbeat_attempts", &[])
        .await?;
    client.batch_execute("DROP TRIGGER fail_first_heartbeat ON producer_runtime_state; DROP FUNCTION fail_first_heartbeat(); DROP SEQUENCE heartbeat_attempts").await?;
    ensure!(
        after.get::<_, i64>(0) >= before.get::<_, i64>(0) + 60,
        "heartbeat did not renew independently of work"
    );
    ensure!(
        attempts.get::<_, bool>(1),
        "fixture did not fail a heartbeat"
    );
    println!(
        "heartbeat attempts after one injected failure: {}",
        attempts.get::<_, i64>(0)
    );
    ensure!(
        after.get::<_, Option<i64>>(1) == before.get::<_, Option<i64>>(1),
        "fixture did not hold a long tick"
    );
    println!(
        "long tick receipt: heartbeat advanced {}s; child SIGTERM {:?}",
        after.get::<_, i64>(0) - before.get::<_, i64>(0),
        stop(child).await?
    );
    Ok(())
}

#[tokio::test]
async fn cli_retry_deadline_and_shutdown_survive_slow_status_rpc_and_cache_contention() -> Result<()>
{
    with_database(exercise_shutdown_and_deadline).await
}

async fn exercise_shutdown_and_deadline(config: &mmm_pg::PgConfig) -> Result<()> {
    let client = mmm_pg::connect(config).await?;
    let rpc = RpcFixture::start().await?;
    slow_status_deadline(config, &client, &rpc).await?;

    rpc.state.mode.store(7, Ordering::Relaxed);
    let before = rpc.count("getblockchaininfo");
    let mut child = start(config, &rpc, &["poll-namecoin"])?;
    until(
        async || Ok(rpc.count("getblockchaininfo") > before),
        Duration::from_secs(10),
    )
    .await?;
    println!(
        "startup blackhole SIGTERM receipt: {:?}",
        stop(&mut child).await?
    );

    rpc.state.mode.store(0, Ordering::Relaxed);
    mmm_store::lock_bitcoin_core_header_cache_shared(&client).await?;
    let mut child = start(config, &rpc, &["poll-namecoin"])?;
    let result = until(async || {
        Ok(client.query_one("SELECT count(*) FROM producer_runtime_state WHERE state='recovering' AND reason_code='core_cache_publication_pending'", &[]).await?.get::<_,i64>(0) == 1)
    }, Duration::from_secs(10)).await;
    let stopped = stop(&mut child).await;
    mmm_store::finish_bitcoin_core_header_cache_shared_operation(&client, result).await?;
    assert_eq!(
        rpc.count("child"),
        0,
        "cache conflicts must not fabricate completed startup"
    );
    println!("cache contention SIGTERM receipt: {:?}", stopped?);
    stop_during_publication_lock(config, &client, &rpc).await?;
    hathor_held_work_stays_recovering(config, &client, &rpc).await?;

    let source = mmm_store::get_source_id(&client, "auxpow:namecoin").await?;
    mmm_store::upsert_poll_cursor_with_target(&client, source, 1_000, Some(0)).await?;
    let mut child = start(config, &rpc, &["poll-namecoin"])?;
    until(
        async || {
            Ok(client
                .query_one(
                    "SELECT phase='running' FROM producer_runtime_state WHERE source_id=$1",
                    &[&source],
                )
                .await?
                .get(0))
        },
        Duration::from_secs(10),
    )
    .await?;
    let state: String = client
        .query_one(
            "SELECT state FROM producer_runtime_state WHERE source_id=$1",
            &[&source],
        )
        .await?
        .get(0);
    stop(&mut child).await?;
    assert_eq!(
        state, "recovering",
        "a child tip below the rescan window must not report a healthy idle tick"
    );

    rpc.state.mode.store(5, Ordering::Relaxed);
    let mut child = start(config, &rpc, &["poll-namecoin"])?;
    until(
        async || Ok(child.0.try_wait()?.is_some()),
        Duration::from_secs(10),
    )
    .await?;
    ensure!(!child.0.wait()?.success(), "authentication must fail-stop");
    let row = client
        .query_one("SELECT state, reason_code FROM producer_runtime_state r JOIN source s ON s.id=r.source_id WHERE s.code='auxpow:namecoin'", &[])
        .await?;
    assert_eq!(row.get::<_, String>(0), "failed");
    assert_eq!(row.get::<_, String>(1), "core_authentication");
    Ok(())
}

async fn stop_during_publication_lock(
    config: &mmm_pg::PgConfig,
    client: &Client,
    rpc: &RpcFixture,
) -> Result<()> {
    let mut blocker = mmm_pg::connect(config).await?;
    let transaction = blocker.transaction().await?;
    transaction
        .query_one(
            "SELECT singleton FROM bitcoin_core_header_cache_state FOR UPDATE",
            &[],
        )
        .await?;
    let mut child = start(config, rpc, &["poll-namecoin"])?;
    let result = until(async || Ok(client.query_one(
        "SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE '%publication_revision%FOR UPDATE%'", &[]).await?.get::<_,i64>(0)>0), Duration::from_secs(10)).await;
    let stopped = stop(&mut child).await;
    transaction.rollback().await?;
    result?;
    println!("publication row-lock SIGTERM receipt: {:?}", stopped?);
    Ok(())
}

async fn hathor_held_work_stays_recovering(
    config: &mmm_pg::PgConfig,
    client: &Client,
    rpc: &RpcFixture,
) -> Result<()> {
    let source = mmm_store::get_source_id(client, "auxpow:hathor").await?;
    mmm_store::upsert_poll_cursor_with_target(client, source, 1_000, Some(1_000)).await?;
    client.execute("INSERT INTO poll_pending_reconcile (source_id,height,reason) SELECT $1,h,'hathor_hold' FROM generate_series(1,1000) h", &[&source]).await?;
    let page = mmm_store::list_pending_reconcile(client, source, Some(26)).await?;
    assert_eq!(
        page.len(),
        26,
        "live recovery must fetch only its batch and sentinel"
    );
    assert_eq!((page[0].height, page[25].height), (1, 26));
    let mut producer = start(config, rpc, &["poll-hathor"])?;
    until(async || Ok(client.query_one("SELECT count(*) FROM producer_runtime_state WHERE source_id=$1 AND state='recovering' AND reason_code='capture_pending'", &[&source]).await?.get::<_,i64>(0)==1), Duration::from_secs(10)).await?;
    stop(&mut producer).await?;
    assert_eq!(rpc.count("hathor_hold"), 25);
    let row = client
        .query_one(
            "SELECT count(*),sum(attempts)::bigint FROM poll_pending_reconcile WHERE source_id=$1",
            &[&source],
        )
        .await?;
    assert_eq!((row.get::<_, i64>(0), row.get::<_, i64>(1)), (1000, 25));
    assert_eq!(
        mmm_store::load_poll_cursor(client, source).await?,
        Some(1_000)
    );
    Ok(())
}

async fn slow_status_deadline(
    config: &mmm_pg::PgConfig,
    client: &Client,
    rpc: &RpcFixture,
) -> Result<()> {
    // Delay the real status statement, long enough that sleeping for the chosen
    // duration after this write would overshoot its published second-resolution deadline.
    client.batch_execute("CREATE FUNCTION slow_wait_status() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(1.5); RETURN NEW; END $$;
        CREATE TRIGGER slow_wait_status BEFORE UPDATE ON producer_runtime_state FOR EACH ROW
        WHEN (NEW.state='waiting' AND NEW.consecutive_failures <> OLD.consecutive_failures)
        EXECUTE FUNCTION slow_wait_status();").await?;
    rpc.state.mode.store(1, Ordering::Relaxed);
    let anchor_wall = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    let anchor = Instant::now();
    let mut child = start(config, rpc, &["poll-namecoin"])?;
    until(async || Ok(client.query_one("SELECT count(*) FROM producer_runtime_state WHERE state='waiting' AND consecutive_failures=1", &[]).await?.get::<_,i64>(0) == 1), Duration::from_secs(10)).await?;
    let retry: i64 = client
        .query_one("SELECT next_retry_at FROM producer_runtime_state", &[])
        .await?
        .get(0);
    until(
        async || Ok(rpc.count("getblockchaininfo") >= 2),
        Duration::from_secs(8),
    )
    .await?;
    let second = rpc
        .state
        .requests
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, method, _)| method == "getblockchaininfo")
        .nth(1)
        .unwrap()
        .0;
    let observed = anchor_wall + second.duration_since(anchor).as_secs_f64();
    let late = observed - retry as f64;
    ensure!(
        (-0.1..1.2).contains(&late),
        "retry started {late:.3}s after its published second, including a 1.5s status write"
    );
    println!(
        "retry deadline receipt: actual-minus-published={late:.3}s, injected status-write delay=1.5s"
    );
    until(async || Ok(client.query_one("SELECT count(*) FROM producer_runtime_state WHERE state='waiting' AND consecutive_failures=2", &[]).await?.get::<_,i64>(0) == 1), Duration::from_secs(5)).await?;
    println!("backoff SIGTERM receipt: {:?}", stop(&mut child).await?);
    client.batch_execute("DROP TRIGGER slow_wait_status ON producer_runtime_state; DROP FUNCTION slow_wait_status()").await?;
    Ok(())
}
