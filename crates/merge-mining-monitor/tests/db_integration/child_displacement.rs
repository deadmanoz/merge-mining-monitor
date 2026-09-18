use std::time::Duration;

use anyhow::Result;
use mmm_capture::capture::MergeMiningEventPayload;
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::{
    ChildChainHeadOutcome, ChildChainHeadRecord, ChildDisplacementOutcome, CurrentBlockParent,
    EventWriteDisposition, EvidenceMarker, get_source_id, lock_child_chain_height,
    record_child_chain_block, upsert_merge_mining_event,
};
use tokio_postgres::Client;

use crate::support::db::connect_to_schema;
use crate::support::exact_observation;

const HEIGHT: i32 = 1_030;
const HASH_A: [u8; 32] = [0x0a; 32];
const HASH_B: [u8; 32] = [0x0b; 32];
const HASH_C: [u8; 32] = [0x0c; 32];

/// An exact observation at `HEIGHT` with a synthetic child hash.
fn exact_at_height(fixture: &str, hash: [u8; 32]) -> Result<MergeMiningEventPayload> {
    exact_observation(fixture, HEIGHT, hash, 2_030)
}

/// Record the chain's block in its own committed transaction, as a producer
/// with no event to write would.
async fn record(
    client: &mut Client,
    source_id: i64,
    hash: &[u8],
    parent: CurrentBlockParent<'_>,
    observed_at: i64,
) -> Result<ChildDisplacementOutcome> {
    let txn = client.transaction().await?;
    let outcome = record_child_chain_block(
        &txn,
        source_id,
        HEIGHT,
        ChildChainHeadRecord {
            block_hash: hash,
            parent,
            outcome: ChildChainHeadOutcome::Captured,
            evidence: EvidenceMarker::None,
            observed_at,
        },
    )
    .await?;
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
        let outcome = record(
            &mut client,
            source_id,
            &HASH_A,
            CurrentBlockParent::Unknown,
            3_001,
        )
        .await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());

        // The chain replaces A with B: A is displaced by B, B is current.
        upsert_merge_mining_event(&client, source_id, &b).await?;
        let outcome = record(
            &mut client,
            source_id,
            &HASH_B,
            CurrentBlockParent::Unknown,
            3_002,
        )
        .await?;
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
        let outcome = record(
            &mut client,
            source_id,
            &HASH_B,
            CurrentBlockParent::Unknown,
            3_003,
        )
        .await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        assert_eq!(rows_at_height(&client, source_id).await?[0].1, Some(3_002));

        // The chain flips back to A: A is restored, B is displaced by A.
        let outcome = record(
            &mut client,
            source_id,
            &HASH_A,
            CurrentBlockParent::Unknown,
            3_004,
        )
        .await?;
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

/// Seed A (an exact observation) and a hashless observation of a different
/// block (a different parent) at `HEIGHT`, then record A as the chain's block,
/// which displaces the hashless row. Returns A's parent and the hashless row's
/// parent.
async fn seed_a_beside_a_hashless_sibling(
    client: &mut Client,
    source_id: i64,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let a = exact_at_height("500000-valid-parent", HASH_A)?;
    let mut hashless = exact_at_height("500001-near-parent", HASH_C)?;
    hashless.child_block_hash = None;
    hashless.child_block_time = None;
    upsert_merge_mining_event(&*client, source_id, &a).await?;
    upsert_merge_mining_event(&*client, source_id, &hashless).await?;
    let parent_a = a.btc_parent_header_hash.clone();
    let outcome = record(
        client,
        source_id,
        &HASH_A,
        CurrentBlockParent::Known(parent_a.as_slice()),
        3_001,
    )
    .await?;
    assert_eq!(outcome.displaced, 1);
    Ok((parent_a, hashless.btc_parent_header_hash.clone()))
}

