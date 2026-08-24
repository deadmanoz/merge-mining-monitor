use anyhow::Result;
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, CoreHeader, FakeParentClassifier, ParentClassification,
};
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::capture::ClassificationProof;
use mmm_capture::nbits_table::NbitsLookup;
use mmm_producers::refresh_bitcoin_core_header_cache;
use mmm_read_model::reconcile_from_merge_mining_event;
use mmm_store::{
    BitcoinCoreHeader, complete_bitcoin_core_header_cache_reclassification,
    finish_bitcoin_core_header_cache_operation, load_bitcoin_core_nbits_table,
    load_bitcoin_core_nbits_table_if_present, lock_bitcoin_core_header_cache,
    record_bitcoin_core_header, replace_bitcoin_core_header_cache, upsert_merge_mining_event,
};

use crate::support::db::connect_to_schema;
use crate::support::scenario::orphan_candidate_verdict;
use crate::support::{namecoin_event_payload, namecoin_fixture};

fn header(height: i32, hash_byte: u8, block_time: i64, bits: u32) -> BitcoinCoreHeader {
    BitcoinCoreHeader {
        height,
        block_hash: vec![hash_byte; 32],
        block_time,
        bits,
    }
}

fn core_header(height: i32, hash_byte: u8, block_time: i64, bits: u32) -> CoreHeader {
    CoreHeader {
        height,
        hash: BlockHash::from_byte_array([hash_byte; 32]),
        header_time: block_time,
        nbits: bits,
    }
}

#[tokio::test]
async fn core_header_cache_retains_epochs_replaces_horizon_and_rejects_conflicts() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // `new_test_db` supplies this genesis row. An identical observation is
        // harmless, while a conflicting row below must fail closed.
        record_bitcoin_core_header(&client, &header(0, 0, 1, 0x1d00_ffff)).await?;
        record_bitcoin_core_header(&client, &header(2016, 1, 2, 0x1c00_ffff)).await?;
        replace_bitcoin_core_header_cache(
            &mut client,
            2016,
            &[],
            None,
            &header(2020, 2, 3, 0x1c00_ffff),
            false,
        )
        .await?;
        replace_bitcoin_core_header_cache(
            &mut client,
            2016,
            &[],
            None,
            &header(2030, 3, 4, 0x1c00_ffff),
            false,
        )
        .await?;

        let table = load_bitcoin_core_nbits_table(&client).await?;
        assert_eq!(table.horizon_height(), 2030);
        assert_eq!(table.expected_nbits(2017), NbitsLookup::Found(0x1c00_ffff));

        let stale_horizons: i64 = client
            .query_one(
                "SELECT count(*) FROM bitcoin_core_header WHERE height % 2016 <> 0",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(stale_horizons, 1);

        let err = record_bitcoin_core_header(&client, &header(2016, 1, 2, 0x1c00_fffe))
            .await
            .expect_err("a conflicting Core observation must fail closed");
        assert!(err.to_string().contains("disagrees"));
        Ok(())
    })
}

