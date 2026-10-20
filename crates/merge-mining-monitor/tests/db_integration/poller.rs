use std::time::Duration;

use anyhow::Result;
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_producers::{ChainPoller, ChainPollerState, HeightProgress, Poller, PollerConfig};
use mmm_store::{
    get_source_id, load_poll_cursor, lock_child_chain_height_session,
    upsert_poll_cursor_with_target,
};
use tokio_postgres::Client;

use crate::support::db::connect_to_schema;

struct FakeChain {
    state: ChainPollerState,
    tip: i32,
    fail_above: Option<i32>,
    processed: Option<std::sync::Arc<std::sync::Mutex<Vec<i32>>>>,
}

impl ChainPoller for FakeChain {
    fn name(&self) -> &'static str {
        "FakeChain"
    }

    fn poller_state(&self) -> &ChainPollerState {
        &self.state
    }

    fn client_mut(&mut self) -> &mut Client {
        self.state.client_mut()
    }

    async fn chain_tip(&self) -> Result<i32> {
        Ok(self.tip)
    }
    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        if let Some(max) = self.fail_above
            && height > max
        {
            anyhow::bail!("fake process_height failure at {height}");
        }
        if let Some(processed) = &self.processed {
            processed.lock().unwrap().push(height);
        }
        Ok(HeightProgress::Advance)
    }
}

async fn fake_chain(schema: &str, tip: i32, fail_above: Option<i32>) -> Result<FakeChain> {
    let client = connect_to_schema(schema).await?;
    let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
    Ok(FakeChain {
        state: ChainPollerState::new(
            mmm_producers::chains::by_id(mmm_producers::chains::ChainId::Namecoin),
            source_id,
            client,
        ),
        tip,
        fail_above,
        processed: None,
    })
}

#[tokio::test]
async fn explicit_overlap_handoff_covers_moving_tip_and_restart() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let processed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let config = PollerConfig {
            start_height_override: Some(997),
            ..monotonic_poll_config(4)
        };
        let mut chain = fake_chain(&schema, 1_003, None).await?;
        chain.processed = Some(processed.clone());
        let mut poller = Poller::new(chain, config).await?;
        poller.poll_tick().await?;
        assert_eq!(
            *processed.lock().unwrap(),
            (997..=1_003).collect::<Vec<_>>()
        );
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_003));
        drop(poller);
        processed.lock().unwrap().clear();
        let mut resumed = fake_chain(&schema, 1_005, None).await?;
        resumed.processed = Some(processed.clone());
        Poller::new(resumed, monotonic_poll_config(4))
            .await?
            .poll_tick()
            .await?;
        assert_eq!(
            *processed.lock().unwrap(),
            (1_000..=1_005).collect::<Vec<_>>()
        );

        upsert_poll_cursor_with_target(&client, source_id, 1_050, None).await?;
        processed.lock().unwrap().clear();
        let mut failing = fake_chain(&schema, 1_050, Some(1_001)).await?;
        failing.processed = Some(processed.clone());
        assert!(
            Poller::new(failing, config)
                .await?
                .poll_tick()
                .await
                .is_err()
        );
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_050));
        assert_eq!(
            *processed.lock().unwrap(),
            (997..=1_001).collect::<Vec<_>>()
        );
        processed.lock().unwrap().clear();
        let mut repaired = fake_chain(&schema, 1_050, None).await?;
        repaired.processed = Some(processed.clone());
        Poller::new(repaired, config).await?.poll_tick().await?;
        assert_eq!(
            *processed.lock().unwrap(),
            (997..=1_050).collect::<Vec<_>>()
        );
        Ok::<_, anyhow::Error>(())
    })
}

fn monotonic_poll_config(reorg_depth: i32) -> PollerConfig {
    PollerConfig {
        start_height_override: None,
        poll_interval: Duration::from_secs(30),
        batch_size: 100,
        reorg_depth,
    }
}

async fn load_poll_cursor_row(client: &Client, source_id: i64) -> Result<(i32, Option<i32>, i64)> {
    let row = client
        .query_one(
            "SELECT cursor_height, target_height, EXTRACT(EPOCH FROM updated_at)::BIGINT \
             FROM poll_cursor \
             WHERE source_id = $1",
            &[&source_id],
        )
        .await?;
    Ok((row.get(0), row.get(1), row.get(2)))
}

