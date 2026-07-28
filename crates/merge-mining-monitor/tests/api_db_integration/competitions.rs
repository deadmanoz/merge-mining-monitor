use anyhow::Result;
use mmm_api::projection::{self, CompetitionsPayload};
use mmm_capture::source_registry::{BITCOIN_SOURCE_CODE, NAMECOIN_SOURCE_CODE, RSK_SOURCE_CODE};
use mmm_store::get_source_id;
use tokio_postgres::Client;

use crate::helpers::format_projection_error;
use crate::support::seed::{display_hash, hash_bytes, insert_attestation_proof, insert_block};

const BASE_TS: i64 = 1_700_000_000;

async fn fetch(client: &Client) -> Result<CompetitionsPayload> {
    projection::competitions(client)
        .await
        .map_err(format_projection_error)
}

/// A canonical block plus a stale that lost to it. Returns both hashes.
async fn seed_competition(
    client: &Client,
    canonical_n: u32,
    stale_n: u32,
    height: i32,
    canonical_ts: i64,
    stale_ts: i64,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let prev = hash_bytes(canonical_n + 1000);
    let canonical = hash_bytes(canonical_n);
    let stale = hash_bytes(stale_n);
    insert_block(
        client,
        &canonical,
        &prev,
        Some(height),
        "canonical",
        canonical_ts,
        None,
    )
    .await?;
    insert_block(
        client,
        &stale,
        &prev,
        Some(height),
        "stale",
        stale_ts,
        Some(&canonical),
    )
    .await?;
    Ok((canonical, stale))
}

#[tokio::test]
async fn competitions_require_a_same_height_canonical_competitor() -> Result<()> {
    crate::run_db_test!(client, {
        // Included: a stale whose competitor really is canonical.
        let (_, stale) = seed_competition(&client, 1, 2, 800_000, BASE_TS + 25, BASE_TS).await?;

        // Excluded: a stale whose competitor row exists but is not canonical.
        // The plan's original "competitor absent" case is unrepresentable -
        // chk_block_kind_height forces every stale to carry a non-null
        // competitor and the column is a foreign key - so the reachable
        // exclusion is a competitor of the wrong kind.
        let unknown = hash_bytes(3);
        insert_block(
            &client,
            &unknown,
            &hash_bytes(1003),
            None,
            "unknown",
            BASE_TS,
            None,
        )
        .await?;
        let orphaned_stale = hash_bytes(4);
        insert_block(
            &client,
            &orphaned_stale,
            &hash_bytes(1003),
            Some(800_001),
            "stale",
            BASE_TS,
            Some(&unknown),
        )
        .await?;

        // Excluded: a stale whose competitor IS canonical but sits at another
        // height. Nothing in the schema forbids it (the column is a plain FK
        // with a not-self check), only the classifier's same-height rule, and
        // two blocks at different heights never raced.
        let far_canonical = hash_bytes(5);
        insert_block(
            &client,
            &far_canonical,
            &hash_bytes(1005),
            Some(800_050),
            "canonical",
            BASE_TS,
            None,
        )
        .await?;
        insert_block(
            &client,
            &hash_bytes(6),
            &hash_bytes(1005),
            Some(800_002),
            "stale",
            BASE_TS,
            Some(&far_canonical),
        )
        .await?;

        let payload = fetch(&client).await?;
        let hashes = payload
            .competitions
            .iter()
            .map(|row| row.stale_hash.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            hashes,
            vec![display_hash(&stale)],
            "a competition needs a canonical competitor at the same height"
        );

        let row = &payload.competitions[0];
        assert_eq!(row.btc_height, 800_000);
        assert_eq!(
            row.header_time_delta_s,
            Some(25),
            "delta is canonical minus stale header time"
        );
        assert_eq!(row.stale_header_time, BASE_TS);
        assert!(!row.stale_bitcoin_miner_pool.known);
        Ok(())
    })
}

#[tokio::test]
async fn competitions_order_by_height_then_display_hash() -> Result<()> {
    crate::run_db_test!(client, {
        // Two stales at one height, inserted with the higher display hash
        // first, plus a lower height inserted last.
        let canonical = hash_bytes(10);
        let prev = hash_bytes(1010);
        insert_block(
            &client,
            &canonical,
            &prev,
            Some(900_001),
            "canonical",
            BASE_TS,
            None,
        )
        .await?;
        for n in [22_u32, 21] {
            insert_block(
                &client,
                &hash_bytes(n),
                &prev,
                Some(900_001),
                "stale",
                BASE_TS,
                Some(&canonical),
            )
            .await?;
        }
        let lower_canonical = hash_bytes(11);
        insert_block(
            &client,
            &lower_canonical,
            &prev,
            Some(900_000),
            "canonical",
            BASE_TS,
            None,
        )
        .await?;
        insert_block(
            &client,
            &hash_bytes(30),
            &prev,
            Some(900_000),
            "stale",
            BASE_TS,
            Some(&lower_canonical),
        )
        .await?;

        let payload = fetch(&client).await?;
        let keys = payload
            .competitions
            .iter()
            .map(|row| (row.btc_height, row.stale_hash.clone()))
            .collect::<Vec<_>>();
        let mut expected = keys.clone();
        expected.sort();
        assert_eq!(
            keys, expected,
            "ordering is ascending height then lexicographic display hash"
        );
        assert_eq!(
            keys.iter().map(|key| key.0).collect::<Vec<_>>(),
            vec![900_000, 900_001, 900_001]
        );
        Ok(())
    })
}