#[tokio::test]
async fn cache_refresh_keeps_timestamp_coverage_and_retries_an_unacknowledged_sweep() -> Result<()>
{
    crate::run_mut_db_test!(client, {
        client
            .execute(
                "UPDATE bitcoin_core_header_cache_state \
                 SET horizon_time = 0, reclassification_needed = FALSE",
                &[],
            )
            .await?;
        let first = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[],
            None,
            &header(100, 1, 100, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(first.reclassification_needed);
        assert!(
            !first.recheck_orphans,
            "initial coverage classifies pending rows without sweeping existing orphans"
        );
        complete_bitcoin_core_header_cache_reclassification(&client).await?;

        let advanced_with_an_older_timestamp = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[],
            None,
            &header(101, 2, 99, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(advanced_with_an_older_timestamp.reclassification_needed);
        assert!(
            !advanced_with_an_older_timestamp.recheck_orphans,
            "ordinary horizon advances do not revisit already classified orphans"
        );
        assert_eq!(
            load_bitcoin_core_nbits_table(&client).await?.horizon_time(),
            100
        );

        let retry = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[],
            None,
            &header(101, 2, 99, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(retry.reclassification_needed);
        complete_bitcoin_core_header_cache_reclassification(&client).await?;

        let settled = replace_bitcoin_core_header_cache(
            &mut client,
            0,
            &[],
            None,
            &header(101, 2, 99, 0x1d00_ffff),
            false,
        )
        .await?;
        assert!(!settled.reclassification_needed);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_reads_epoch_boundaries_and_the_current_horizon_from_core() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        assert!(
            load_bitcoin_core_nbits_table_if_present(&client)
                .await?
                .is_none()
        );
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );

        let table = refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        assert_eq!(table.horizon_height(), 2030);
        assert_eq!(table.expected_nbits(2017), NbitsLookup::Found(0x1c00_ffff));

        let heights = client
            .query(
                "SELECT height FROM bitcoin_core_header ORDER BY height",
                &[],
            )
            .await?
            .iter()
            .map(|row| row.get::<_, i32>(0))
            .collect::<Vec<_>>();
        assert_eq!(heights, [0, 2016, 2030]);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_rejects_a_non_mainnet_core_node() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_non_mainnet_synced_tip(2030),
        );
        let error = refresh_bitcoin_core_header_cache(&mut client, &classifier)
            .await
            .expect_err("testnet, signet, and regtest must not populate the mainnet cache");
        assert!(error.to_string().contains("connected to mainnet"));
        Ok(())
    })
}

#[tokio::test]
async fn core_header_cache_refresh_lock_serializes_sessions() -> Result<()> {
    crate::run_db_test!(client, schema, {
        lock_bitcoin_core_header_cache(&client).await?;
        let waiting_client = connect_to_schema(&schema).await?;
        let mut waiter = tokio::spawn(async move {
            lock_bitcoin_core_header_cache(&waiting_client).await?;
            finish_bitcoin_core_header_cache_operation(&waiting_client, Ok(())).await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "a second Core-cache refresh must wait for the current observation"
        );

        finish_bitcoin_core_header_cache_operation(&client, Ok(())).await?;
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut waiter)
            .await
            .expect("waiting Core-cache refresh did not resume")??;
        Ok(())
    })
}

#[tokio::test]
async fn refresh_replaces_a_shallow_epoch_boundary_from_core() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let first = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &first).await?;

        let replacement = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(2016, 9, 4, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &replacement).await?;

        let block_hash: Vec<u8> = client
            .query_one(
                "SELECT block_hash FROM bitcoin_core_header WHERE height = 2016",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(block_hash, vec![9; 32]);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_retries_a_same_height_core_reorg_before_writing() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let classifier = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header_sequence([
                core_header(2030, 2, 3, 0x1c00_ffff),
                core_header(2030, 9, 4, 0x1c00_ffff),
            ]),
        );

        let table = refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        assert_eq!(table.horizon_height(), 2030);
        let horizon_hash: Vec<u8> = client
            .query_one(
                "SELECT block_hash FROM bitcoin_core_header WHERE height = 2030",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(horizon_hash, vec![9; 32]);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_detects_an_advancing_tip_reorg_at_the_prior_horizon() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let first = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 100, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &first).await?;
        assert_eq!(
            load_bitcoin_core_nbits_table(&client).await?.horizon_time(),
            100
        );

        // A new tip can conceal a reorg at the old horizon because that height
        // is no longer part of the next sparse replacement. The refresh must
        // verify it directly before preserving the prior timestamp high-water.
        let reorged = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2031)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 9, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2031, 8, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &reorged).await?;
        assert_eq!(
            load_bitcoin_core_nbits_table(&client).await?.horizon_time(),
            3,
            "a reorg replaces, rather than preserves, the stale timestamp horizon"
        );
        Ok(())
    })
}

#[tokio::test]
async fn refresh_rejects_a_horizon_below_an_existing_final_epoch() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let finalized = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2116)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2116, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &finalized).await?;

        let lagging = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2015)
            .with_canonical_header(core_header(2015, 3, 4, 0x1d00_ffff)),
        );
        let error = refresh_bitcoin_core_header_cache(&mut client, &lagging)
            .await
            .expect_err("a lagging Core node must not move the cache before a finalized epoch");
        assert!(
            error.to_string().contains("highest finalized epoch"),
            "unexpected error: {error:#}"
        );
        Ok(())
    })
}