#[tokio::test]
async fn poll_cursor_target_updates_do_not_refresh_progress_time() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;

        upsert_poll_cursor_with_target(&client, source_id, 1_000, Some(1_100)).await?;
        let (_, _, inserted_at) = load_poll_cursor_row(&client, source_id).await?;

        upsert_poll_cursor_with_target(&client, source_id, 500, Some(900)).await?;
        assert_eq!(
            load_poll_cursor_row(&client, source_id).await?,
            (1_000, Some(900), inserted_at)
        );

        upsert_poll_cursor_with_target(&client, source_id, 1_500, None).await?;
        let (cursor, target, _) = load_poll_cursor_row(&client, source_id).await?;
        assert_eq!((cursor, target), (1_500, Some(900)));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poller_seeds_fresh_at_tip_then_resumes() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let config = monotonic_poll_config(0);

        let chain = fake_chain(&schema, 1_000, None).await?;
        let _poller = Poller::new(chain, config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_000));

        let chain2 = fake_chain(&schema, 1_500, None).await?;
        let mut poller2 = Poller::new(chain2, config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_000));

        poller2.poll_tick().await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_100));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poller_persists_partial_progress_on_tick_error() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        upsert_poll_cursor_with_target(&client, source_id, 1_000, None).await?;

        let chain = fake_chain(&schema, 1_010, Some(1_005)).await?;
        let mut poller = Poller::new(chain, monotonic_poll_config(0)).await?;

        let result = poller.poll_tick().await;
        assert!(result.is_err(), "tick should propagate the height error");

        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_005));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poller_empty_tick_persists_target_without_refreshing_cursor_time() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        upsert_poll_cursor_with_target(&client, source_id, 1_000, None).await?;

        let fixed_updated_at = 1_780_000_000_f64;
        client
            .execute(
                "UPDATE poll_cursor \
                 SET target_height = NULL, updated_at = to_timestamp($2::DOUBLE PRECISION) \
                 WHERE source_id = $1",
                &[&source_id, &fixed_updated_at],
            )
            .await?;

        let chain = fake_chain(&schema, 1_000, None).await?;
        let mut poller = Poller::new(chain, monotonic_poll_config(0)).await?;
        assert_eq!(poller.poll_tick().await?, 0);

        assert_eq!(
            load_poll_cursor_row(&client, source_id).await?,
            (1_000, Some(1_000), fixed_updated_at as i64)
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poller_override_seeds_fresh_and_does_not_downgrade_on_replay() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let override_config = PollerConfig {
            start_height_override: Some(1_000),
            poll_interval: Duration::from_secs(30),
            batch_size: 100,
            reorg_depth: 0,
        };

        let chain = fake_chain(&schema, 5_000, None).await?;
        let _poller = Poller::new(chain, override_config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(999));

        upsert_poll_cursor_with_target(&client, source_id, 5_000, None).await?;
        let chain2 = fake_chain(&schema, 5_000, None).await?;
        let _poller2 = Poller::new(chain2, override_config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(5_000));
        Ok::<_, anyhow::Error>(())
    })
}

/// Seed `child_chain_head` rows at heights 990..=1,000 behind a cursor at 1,000.
async fn seed_chain_heads_behind_cursor(client: &Client, source_id: i64) -> Result<()> {
    upsert_poll_cursor_with_target(client, source_id, 1_000, None).await?;
    client
        .execute(
            "INSERT INTO child_chain_head \
                 (source_id, child_height, block_hash, outcome, core_cache_generation, observed_at) \
             SELECT $1, h, decode(lpad(to_hex(h), 64, '0'), 'hex'), 'captured', 0, 1 \
               FROM generate_series(990, 1_000) AS h",
            &[&source_id],
        )
        .await?;
    Ok(())
}

async fn chain_head_heights(client: &Client, source_id: i64) -> Result<Vec<i32>> {
    Ok(client
        .query(
            "SELECT child_height FROM child_chain_head WHERE source_id = $1 ORDER BY 1",
            &[&source_id],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

#[tokio::test]
async fn a_tick_prunes_chain_heads_below_the_rescan_floor() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        seed_chain_heads_behind_cursor(&client, source_id).await?;
        let chain = fake_chain(&schema, 1_003, None).await?;
        Poller::new(chain, monotonic_poll_config(4))
            .await?
            .poll_tick()
            .await?;
        // The cursor reached 1,003, so the rescan window starts at 1,000.
        assert_eq!(chain_head_heights(&client, source_id).await?, vec![1_000]);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn pruning_keeps_heads_from_the_start_override_and_locked_heights() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        seed_chain_heads_behind_cursor(&client, source_id).await?;
        // Another session mid-rescan at 990 holds its height lock.
        lock_child_chain_height_session(&client, source_id, 990).await?;
        let config = PollerConfig {
            start_height_override: Some(992),
            ..monotonic_poll_config(4)
        };
        let chain = fake_chain(&schema, 1_003, None).await?;
        Poller::new(chain, config).await?.poll_tick().await?;
        let mut expected = vec![990];
        expected.extend(992..=1_000);
        assert_eq!(chain_head_heights(&client, source_id).await?, expected);
        Ok::<_, anyhow::Error>(())
    })
}
