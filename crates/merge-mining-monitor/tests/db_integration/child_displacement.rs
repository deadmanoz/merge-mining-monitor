use anyhow::Result;
use mmm_capture::capture::{
    ClassificationProof, MergeMiningEventPayload, ResolvedPoolAttributions, build_event_payload,
};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::{
    ChildDisplacementOutcome, get_source_id, record_child_chain_block, upsert_merge_mining_event,
};
use tokio_postgres::Client;

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
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let a = exact_at_height("500000-valid-parent", HASH_A)?;
        let mut b = a.clone();
        b.child_block_hash = Some(HASH_B.to_vec());

        // A is captured and is the chain's block: nothing to displace.
        upsert_merge_mining_event(&client, source_id, &a).await?;
        let outcome = record_child_chain_block(&client, source_id, HEIGHT, &HASH_A, 3_001).await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());

        // The chain replaces A with B: A is displaced by B, B is current.
        upsert_merge_mining_event(&client, source_id, &b).await?;
        let outcome = record_child_chain_block(&client, source_id, HEIGHT, &HASH_B, 3_002).await?;
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
        let outcome = record_child_chain_block(&client, source_id, HEIGHT, &HASH_B, 3_003).await?;
        assert_eq!(outcome, ChildDisplacementOutcome::default());
        assert_eq!(rows_at_height(&client, source_id).await?[0].1, Some(3_002));

        // The chain flips back to A: A is restored, B is displaced by A.
        let outcome = record_child_chain_block(&client, source_id, HEIGHT, &HASH_A, 3_004).await?;
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
    crate::run_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let a = exact_at_height("500000-valid-parent", HASH_A)?;
        let mut b = a.clone();
        b.child_block_hash = Some(HASH_B.to_vec());
        // A hashless observation of a different block (a different parent)
        // at the same height.
        let mut hashless = exact_at_height("500001-near-parent", HASH_C)?;
        hashless.child_block_hash = None;
        hashless.child_block_time = None;

        upsert_merge_mining_event(&client, source_id, &a).await?;
        upsert_merge_mining_event(&client, source_id, &hashless).await?;

        // A is current: the hashless row belongs to another block, so it is
        // displaced by A.
        let outcome = record_child_chain_block(&client, source_id, HEIGHT, &HASH_A, 3_001).await?;
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
        let outcome = record_child_chain_block(&client, source_id, HEIGHT, &HASH_B, 3_003).await?;
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
        // and the height has no current event at all.
        let outcome = record_child_chain_block(&client, source_id, HEIGHT, &HASH_C, 3_004).await?;
        assert_eq!(outcome.displaced, 1);
        let rows = rows_at_height(&client, source_id).await?;
        assert!(rows.iter().all(|row| row.1.is_some()));
        assert_eq!(rows[2].2, Some(HASH_C.to_vec()));

        // A hash of the wrong length is refused before any write.
        let error = record_child_chain_block(&client, source_id, HEIGHT, &[0x0d; 31], 3_005)
            .await
            .expect_err("a 31-byte hash must be refused");
        assert!(error.to_string().contains("32 bytes"));
        Ok::<_, anyhow::Error>(())
    })
}