#[tokio::test]
async fn hashless_and_revoked_rows_follow_the_chain() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let (parent_a, _) = seed_a_beside_a_hashless_sibling(&mut client, source_id).await?;

        // A is current: the hashless row's parent differs from A's, so it is
        // displaced by A.
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
        let mut b = exact_at_height("500000-valid-parent", HASH_A)?;
        b.child_block_hash = Some(HASH_B.to_vec());
        upsert_merge_mining_event(&client, source_id, &b).await?;
        let outcome = record(
            &mut client,
            source_id,
            &HASH_B,
            CurrentBlockParent::Known(parent_a.as_slice()),
            3_003,
        )
        .await?;
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
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn eventless_blocks_and_a_hashless_restore_follow_the_chain() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let (_, hashless_parent) = seed_a_beside_a_hashless_sibling(&mut client, source_id).await?;

        // The chain moves to a block with no AuxPoW: A is displaced and the
        // height has no current event at all. A second such block changes
        // nothing: the columns record the first displacement.
        let outcome = record(
            &mut client,
            source_id,
            &HASH_C,
            CurrentBlockParent::NoAuxpow,
            3_004,
        )
        .await?;
        assert_eq!(outcome.displaced, 1);
        let rows = rows_at_height(&client, source_id).await?;
        assert!(rows.iter().all(|row| row.1.is_some()));
        assert_eq!(rows[1].2, Some(HASH_C.to_vec()));
        let outcome = record(
            &mut client,
            source_id,
            &[0x0d; 32],
            CurrentBlockParent::NoAuxpow,
            3_005,
        )
        .await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        assert_eq!(rows_at_height(&client, source_id).await?, rows);

        // A hashless row is the current block when the recorded block's parent is
        // its parent (the identity partial promotion uses): recording the block it
        // represents restores it instead of displacing it by itself.
        let outcome = record(
            &mut client,
            source_id,
            &[0x0d; 32],
            CurrentBlockParent::Known(hashless_parent.as_slice()),
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
        let error = record(
            &mut client,
            source_id,
            &[0x0e; 31],
            CurrentBlockParent::Unknown,
            3_006,
        )
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
        record_child_chain_block(
            &first,
            source_id,
            HEIGHT,
            ChildChainHeadRecord {
                block_hash: &HASH_A,
                parent: CurrentBlockParent::Unknown,
                outcome: ChildChainHeadOutcome::Unverified,
                evidence: EvidenceMarker::None,
                observed_at: 3_001,
            },
        )
        .await?;

        // A second capture of a different block at the same height contends on
        // the height lock before it touches any event row, so it cannot
        // deadlock against the first and must wait for its commit.
        let mut other = connect_to_schema(&schema).await?;
        let mut second = tokio::spawn(async move {
            let txn = other.transaction().await?;
            lock_child_chain_height(&txn, source_id, HEIGHT).await?;
            upsert_merge_mining_event(&txn, source_id, &b).await?;
            let outcome = record_child_chain_block(
                &txn,
                source_id,
                HEIGHT,
                ChildChainHeadRecord {
                    block_hash: &HASH_B,
                    parent: CurrentBlockParent::Unknown,
                    outcome: ChildChainHeadOutcome::Unverified,
                    evidence: EvidenceMarker::None,
                    observed_at: 3_002,
                },
            )
            .await?;
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
async fn a_parentless_record_leaves_a_hashless_row_alone() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        const HASH_X: [u8; 32] = [0x1f; 32];
        // A hashless historical observation, then a record of block X with no
        // parent to name (X carries no AuxPoW, or its proof did not verify).
        // The record cannot tell whether the hashless row observed X, so it
        // leaves the row untouched rather than displacing it by X.
        let mut hashless = exact_at_height("500001-near-parent", HASH_X)?;
        hashless.child_block_hash = None;
        hashless.child_block_time = None;
        upsert_merge_mining_event(&client, source_id, &hashless).await?;
        let outcome = record(
            &mut client,
            source_id,
            &HASH_X,
            CurrentBlockParent::Unknown,
            3_001,
        )
        .await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![(None, None, None, None)]
        );

        // The proof arrives and promotes the row to X's exact identity; it is
        // current, and recording X with its parent changes nothing.
        let exact = exact_at_height("500001-near-parent", HASH_X)?;
        let outcome = upsert_merge_mining_event(&client, source_id, &exact).await?;
        assert_eq!(outcome.disposition, EventWriteDisposition::Promoted);
        let outcome = record(
            &mut client,
            source_id,
            &HASH_X,
            CurrentBlockParent::Known(exact.btc_parent_header_hash.as_slice()),
            3_002,
        )
        .await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![(Some(HASH_X.to_vec()), None, None, None)]
        );
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn a_block_with_no_auxpow_displaces_hashless_rows() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        const HASH_N: [u8; 32] = [0x2e; 32];
        // A hashless AuxPoW observation cannot be a block the node confirms
        // carries no AuxPoW, so recording such a block displaces it.
        let mut hashless = exact_at_height("500001-near-parent", HASH_C)?;
        hashless.child_block_hash = None;
        hashless.child_block_time = None;
        upsert_merge_mining_event(&client, source_id, &hashless).await?;
        let outcome = record(
            &mut client,
            source_id,
            &HASH_N,
            CurrentBlockParent::NoAuxpow,
            3_001,
        )
        .await?;
        assert_eq!(
            outcome,
            ChildDisplacementOutcome {
                restored: 0,
                displaced: 1
            }
        );
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![(None, Some(3_001), Some(HASH_N.to_vec()), None)]
        );
        Ok::<_, anyhow::Error>(())
    })
}