#[tokio::test]
async fn competitions_null_the_delta_outside_i32() -> Result<()> {
    crate::run_db_test!(client, {
        // ~4e9 seconds apart, past i32::MAX, so the guard must yield NULL
        // rather than a wrapped number.
        seed_competition(&client, 40, 41, 950_000, 5_000_000_000, 1_000_000_000).await?;

        let payload = fetch(&client).await?;
        assert_eq!(payload.competitions.len(), 1);
        assert_eq!(
            payload.competitions[0].header_time_delta_s, None,
            "an out-of-i32 difference serialises as null, never truncated"
        );
        Ok(())
    })
}

#[tokio::test]
async fn competitions_survive_timestamps_at_opposite_i64_extrema() -> Result<()> {
    crate::run_db_test!(client, {
        // A BIGINT subtraction is evaluated before the range guard can reject
        // it, so i64::MAX - i64::MIN raises `bigint out of range` and fails the
        // whole request rather than nulling one row. Nothing in the schema
        // bounds btc_header_time, so the projection must tolerate it.
        seed_competition(&client, 60, 61, 970_000, i64::MAX, i64::MIN).await?;
        // A normal competition alongside it: the pathological row must not take
        // the rest of the payload down with it.
        seed_competition(&client, 62, 63, 970_001, BASE_TS + 7, BASE_TS).await?;

        let payload = fetch(&client).await?;
        assert_eq!(payload.competitions.len(), 2);
        assert_eq!(
            payload.competitions[0].header_time_delta_s, None,
            "an i64-extrema difference nulls its own row"
        );
        assert_eq!(
            payload.competitions[1].header_time_delta_s,
            Some(7),
            "and leaves every other competition intact"
        );
        Ok(())
    })
}

#[tokio::test]
async fn competitions_sources_match_block_evidence_semantics() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let rsk = get_source_id(&client, RSK_SOURCE_CODE).await?;

        // 1. AuxPoW-only, single source.
        let (_, auxpow_only) =
            seed_competition(&client, 50, 51, 960_000, BASE_TS + 5, BASE_TS).await?;
        insert_attestation_proof(&client, &auxpow_only, namecoin, &[1], BASE_TS).await?;

        // 2. Two live proofs plus one revoked: the revoked source must not
        //    appear, and the survivors must come back sorted and unique.
        let (_, multi) = seed_competition(&client, 52, 53, 960_001, BASE_TS + 5, BASE_TS).await?;
        insert_attestation_proof(&client, &multi, rsk, &[2], BASE_TS).await?;
        insert_attestation_proof(&client, &multi, namecoin, &[3], BASE_TS).await?;

        // 3. Core-attested with no AuxPoW proof at all: the synthetic Bitcoin
        //    source is what the tree and block projections report here, so the
        //    shared Source filter needs it on this payload too.
        let (_, core_only) =
            seed_competition(&client, 54, 55, 960_002, BASE_TS + 5, BASE_TS).await?;
        client
            .execute(
                "UPDATE block SET core_attested = TRUE WHERE btc_header_hash = $1",
                &[&core_only],
            )
            .await?;

        // 4. A stale whose only proof has been revoked, and which is neither
        //    Core-attested nor live-observed: no evidence at all.
        let (_, revoked) = seed_competition(&client, 56, 57, 960_003, BASE_TS + 5, BASE_TS).await?;
        insert_attestation_proof(&client, &revoked, rsk, &[4], BASE_TS).await?;
        client
            .execute(
                "UPDATE attestation_proof SET revoked_at = $2, revocation_reason = 'test' \
                 WHERE btc_header_hash = $1",
                &[&revoked, &BASE_TS],
            )
            .await?;

        let payload = fetch(&client).await?;
        // `expect`, not `unwrap_or_default`: an absent row must fail loudly.
        // Defaulting to an empty vec would let the revoked-proof assertion pass
        // for the wrong reason, by dropping the competition entirely instead of
        // returning it with no sources.
        let sources_for = |hash: &[u8]| {
            let want = display_hash(hash);
            payload
                .competitions
                .iter()
                .find(|row| row.stale_hash == want)
                .unwrap_or_else(|| panic!("competition {want} missing from the payload"))
                .sources
                .clone()
        };

        assert_eq!(sources_for(&auxpow_only), vec![NAMECOIN_SOURCE_CODE]);
        assert_eq!(
            sources_for(&multi),
            vec![NAMECOIN_SOURCE_CODE, RSK_SOURCE_CODE],
            "multiple proof sources come back sorted and unique"
        );
        assert_eq!(
            sources_for(&core_only),
            vec![BITCOIN_SOURCE_CODE],
            "a Core-attested stale carries the synthetic Bitcoin source"
        );
        assert!(
            sources_for(&revoked).is_empty(),
            "a revoked proof contributes no source"
        );
        Ok(())
    })
}
