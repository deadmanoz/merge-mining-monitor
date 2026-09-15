use std::time::Duration;

use anyhow::Result;
use mmm_capture::capture::{
    ClassificationProof, MergeMiningEventPayload, ResolvedPoolAttributions, build_event_payload,
};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::{
    ChildDisplacementOutcome, EventWriteDisposition, get_source_id, lock_child_chain_height,
    record_child_chain_block, upsert_merge_mining_event,
};
use tokio_postgres::Client;

use crate::support::db::connect_to_schema;
use crate::support::parse_auxpow_fixture;

const HEIGHT: i32 = 1_030;
const HASH_A: [u8; 32] = [0x0a; 32];
const HASH_B: [u8; 32] = [0x0b; 32];
const HASH_C: [u8; 32] = [0x0c; 32];

/// An exact observation at `HEIGHT` with a synthetic child hash. The child
/// header is dropped so the hash need not authenticate against it.
fn exact_at_height(fixture: &str, hash: [u8; 32]) -> Result<MergeMiningEventPayload> {
    let parsed = parse_auxpow_fixture(fixture)?;
    let mut payload = build_event_payload(
        &parsed,
        Some(HEIGHT),
        ResolvedPoolAttributions::default(),
        ClassificationProof::default(),
        2_030,
    )?;
    payload.child_block_hash = Some(hash.to_vec());
    payload.child_header_bytes = None;
    payload.child_nbits = None;
    payload.pow_validates_child_target = None;
    Ok(payload)
}

/// Record the chain's block in its own committed transaction, as a producer
/// with no event to write would.
async fn record(
    client: &mut Client,
    source_id: i64,
    hash: &[u8],
    parent: Option<&[u8]>,
    observed_at: i64,
) -> Result<ChildDisplacementOutcome> {
    let txn = client.transaction().await?;
    let outcome =
        record_child_chain_block(&txn, source_id, HEIGHT, hash, parent, observed_at).await?;
    txn.commit().await?;
    Ok(outcome)
}

/// `(child_block_hash, child_displaced_at, child_displaced_by, revoked_at)` for
/// every event at `HEIGHT`, ordered by hash with the hashless row first.
async fn rows_at_height(
    client: &Client,
    source_id: i64,
) -> Result<Vec<(Option<Vec<u8>>, Option<i64>, Option<Vec<u8>>, Option<i64>)>> {
    let rows = client
        .query(
            "SELECT child_block_hash, child_displaced_at, child_displaced_by, revoked_at \
             FROM merge_mining_event \
             WHERE source_id = $1 AND child_height = $2 \
             ORDER BY child_block_hash NULLS FIRST",
            &[&source_id, &HEIGHT],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
        .collect())
}

