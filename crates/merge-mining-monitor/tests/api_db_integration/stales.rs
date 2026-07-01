use anyhow::Result;
use mmm_api::projection::{self};
use mmm_api::query::{self, NavigatorTarget};
use time::Month;
use tokio_postgres::Client;

use crate::support::seed::{day_epoch, display_hash, hash_bytes, insert_block};

use crate::helpers::{format_api_error, format_projection_error};

async fn insert_canonical_competitor(
    client: &Client,
    hash: &[u8],
    prev_hash: &[u8],
    height: i32,
    ts: i64,
    complete: bool,
) -> Result<()> {
    insert_block(client, hash, prev_hash, Some(height), "canonical", ts, None).await?;
    if !complete {
        client
            .execute(
                "UPDATE block SET btc_coinbase_status = 'not_attempted' \
                 WHERE btc_header_hash = $1",
                &[&hash],
            )
            .await?;
    }
    Ok(())
}

async fn insert_stale_competitor(
    client: &Client,
    stale_hash: &[u8],
    prev_hash: &[u8],
    height: i32,
    ts: i64,
    canonical_hash: &[u8],
) -> Result<()> {
    insert_stale_block(client, stale_hash, prev_hash, height, ts, canonical_hash).await
}

async fn insert_stale_block(
    client: &Client,
    stale_hash: &[u8],
    prev_hash: &[u8],
    height: i32,
    ts: i64,
    canonical_hash: &[u8],
) -> Result<()> {
    let canonical_hash_vec = canonical_hash.to_vec();
    insert_block(
        client,
        stale_hash,
        prev_hash,
        Some(height),
        "stale",
        ts,
        Some(&canonical_hash_vec),
    )
    .await
}

async fn fetch_stale_navigator(
    client: &Client,
    raw_query: &str,
) -> Result<projection::NavigatorPayload> {
    let query = query::parse_navigator_query(NavigatorTarget::Stale, Some(raw_query))
        .map_err(format_api_error)?;
    projection::navigator(client, &query)
        .await
        .map_err(format_projection_error)
}

fn assert_stale_navigation_contract(payload: &projection::NavigatorPayload, s1: &[u8]) {
    assert_eq!(payload.total, 4);
    assert_eq!(payload.items.len(), 4);
    assert_eq!(payload.items[0].position.max, 250);
    assert_eq!(payload.items[1].position.max, 200);
    assert_eq!(payload.items[2].position.max, 200);
    assert_eq!(payload.items[3].position.max, 100);
    assert_eq!(payload.items[3].primary_hash, display_hash(s1));
    assert!(payload.items[0].view.is_none());
    let nav_error = payload.items[0]
        .view_error
        .as_ref()
        .expect("height above max complete carries navigation error");
    assert_eq!(nav_error.code, "target_backbone_unsynced");
    assert_eq!(nav_error.target_height, 250);

    let projection::NavigatorView::TreeWindow {
        target_height,
        tree_from,
        tree_to,
        select_hash,
        center_hash,
    } = payload.items[1]
        .view
        .as_ref()
        .expect("height 200 stale has navigation")
    else {
        panic!("expected tree_window view");
    };
    assert_eq!(*target_height, 200);
    assert_eq!(*tree_from, 200);
    assert_eq!(*tree_to, 200);
    assert_eq!(select_hash, &payload.items[1].primary_hash);
    assert_eq!(center_hash, &payload.items[1].primary_hash);
}

fn assert_same_height_stored_tiebreak(
    payload: &projection::NavigatorPayload,
    s2a: &[u8],
    s2b: &[u8],
) {
    let mut stored_order = [s2a.to_vec(), s2b.to_vec()];
    stored_order.sort();
    let expected_by_stored = [
        display_hash(&stored_order[0]),
        display_hash(&stored_order[1]),
    ];
    let mut display_order = [display_hash(s2a), display_hash(s2b)];
    display_order.sort();
    assert_ne!(
        expected_by_stored, display_order,
        "tie-break test hashes must sort differently by stored bytes vs displayed hex"
    );
    assert_eq!(
        [
            payload.items[1].primary_hash.clone(),
            payload.items[2].primary_hash.clone(),
        ],
        expected_by_stored,
    );
}

