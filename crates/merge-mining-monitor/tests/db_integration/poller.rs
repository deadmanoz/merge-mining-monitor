use std::time::Duration;

use anyhow::Result;
use mmm_capture::capture::{ClassificationProof, ResolvedPoolAttributions, build_event_payload};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_producers::{ChainPoller, ChainPollerState, HeightProgress, Poller, PollerConfig};
use mmm_store::{
    get_source_id, load_poll_cursor, upsert_merge_mining_event, upsert_poll_cursor_with_target,
};
use tokio_postgres::Client;

use crate::support::db::connect_to_schema;
use crate::support::{default_pool_snapshot, parse_auxpow_fixture};

// ---------------------------------------------------------------------------
// poll_cursor: live-poll cursor decoupled from merge_mining_event.
// ---------------------------------------------------------------------------

/// Minimal `ChainPoller` for driving `Poller::new`/`poll_tick` against the test
/// schema. It owns its own search-path-scoped connection (the poller takes the
/// chain by value), advances every height up to `fail_above`, and errors past
/// it so partial-progress persistence can be asserted.
struct FakeChain {
    state: ChainPollerState,
    tip: i32,
    fail_above: Option<i32>,
}

impl ChainPoller for FakeChain {
    // Keep a fake name for log clarity; spec, source id, and DB access come
    // from `state`.
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
async fn poll_cursor_round_trip_is_monotonic() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, None);

        upsert_poll_cursor_with_target(&client, source_id, 1_000, None).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_000));

        // A lower replay must not downgrade persisted live progress.
        upsert_poll_cursor_with_target(&client, source_id, 500, None).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_000));

        // A higher value advances.
        upsert_poll_cursor_with_target(&client, source_id, 1_500, None).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_500));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poll_cursor_target_updates_do_not_refresh_progress_time() -> Result<()> {
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;

        upsert_poll_cursor_with_target(&client, source_id, 1_000, Some(1_100)).await?;
        let (_, _, inserted_at) = load_poll_cursor_row(&client, source_id).await?;

        // A lower replay may update the observed target, but must not downgrade
        // progress or refresh the progress timestamp.
        upsert_poll_cursor_with_target(&client, source_id, 500, Some(900)).await?;
        assert_eq!(
            load_poll_cursor_row(&client, source_id).await?,
            (1_000, Some(900), inserted_at)
        );

        // A targetless progress write must preserve the last observed target.
        upsert_poll_cursor_with_target(&client, source_id, 1_500, None).await?;
        let (cursor, target, _) = load_poll_cursor_row(&client, source_id).await?;
        assert_eq!((cursor, target), (1_500, Some(900)));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poll_cursor_unaffected_by_event_writes() -> Result<()> {
    crate::run_db_test!(client, {
        default_pool_snapshot(&client).await?;
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;

        upsert_poll_cursor_with_target(&client, source_id, 800_000, None).await?;

        // A backfill-style event write at a low height must not touch the
        // live cursor.
        let parsed = parse_auxpow_fixture("500000-valid-parent")?;
        let event = build_event_payload(
            &parsed,
            Some(500_000),
            ResolvedPoolAttributions::default(),
            ClassificationProof::default(),
            111,
        )?;
        upsert_merge_mining_event(&client, source_id, &event).await?;

        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(800_000));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poller_seeds_fresh_at_tip_then_resumes() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let config = monotonic_poll_config(0);

        // Fresh source (no row, no override), zero-reorg, tip 1000: seed at the
        // tip and persist it immediately so the empty first tick cannot lose it.
        let chain = fake_chain(&schema, 1_000, None).await?;
        let _poller = Poller::new(chain, config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_000));

        // Rebuild after the tip moved to 1500: must RESUME from 1000, not
        // re-anchor to the newer tip. The resume path leaves the seed unchanged.
        let chain2 = fake_chain(&schema, 1_500, None).await?;
        let mut poller2 = Poller::new(chain2, config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_000));

        // One tick advances a batch from 1001 (resumed), proving it did not skip
        // to the tip.
        poller2.poll_tick().await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(1_100));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn poller_persists_partial_progress_on_tick_error() -> Result<()> {
    crate::run_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        // Pre-seed the live cursor so the build resumes from 1000.
        upsert_poll_cursor_with_target(&client, source_id, 1_000, None).await?;

        // Tip 1010; process_height errors above 1005. The tick advances
        // 1001..=1005 then fails at 1006.
        let chain = fake_chain(&schema, 1_010, Some(1_005)).await?;
        let mut poller = Poller::new(chain, monotonic_poll_config(0)).await?;

        let result = poller.poll_tick().await;
        assert!(result.is_err(), "tick should propagate the height error");

        // The completed heights are persisted, not discarded.
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

        // First deploy: explicit override with no prior row seeds at start - 1
        // and persists it immediately (this is what the first-deploy rollout
        // relies on to bootstrap poll_cursor).
        let chain = fake_chain(&schema, 5_000, None).await?;
        let _poller = Poller::new(chain, override_config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(999));

        // Raise the persisted cursor, then a lower override replay must NOT
        // downgrade it (build takes the override path, persist_seed is false
        // because a row exists, and any later persist is monotonic).
        upsert_poll_cursor_with_target(&client, source_id, 5_000, None).await?;
        let chain2 = fake_chain(&schema, 5_000, None).await?;
        let _poller2 = Poller::new(chain2, override_config).await?;
        assert_eq!(load_poll_cursor(&client, source_id).await?, Some(5_000));
        Ok::<_, anyhow::Error>(())
    })
}