#[tokio::test]
async fn shallow_cache_reorg_reclassifies_existing_orphans_and_source_health() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (resolver, pool_ids_by_slug, source_id, parsed) = namecoin_fixture(&client).await?;
        let parent_height = parse_bip34_height(&parsed.parent_coinbase_script)
            .expect("Namecoin fixture carries a BIP34 parent height");
        let parent_header = parsed.parent_header.header;
        crate::support::db::seed_bitcoin_core_header_cache_through(
            &client,
            parent_height,
            i64::from(parent_header.time),
            parent_header.bits.to_consensus(),
        )
        .await?;
        let epoch = mmm_capture::nbits_table::daa_epoch_start(parent_height);
        client
            .execute(
                "UPDATE bitcoin_core_header SET is_final = FALSE WHERE height = $1",
                &[&epoch],
            )
            .await?;
        let payload = namecoin_event_payload(
            &parsed,
            &resolver,
            &pool_ids_by_slug,
            500_000,
            ClassificationProof::default(),
            1_000,
        )?;
        let event_id = upsert_merge_mining_event(&client, source_id, &payload)
            .await?
            .event_id;
        let absent = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&parent_header),
        ));
        reconcile_from_merge_mining_event(&mut client, event_id, &absent, None).await?;
        let parent_hash = parsed.parent_header.hash().to_byte_array().to_vec();
        let before: Option<String> = client
            .query_one(
                "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await?
            .get(0);
        assert_eq!(before.as_deref(), Some("strict_btc_orphan"));

        let core_tip = parent_height + 100;
        let reorged = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(orphan_candidate_verdict(&parent_header))
                .with_synced_tip_height(core_tip)
                .with_canonical_header(core_header(
                    epoch,
                    9,
                    i64::from(epoch) + 1,
                    parent_header.bits.to_consensus() ^ 1,
                ))
                .with_canonical_header(core_header(
                    core_tip,
                    8,
                    i64::from(parent_header.time) + 1,
                    parent_header.bits.to_consensus(),
                )),
        );
        refresh_bitcoin_core_header_cache(&mut client, &reorged).await?;

        let after: Option<String> = client
            .query_one(
                "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
                &[&parent_hash],
            )
            .await?
            .get(0);
        assert_eq!(after.as_deref(), Some("excluded"));
        let source_health = client
            .query_one(
                "SELECT strict_orphan_parents, unknown_parents \
                 FROM source_health WHERE source_id = $1",
                &[&source_id],
            )
            .await?;
        assert_eq!(source_health.get::<_, i64>(0), 0);
        assert_eq!(source_health.get::<_, i64>(1), 1);
        Ok(())
    })
}

#[tokio::test]
async fn refresh_finalizes_a_shallow_boundary_after_a_missed_verification_window() -> Result<()> {
    crate::run_mut_db_test!(client, {
        client
            .execute("DELETE FROM bitcoin_core_header", &[])
            .await?;
        let first = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(2030)
            .with_canonical_header(core_header(0, 0, 1, 0x1d00_ffff))
            .with_canonical_header(core_header(2016, 1, 2, 0x1c00_ffff))
            .with_canonical_header(core_header(2030, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &first).await?;

        let reorged = ConfiguredParentClassifier::Fake(
            FakeParentClassifier::new(ParentClassification::unknown(
                &bitcoin::blockdata::constants::genesis_block(bitcoin::Network::Bitcoin).header,
            ))
            .with_synced_tip_height(4032)
            .with_canonical_header(core_header(2016, 9, 4, 0x1c00_ffff))
            .with_canonical_header(core_header(4032, 2, 3, 0x1c00_ffff)),
        );
        refresh_bitcoin_core_header_cache(&mut client, &reorged).await?;
        let row = client
            .query_one(
                "SELECT block_hash, is_final FROM bitcoin_core_header WHERE height = 2016",
                &[],
            )
            .await?;
        assert_eq!(row.get::<_, Vec<u8>>(0), vec![9; 32]);
        assert!(row.get::<_, bool>(1));
        Ok(())
    })
}
