use anyhow::Result;
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::{
    ConfiguredParentClassifier, CoreHeader, FakeParentClassifier, ParentClassification,
};
use mmm_capture::nbits_table::NbitsLookup;
use mmm_producers::refresh_bitcoin_core_header_cache;
use mmm_store::{
    BitcoinCoreHeader, load_bitcoin_core_nbits_table, record_bitcoin_core_header,
    replace_bitcoin_core_header_horizon,
};

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
        replace_bitcoin_core_header_horizon(&mut client, &header(2020, 2, 3, 0x1c00_ffff)).await?;
        replace_bitcoin_core_header_horizon(&mut client, &header(2030, 3, 4, 0x1c00_ffff)).await?;

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
async fn refresh_reads_epoch_boundaries_and_a_confirmed_horizon_from_core() -> Result<()> {
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
            .with_canonical_header(core_header(2024, 2, 3, 0x1c00_ffff)),
        );

        let table = refresh_bitcoin_core_header_cache(&mut client, &classifier).await?;
        assert_eq!(table.horizon_height(), 2024);
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
        assert_eq!(heights, [0, 2016, 2024]);
        Ok(())
    })
}
