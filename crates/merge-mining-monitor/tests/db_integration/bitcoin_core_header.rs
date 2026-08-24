use anyhow::Result;
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, CoreHeader, FakeParentClassifier, ParentClassification,
};
use mmm_capture::nbits_table::NbitsLookup;
use mmm_producers::refresh_bitcoin_core_header_cache;
use mmm_store::{
    BitcoinCoreHeader, load_bitcoin_core_nbits_table, load_bitcoin_core_nbits_table_if_present,
    lock_bitcoin_core_header_cache, record_bitcoin_core_header, replace_bitcoin_core_header_cache,
    unlock_bitcoin_core_header_cache,
};

use crate::support::db::connect_to_schema;

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
        )
        .await?;
        replace_bitcoin_core_header_cache(
            &mut client,
            2016,
            &[],
            None,
            &header(2030, 3, 4, 0x1c00_ffff),
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
            unlock_bitcoin_core_header_cache(&waiting_client).await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut waiter)
                .await
                .is_err(),
            "a second Core-cache refresh must wait for the current observation"
        );

        unlock_bitcoin_core_header_cache(&client).await?;
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
