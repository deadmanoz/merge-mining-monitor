use std::time::Duration;

use anyhow::Result;
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier, ParentClassification};
use mmm_producers::refresh_bitcoin_core_header_cache;
use mmm_store::{
    finish_bitcoin_core_header_cache_operation, lock_bitcoin_core_header_cache,
    replace_bitcoin_core_header_cache,
};

#[tokio::test]
async fn unavailable_core_is_reported_without_waiting_for_another_cache_writer() -> Result<()> {
    crate::run_db_test!(client, schema, {
        lock_bitcoin_core_header_cache(&client).await?;
        let mut observer = crate::support::db::connect_to_schema(&schema).await?;
        let header = crate::support::seed::test_header_chain(0, 1_800_000_000)[&0];
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(&header))
                .with_synced_tip_error(),
        );
        let outcome = tokio::time::timeout(
            Duration::from_millis(300),
            refresh_bitcoin_core_header_cache(&mut observer, &classifier),
        )
        .await;
        finish_bitcoin_core_header_cache_operation(&client, Ok(())).await?;
        assert!(
            outcome.is_ok(),
            "Core readiness must be checked before waiting for the cache lock"
        );
        assert!(outcome.unwrap().is_err());
        Ok(())
    })
}

#[tokio::test]
async fn cache_publication_rejects_old_revision_and_yields_to_readers() -> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        crate::support::db::clear_bitcoin_history(&client).await?;
        let revision = mmm_store::load_core_cache_publication_revision(&client).await?;
        let epoch = cache_epoch();
        let mut horizon = mmm_store::BitcoinCoreHeader {
            height: 5,
            block_hash: vec![2; 32],
            block_time: 6,
            bits: epoch.bits,
        };
        let mut publisher = crate::support::db::connect_to_schema(&schema).await?;
        mmm_store::lock_bitcoin_core_header_cache_shared(&client).await?;
        let epochs = [epoch];
        let (published, released) = tokio::join!(
            replace_bitcoin_core_header_cache(
                &mut publisher,
                0,
                &epochs,
                None,
                &horizon,
                false,
                revision,
            ),
            async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                mmm_store::finish_bitcoin_core_header_cache_shared_operation(&client, Ok(())).await
            }
        );
        released?;
        assert!(
            published?.is_some(),
            "brief reader contention should settle within the publication bound"
        );
        let generation = mmm_store::load_core_cache_generation(&client).await?;
        let current = mmm_store::load_core_cache_publication_revision(&client).await?;
        // Even an identical replacement changes publication ownership, without invalidating verdicts.
        assert!(
            replace_bitcoin_core_header_cache(
                &mut publisher,
                0,
                &[],
                None,
                &horizon,
                false,
                current
            )
            .await?
            .is_some()
        );
        assert_eq!(
            mmm_store::load_core_cache_generation(&client).await?,
            generation
        );
        horizon.block_hash = vec![3; 32];
        for height in [5, 6] {
            horizon.height = height;
            assert!(
                replace_bitcoin_core_header_cache(
                    &mut client,
                    0,
                    &[],
                    None,
                    &horizon,
                    false,
                    current
                )
                .await?
                .is_none()
            );
        }
        let current = mmm_store::load_core_cache_publication_revision(&client).await?;
        mmm_store::lock_bitcoin_core_header_cache_shared(&client).await?;
        let result = tokio::time::timeout(
            Duration::from_millis(800),
            replace_bitcoin_core_header_cache(
                &mut publisher,
                0,
                &[],
                None,
                &horizon,
                false,
                current,
            ),
        )
        .await;
        mmm_store::finish_bitcoin_core_header_cache_shared_operation(&client, Ok(())).await?;
        assert_eq!(
            result?
                .unwrap_err()
                .downcast_ref::<tokio_postgres::Error>()
                .and_then(tokio_postgres::Error::code),
            Some(&tokio_postgres::error::SqlState::LOCK_NOT_AVAILABLE),
            "publication must yield after its 500 ms lock budget"
        );
        let stored = mmm_store::load_bitcoin_core_header_cache_horizon(&client)
            .await?
            .unwrap();
        assert_eq!(stored.block_hash, vec![2; 32]);
        Ok(())
    })
}

#[tokio::test]
async fn runtime_writes_are_fenced_and_never_advance_progress() -> Result<()> {
    crate::run_db_test!(client, {
        let source = mmm_store::get_source_id(&client, "auxpow:namecoin").await?;
        let first = mmm_store::start_producer_run(&client, source).await?;
        let next = mmm_store::RuntimeTransition {
            phase: "startup",
            state: "waiting",
            dependency: Some("bitcoin-core"),
            reason_code: Some("core_warmup"),
            failures: 6,
            next_retry_at: Some(1_800_000_005),
        };
        mmm_store::transition_producer_run(&client, first, &next).await?;
        let second = mmm_store::start_producer_run(&client, source).await?;
        assert_ne!(first.run_id, second.run_id);
        assert!(
            mmm_store::heartbeat_producer_run(&client, first)
                .await
                .is_err()
        );
        assert!(
            mmm_store::transition_producer_run(&client, first, &next)
                .await
                .is_err()
        );
        mmm_store::transition_producer_run(&client, second, &next).await?;
        mmm_store::heartbeat_producer_run(&client, second).await?;
        assert!(
            mmm_store::load_poll_cursor(&client, source)
                .await?
                .is_none()
        );
        assert_eq!(client.query_one("SELECT state, consecutive_failures FROM producer_runtime_state WHERE source_id = $1", &[&source]).await?.get::<_,String>(0), "waiting");
        Ok(())
    })
}

