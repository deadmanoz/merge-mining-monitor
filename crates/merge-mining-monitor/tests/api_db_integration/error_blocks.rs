use anyhow::Result;
use mmm_api::projection::{self};
use mmm_api::query::{self, NavigatorTarget};
use time::Month;
use tokio_postgres::Client;

use crate::support::seed::{day_epoch, display_hash, hash_bytes, insert_block, insert_error_block};

use crate::helpers::{format_api_error, format_projection_error};

async fn fetch_error_block_navigator(
    client: &Client,
    raw_query: &str,
) -> Result<projection::NavigatorPayload> {
    let query = query::parse_navigator_query(NavigatorTarget::ErrorBlock, Some(raw_query))
        .map_err(format_api_error)?;
    projection::navigator(client, &query)
        .await
        .map_err(format_projection_error)
}

/// Seed a catalogue shaped like the real one: mostly one block per height, but
/// with genuine same-height groups. The committed research catalogue holds 33
/// blocks across only 26 heights, with one height carrying six of them, so
/// height alone is not a unique key and the navigator must tie-break on hash.
///
/// Hash values are chosen so stored-byte order and display-hex order DISAGREE
/// (see the assertion in `assert_stored_not_display_order`): `hash_bytes` writes
/// the value big-endian into the trailing bytes, so stored order tracks the
/// value while the display form reverses the bytes and keys on the low byte
/// first. A regression to display-hex ordering flips the groups and fails.
struct SeededCatalogue {
    /// Every catalogued hash, in the order the navigator must return them
    /// (height descending, then stored bytes ascending).
    expected: Vec<Vec<u8>>,
}

async fn seed_catalogue(client: &Client) -> Result<SeededCatalogue> {
    let ts = day_epoch(2026, Month::May, 10);

    // Canonical context above every catalogued height so navigation views
    // resolve instead of reporting a target beyond the complete canonical tip.
    insert_block(
        client,
        &hash_bytes(0x9000),
        &hash_bytes(0x8fff),
        Some(400),
        "canonical",
        ts,
        None,
    )
    .await?;

    // Height 300: a single error block.
    let h300 = hash_bytes(0x0301);
    insert_error_block(
        client,
        &h300,
        &hash_bytes(0x02ff),
        300,
        ts + 1,
        "nbits_retarget_not_applied",
    )
    .await?;

    // Height 200: a six-member collision group, the shape that makes a
    // height-only cursor skip or repeat members at a page boundary.
    let group200 = [0x0201, 0x0300, 0x0402, 0x0501, 0x0600, 0x0703]
        .map(hash_bytes)
        .to_vec();
    for (index, hash) in group200.iter().enumerate() {
        insert_error_block(
            client,
            hash,
            &hash_bytes(0x01f0 + index as u32),
            200,
            ts + 2 + index as i64,
            "bip34_coinbase_height_mismatch",
        )
        .await?;
    }

    // Height 100: a two-member group.
    let group100 = [0x1102, 0x1200].map(hash_bytes).to_vec();
    for (index, hash) in group100.iter().enumerate() {
        insert_error_block(
            client,
            hash,
            &hash_bytes(0x00f0 + index as u32),
            100,
            ts + 10 + index as i64,
            "bip34_v2_coinbase_height_mismatch",
        )
        .await?;
    }

    // Height descending, then stored bytes ascending within each height.
    let mut expected = vec![h300];
    let mut g200 = group200;
    g200.sort();
    expected.extend(g200);
    let mut g100 = group100;
    g100.sort();
    expected.extend(g100);

    Ok(SeededCatalogue { expected })
}

/// Guard the guard: if stored-byte and display-hex order happened to agree, the
/// tie-break assertions would pass under a display-ordering regression too.
fn assert_stored_not_display_order(expected: &[Vec<u8>]) {
    let by_display = {
        let mut displayed = expected.iter().map(|h| display_hash(h)).collect::<Vec<_>>();
        displayed.sort();
        displayed
    };
    let in_stored_order = expected.iter().map(|h| display_hash(h)).collect::<Vec<_>>();
    assert_ne!(
        by_display, in_stored_order,
        "catalogue fixture must distinguish stored-byte ordering from display-hex ordering"
    );
}