#[tokio::test]
async fn flip_flop_keeps_exactly_one_current_block_and_revokes_nothing() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let a = exact_at_height("500000-valid-parent", HASH_A)?;
        let mut b = a.clone();
        b.child_block_hash = Some(HASH_B.to_vec());

        // A is captured and is the chain's block: nothing to displace.
        upsert_merge_mining_event(&client, source_id, &a).await?;
        let outcome = record(&mut client, source_id, &HASH_A, None, 3_001).await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());

        // The chain replaces A with B: A is displaced by B, B is current.
        upsert_merge_mining_event(&client, source_id, &b).await?;
        let outcome = record(&mut client, source_id, &HASH_B, None, 3_002).await?;
        assert_eq!(
            outcome,
            ChildDisplacementOutcome {
                restored: 0,
                displaced: 1
            }
        );
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![
                (
                    Some(HASH_A.to_vec()),
                    Some(3_002),
                    Some(HASH_B.to_vec()),
                    None
                ),
                (Some(HASH_B.to_vec()), None, None, None),
            ]
        );

        // Seeing B again changes nothing, and A keeps its first record.
        let outcome = record(&mut client, source_id, &HASH_B, None, 3_003).await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        assert_eq!(rows_at_height(&client, source_id).await?[0].1, Some(3_002));

        // The chain flips back to A: A is restored, B is displaced by A.
        let outcome = record(&mut client, source_id, &HASH_A, None, 3_004).await?;
        assert_eq!(
            outcome,
            ChildDisplacementOutcome {
                restored: 1,
                displaced: 1
            }
        );
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![
                (Some(HASH_A.to_vec()), None, None, None),
                (
                    Some(HASH_B.to_vec()),
                    Some(3_004),
                    Some(HASH_A.to_vec()),
                    None
                ),
            ]
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn hashless_revoked_and_eventless_blocks_follow_the_chain() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let a = exact_at_height("500000-valid-parent", HASH_A)?;
        let mut b = a.clone();
        b.child_block_hash = Some(HASH_B.to_vec());
        // A hashless observation of a different block (a different parent)
        // at the same height.
        let mut hashless = exact_at_height("500001-near-parent", HASH_C)?;
        hashless.child_block_hash = None;
        hashless.child_block_time = None;
        let hashless_parent = hashless.btc_parent_header_hash.clone();

        upsert_merge_mining_event(&client, source_id, &a).await?;
        upsert_merge_mining_event(&client, source_id, &hashless).await?;

        // A is current: the hashless row belongs to another block, so it is
        // displaced by A.
        let outcome = record(&mut client, source_id, &HASH_A, None, 3_001).await?;
        assert_eq!(outcome.displaced, 1);
        let rows = rows_at_height(&client, source_id).await?;
        assert_eq!(rows[0].0, None);
        assert_eq!(rows[0].2, Some(HASH_A.to_vec()));
        assert_eq!(rows[1].0, Some(HASH_A.to_vec()));
        assert_eq!(rows[1].1, None);

        // Revocation is an independent axis: a revoked A is still displaced
        // when the chain moves to B, and the hashless row keeps its record.
        client
            .execute(
                "UPDATE merge_mining_event SET revoked_at = 3_002, revocation_reason = 'test' \
                 WHERE source_id = $1 AND child_height = $2 AND child_block_hash = $3",
                &[&source_id, &HEIGHT, &HASH_A.as_slice()],
            )
            .await?;
        upsert_merge_mining_event(&client, source_id, &b).await?;
        let outcome = record(&mut client, source_id, &HASH_B, None, 3_003).await?;
        assert_eq!(outcome.displaced, 1);
        let rows = rows_at_height(&client, source_id).await?;
        assert_eq!(rows[0].2, Some(HASH_A.to_vec()));
        assert_eq!(
            rows[1],
            (
                Some(HASH_A.to_vec()),
                Some(3_003),
                Some(HASH_B.to_vec()),
                Some(3_002)
            )
        );
        assert_eq!(rows[2], (Some(HASH_B.to_vec()), None, None, None));

        // The chain moves to a block with no event (no AuxPoW): B is displaced
        // and the height has no current event at all. A second such block
        // changes nothing: the columns record the first displacement.
        let outcome = record(&mut client, source_id, &HASH_C, None, 3_004).await?;
        assert_eq!(outcome.displaced, 1);
        let rows = rows_at_height(&client, source_id).await?;
        assert!(rows.iter().all(|row| row.1.is_some()));
        assert_eq!(rows[2].2, Some(HASH_C.to_vec()));
        let outcome = record(&mut client, source_id, &[0x0d; 32], None, 3_005).await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        assert_eq!(rows_at_height(&client, source_id).await?, rows);

        // A hashless row is the current block when the recorded block's parent is
        // its parent (the identity partial promotion uses): recording the block it
        // represents restores it instead of displacing it by itself.
        let outcome = record(
            &mut client,
            source_id,
            &[0x0d; 32],
            Some(hashless_parent.as_slice()),
            3_005,
        )
        .await?;
        assert_eq!(
            outcome,
            ChildDisplacementOutcome {
                restored: 1,
                displaced: 0
            }
        );
        let rows = rows_at_height(&client, source_id).await?;
        assert_eq!(rows[0].0, None);
        assert_eq!(rows[0].1, None);

        // A hash of the wrong length is refused before any write.
        let error = record(&mut client, source_id, &[0x0e; 31], None, 3_006)
            .await
            .expect_err("a 31-byte hash must be refused");
        assert!(error.to_string().contains("32 bytes"));
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn concurrent_captures_at_one_height_serialize_on_the_lock_and_the_later_commit_wins()
-> Result<()> {
    crate::run_mut_db_test!(client, schema, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let a = exact_at_height("500000-valid-parent", HASH_A)?;
        let mut b = a.clone();
        b.child_block_hash = Some(HASH_B.to_vec());

        // The first capture follows the producer sequence (lock, upsert,
        // record) and holds its transaction open.
        let first = client.transaction().await?;
        lock_child_chain_height(&first, source_id, HEIGHT).await?;
        upsert_merge_mining_event(&first, source_id, &a).await?;
        record_child_chain_block(&first, source_id, HEIGHT, &HASH_A, None, 3_001).await?;

        // A second capture of a different block at the same height contends on
        // the height lock before it touches any event row, so it cannot
        // deadlock against the first and must wait for its commit.
        let mut other = connect_to_schema(&schema).await?;
        let mut second = tokio::spawn(async move {
            let txn = other.transaction().await?;
            lock_child_chain_height(&txn, source_id, HEIGHT).await?;
            upsert_merge_mining_event(&txn, source_id, &b).await?;
            let outcome =
                record_child_chain_block(&txn, source_id, HEIGHT, &HASH_B, None, 3_002).await?;
            txn.commit().await?;
            Ok::<_, anyhow::Error>(outcome)
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut second)
                .await
                .is_err(),
            "the second capture must block on the height lock"
        );

        first.commit().await?;
        let outcome = second.await??;
        assert_eq!(
            outcome,
            ChildDisplacementOutcome {
                restored: 0,
                displaced: 1
            }
        );
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![
                (
                    Some(HASH_A.to_vec()),
                    Some(3_002),
                    Some(HASH_B.to_vec()),
                    None
                ),
                (Some(HASH_B.to_vec()), None, None, None),
            ]
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn promotion_clears_a_self_displacement_left_by_a_parentless_record() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        const HASH_X: [u8; 32] = [0x1f; 32];
        // A hashless historical observation of block X, then an eventless
        // record of X made before its proof was seen: with no parent to name,
        // the record displaces the hashless row by X itself.
        let mut hashless = exact_at_height("500001-near-parent", HASH_X)?;
        hashless.child_block_hash = None;
        hashless.child_block_time = None;
        upsert_merge_mining_event(&client, source_id, &hashless).await?;
        record(&mut client, source_id, &HASH_X, None, 3_001).await?;
        let rows = rows_at_height(&client, source_id).await?;
        assert_eq!(rows[0].0, None);
        assert_eq!(rows[0].2, Some(HASH_X.to_vec()));

        // The proof arrives: the exact observation promotes the hashless row to
        // X's identity, which clears the self-displacement instead of tripping
        // the not-self constraint, and the row is current.
        let exact = exact_at_height("500001-near-parent", HASH_X)?;
        let outcome = upsert_merge_mining_event(&client, source_id, &exact).await?;
        assert_eq!(outcome.disposition, EventWriteDisposition::Promoted);
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![(Some(HASH_X.to_vec()), None, None, None)]
        );
        let outcome = record(
            &mut client,
            source_id,
            &HASH_X,
            Some(exact.btc_parent_header_hash.as_slice()),
            3_002,
        )
        .await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        Ok::<_, anyhow::Error>(())
    })
}