#[tokio::test]
async fn live_readiness_is_typed_and_unready_core_never_attests_absence() -> Result<()> {
    use mmm_bitcoin_core::{
        BitcoinCoreParentClassifier, CoreFault, CoreUnavailable, ParentPreflight,
    };
    use std::sync::atomic::Ordering;
    let rpc = crate::support::live_rpc::RpcFixture::start().await?;
    let classifier = BitcoinCoreParentClassifier::from_live_env_url(&rpc.url)?;
    for (mode, expected) in [
        (1, CoreUnavailable::Warmup),
        (2, CoreUnavailable::InitialBlockDownload),
        (3, CoreUnavailable::HeaderLag),
        (4, CoreUnavailable::StaleTip),
    ] {
        rpc.state.mode.store(mode, Ordering::Relaxed);
        let before = rpc.count("getblockchaininfo");
        let err = classifier.ready_tip().await.unwrap_err();
        assert_eq!(err.downcast_ref::<CoreUnavailable>(), Some(&expected));
        assert_eq!(rpc.count("getblockchaininfo") - before, 1);
    }
    for (mode, expected) in [
        (5, CoreFault::Authentication),
        (6, CoreFault::InvalidResponse),
        (8, CoreFault::WrongNetwork),
    ] {
        rpc.state.mode.store(mode, Ordering::Relaxed);
        assert_eq!(
            classifier
                .ready_tip()
                .await
                .unwrap_err()
                .downcast_ref::<CoreFault>(),
            Some(&expected)
        );
    }
    rpc.state.mode.store(9, Ordering::Relaxed);
    let genesis = bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header;
    let classifier = ConfiguredParentClassifier::BitcoinCore(classifier);
    assert_eq!(
        classifier
            .classify_parent(&genesis, ParentPreflight { known_prev: None })
            .await
            .unwrap_err()
            .downcast_ref::<CoreFault>(),
        Some(&CoreFault::InvalidResponse)
    );
    rpc.state.mode.store(2, Ordering::Relaxed);
    let mut candidate =
        bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header;
    candidate.nonce = 55;
    let err = classifier
        .classify_parent(&candidate, ParentPreflight { known_prev: None })
        .await
        .unwrap_err();
    assert_eq!(
        err.downcast_ref::<CoreUnavailable>(),
        Some(&CoreUnavailable::InitialBlockDownload)
    );
    Ok(())
}

fn cache_epoch() -> mmm_store::BitcoinCoreHeader {
    mmm_store::BitcoinCoreHeader {
        height: 0,
        block_hash: vec![1; 32],
        block_time: 1,
        bits: 0x1d00_ffff,
    }
}

#[tokio::test]
async fn live_recovery_counts_expansions_in_its_batch_budget() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source = mmm_store::get_source_id(&client, "live-chaintip:bitcoin:core").await?;
        client.execute("INSERT INTO bitcoin_core_reconcile_queue (source_id,btc_parent_header_hash,primary_pending)
            SELECT $1,decode(lpad(to_hex(n),64,'0'),'hex'),false FROM generate_series(1,26) n", &[&source]).await?;
        let rpc = crate::support::live_rpc::RpcFixture::start().await?;
        let classifier = ConfiguredParentClassifier::BitcoinCore(
            mmm_bitcoin_core::BitcoinCoreParentClassifier::from_live_env_url(&rpc.url)?,
        );
        let revision = mmm_store::load_core_cache_publication_revision(&client).await?;
        let error = refresh_bitcoin_core_header_cache(&mut client, &classifier)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "core_reconciliation_pending");
        assert_eq!(
            client
                .query_one("SELECT count(*) FROM bitcoin_core_reconcile_queue", &[])
                .await?
                .get::<_, i64>(0),
            1
        );
        assert_eq!(
            mmm_store::load_core_cache_publication_revision(&client).await?,
            revision
        );
        assert_eq!(rpc.count("getblockchaininfo"), 1);
        assert_eq!(
            rpc.state.requests.lock().unwrap().len(),
            1,
            "yield before cache RPC preparation"
        );
        Ok(())
    })
}

#[tokio::test]
async fn live_cold_cache_publishes_only_a_bounded_safe_prefix() -> Result<()> {
    use std::sync::atomic::Ordering;
    crate::run_mut_db_test!(client, {
        crate::support::db::clear_bitcoin_history(&client).await?;
        let rpc = crate::support::live_rpc::RpcFixture::start().await?;
        let delay = std::env::var("MMM_TEST_RPC_DELAY_MS")
            .unwrap_or_else(|_| "0".into())
            .parse()?;
        rpc.state.delay_ms.store(delay, Ordering::Relaxed);
        let started = std::time::Instant::now();
        rpc.state.tip.store(100_000, Ordering::Relaxed);
        let classifier = ConfiguredParentClassifier::BitcoinCore(
            mmm_bitcoin_core::BitcoinCoreParentClassifier::from_live_env_url(&rpc.url)?,
        );
        let err = refresh_bitcoin_core_header_cache(&mut client, &classifier)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("core_cache_bootstrap_pending"));
        let table = mmm_store::load_bitcoin_core_nbits_table(&client).await?;
        assert_eq!(table.horizon_height(), 48_484);
        assert_eq!(
            client
                .query_one(
                    "SELECT count(*) FROM bitcoin_core_header WHERE is_final",
                    &[]
                )
                .await?
                .get::<_, i64>(0),
            25
        );
        assert_eq!(rpc.count("getblockchaininfo"), 3);
        assert_eq!(rpc.count("getblockhash"), 27);
        assert_eq!(rpc.count("getblockheader"), 27);
        println!(
            "cold cache receipt: 25 epochs, 57 RPCs, delay_ms={delay}, elapsed={:?}",
            started.elapsed()
        );
        Ok(())
    })
}