async fn assert_stale_navigator_keyset_tiebreak(
    client: &Client,
    full: &projection::NavigatorPayload,
) -> Result<()> {
    let latest = fetch_stale_navigator(client, "limit=2").await?;
    assert_eq!(latest.total, full.total);
    assert_eq!(latest.items.len(), 2);
    assert_eq!(latest.items[0].primary_hash, full.items[0].primary_hash);
    assert_eq!(latest.items[1].primary_hash, full.items[1].primary_hash);
    assert!(latest.prev_cursor.is_none());
    let older = latest.next_cursor.expect("latest page has older cursor");

    let next =
        fetch_stale_navigator(client, &format!("cursor={older}&direction=older&limit=2")).await?;
    assert_eq!(next.items.len(), 2);
    assert_eq!(next.items[0].primary_hash, full.items[2].primary_hash);
    assert_eq!(next.items[1].primary_hash, full.items[3].primary_hash);
    assert!(next.next_cursor.is_none());
    assert!(next.prev_cursor.is_some());

    let newer = next.prev_cursor.expect("older page has newer cursor");
    let previous =
        fetch_stale_navigator(client, &format!("cursor={newer}&direction=newer&limit=2")).await?;
    assert_eq!(previous.items.len(), 2);
    assert_eq!(previous.items[0].primary_hash, full.items[0].primary_hash);
    assert_eq!(previous.items[1].primary_hash, full.items[1].primary_hash);
    assert!(previous.next_cursor.is_some());
    assert!(previous.prev_cursor.is_none());

    Ok(())
}

#[tokio::test]
async fn stale_navigator_lists_stales_newest_first_with_stored_byte_tiebreak() -> Result<()> {
    crate::run_db_test!(client, {
        // Empty DB: no stale rows.
        let empty = fetch_stale_navigator(&client, "limit=10").await?;
        assert_eq!(empty.total, 0);
        assert!(empty.items.is_empty());

        let ts = day_epoch(2026, Month::May, 10);

        let c1 = hash_bytes(0x0100);
        let s1 = hash_bytes(0x1100);
        insert_canonical_competitor(&client, &c1, &hash_bytes(0x00ff), 100, ts, true).await?;
        insert_stale_competitor(&client, &s1, &hash_bytes(0x00fe), 100, ts + 1, &c1).await?;

        let c2 = hash_bytes(0x0200);
        // Chosen so the two same-height stales sort DIFFERENTLY by stored bytes
        // vs displayed hex: hash_bytes() puts the value big-endian in the trailing
        // bytes, so stored-byte order tracks the value (0x0201 < 0x0300) while the
        // display form reverses bytes and keys on the low byte (0x0300 -> "0003..."
        // sorts before 0x0201 -> "0102..."). A regression to display-hex ordering
        // would flip the pair and fail the tie-break assertion below.
        let s2a = hash_bytes(0x0201);
        let s2b = hash_bytes(0x0300);
        insert_canonical_competitor(&client, &c2, &hash_bytes(0x01ff), 200, ts + 2, true).await?;
        insert_stale_competitor(&client, &s2a, &hash_bytes(0x01fe), 200, ts + 3, &c2).await?;
        insert_stale_competitor(&client, &s2b, &hash_bytes(0x01fd), 200, ts + 4, &c2).await?;

        // Height 250: logical stale row exists, but its canonical competitor is
        // not coinbase-complete, so the navigation DTO must not advertise a
        // renderable target above the max complete canonical height.
        let c3 = hash_bytes(0x0250);
        let s3 = hash_bytes(0x1250);
        insert_canonical_competitor(&client, &c3, &hash_bytes(0x024f), 250, ts + 5, false).await?;
        insert_stale_competitor(&client, &s3, &hash_bytes(0x024e), 250, ts + 6, &c3).await?;

        let payload = fetch_stale_navigator(&client, "limit=10").await?;
        assert_stale_navigation_contract(&payload, &s1);
        assert_same_height_stored_tiebreak(&payload, &s2a, &s2b);
        assert_stale_navigator_keyset_tiebreak(&client, &payload).await?;

        Ok::<_, anyhow::Error>(())
    })
}