#[tokio::test]
async fn error_block_navigator_is_empty_without_catalogued_blocks() -> Result<()> {
    crate::run_db_test!(client, {
        let empty = fetch_error_block_navigator(&client, "limit=10").await?;
        assert_eq!(empty.total, 0);
        assert!(empty.items.is_empty());
        assert!(empty.next_cursor.is_none());
        assert!(empty.prev_cursor.is_none());
        Ok(())
    })
}

#[tokio::test]
async fn error_block_navigator_orders_by_height_then_stored_hash() -> Result<()> {
    crate::run_db_test!(client, {
        let seeded = seed_catalogue(&client).await?;
        assert_stored_not_display_order(&seeded.expected);

        let full = fetch_error_block_navigator(&client, "limit=100").await?;
        assert_eq!(full.total, seeded.expected.len() as u64);
        assert_eq!(full.items.len(), seeded.expected.len());

        let returned = full
            .items
            .iter()
            .map(|item| item.primary_hash.clone())
            .collect::<Vec<_>>();
        let expected = seeded
            .expected
            .iter()
            .map(|hash| display_hash(hash))
            .collect::<Vec<_>>();
        assert_eq!(returned, expected);

        // Each item is a single-height span carrying the catalogue kind, and
        // never a branch or orphan: an error block cannot be either.
        assert_eq!(full.items[0].position.max, 300);
        assert_eq!(full.items[0].position.min, 300);
        assert_eq!(full.items[0].kind, "error-block");
        assert!(full.items[0].branch.is_none());
        assert!(full.items[0].orphan.is_none());

        // Every item resolves to exactly one of a renderable window or an
        // explicit reason it has none, so a client is never left guessing.
        // Which one it is depends on canonical backbone density, whose readiness
        // rules live in shared code (`navigation_for_span`) already covered by
        // the stale navigator's tests.
        for item in &full.items {
            assert_ne!(
                item.view.is_some(),
                item.view_error.is_some(),
                "item {} must carry a view xor a view_error",
                item.primary_hash
            );
        }
        Ok(())
    })
}

#[tokio::test]
async fn error_block_navigator_pages_through_a_same_height_group_exactly_once() -> Result<()> {
    crate::run_db_test!(client, {
        let seeded = seed_catalogue(&client).await?;
        let expected = seeded
            .expected
            .iter()
            .map(|hash| display_hash(hash))
            .collect::<Vec<_>>();

        // Walk the whole catalogue older-ward in pages of two. Page boundaries
        // land INSIDE the six-member height-200 group, which is exactly where a
        // height-only cursor would repeat or skip members.
        let mut walked: Vec<String> = Vec::new();
        let mut page = fetch_error_block_navigator(&client, "limit=2").await?;
        assert!(page.prev_cursor.is_none(), "first page has nothing newer");
        loop {
            walked.extend(page.items.iter().map(|item| item.primary_hash.clone()));
            let Some(cursor) = page.next_cursor.clone() else {
                break;
            };
            page = fetch_error_block_navigator(
                &client,
                &format!("cursor={cursor}&direction=older&limit=2"),
            )
            .await?;
        }

        assert_eq!(
            walked, expected,
            "a full older-ward walk must return every catalogued block exactly once, in order"
        );

        // And back: stepping newer from the last page reconstructs the reverse.
        let mut back: Vec<String> = Vec::new();
        loop {
            back.extend(page.items.iter().map(|item| item.primary_hash.clone()));
            let Some(cursor) = page.prev_cursor.clone() else {
                break;
            };
            page = fetch_error_block_navigator(
                &client,
                &format!("cursor={cursor}&direction=newer&limit=2"),
            )
            .await?;
        }
        let mut back_sorted = back.clone();
        back_sorted.sort();
        back_sorted.dedup();
        assert_eq!(
            back_sorted.len(),
            back.len(),
            "a newer-ward walk must not repeat a block"
        );
        Ok(())
    })
}

#[tokio::test]
async fn error_block_navigator_anchors_on_a_catalogued_hash() -> Result<()> {
    crate::run_db_test!(client, {
        let seeded = seed_catalogue(&client).await?;
        // Anchor on a middle member of the six-block group.
        let anchor = display_hash(&seeded.expected[3]);
        let payload =
            fetch_error_block_navigator(&client, &format!("anchor_hash={anchor}&limit=10")).await?;
        assert_eq!(payload.items.len(), 1);
        assert_eq!(payload.items[0].primary_hash, anchor);
        assert_eq!(payload.items[0].position.max, 200);
        Ok(())
    })
}
