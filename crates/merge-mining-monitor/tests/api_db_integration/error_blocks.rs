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

/// Seed a complete canonical backbone over `[1, to_height]` in one statement.
/// Hashes are derived from the height so they are unique and reproducible.
async fn seed_canonical_backbone(client: &Client, to_height: i32, ts: i64) -> Result<()> {
    client
        .execute(
            "INSERT INTO block ( \
                btc_header_hash, btc_prev_header_hash, btc_height, btc_height_source, \
                kind, btc_header_bytes, btc_header_time, btc_coinbase_script, \
                btc_coinbase_status, total_attestations, distinct_sources, \
                auxpow_chain_count, live_observed, core_attested, pow_validated, \
                created_at, updated_at \
             ) \
             SELECT decode(lpad(to_hex(1000000 + g), 64, '0'), 'hex'), \
                    decode(lpad(to_hex(999999 + g), 64, '0'), 'hex'), \
                    g, 'bitcoin-core', 'canonical', \
                    decode(lpad(to_hex(g), 160, '0'), 'hex'), $2::bigint + g, \
                    decode('51', 'hex'), 'complete', 0, 0, 0, FALSE, FALSE, TRUE, \
                    $2::bigint + g, $2::bigint + g \
             FROM generate_series(1, $1::int) AS g",
            &[&to_height, &ts],
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn error_block_navigator_projects_a_complete_target_window() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        seed_canonical_backbone(&client, 100, ts).await?;

        let target = hash_bytes(0x7777);
        insert_error_block(
            &client,
            &target,
            &hash_bytes(0x7776),
            50,
            ts + 1,
            "time_below_mtp",
        )
        .await?;

        let payload = fetch_error_block_navigator(&client, "limit=1").await?;
        assert_eq!(payload.items.len(), 1);
        let item = &payload.items[0];
        assert!(
            item.view_error.is_none(),
            "a complete backbone must yield a renderable window, got {:?}",
            item.view_error
        );

        // NAVIGATION_PADDING is 16 either side, clamped to the complete tip, and
        // the widest fitting window wins.
        let view = serde_json::to_value(item.view.as_ref().expect("renderable window"))?;
        assert_eq!(view["mode"], "tree_window");
        assert_eq!(view["target_height"], 50);
        assert_eq!(view["tree_from"], 34);
        assert_eq!(view["tree_to"], 66);
        assert_eq!(view["select_hash"], display_hash(&target));
        Ok(())
    })
}

#[tokio::test]
async fn error_blocks_count_against_the_tree_node_budget() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        seed_canonical_backbone(&client, 100, ts).await?;

        let target = hash_bytes(0x7777);
        insert_error_block(
            &client,
            &target,
            &hash_bytes(0x7776),
            50,
            ts + 1,
            "time_below_mtp",
        )
        .await?;

        // Crowd the target's 33-height window ([34, 66]) with error blocks until
        // the visible-node count crosses TREE_NODE_LIMIT (500). Every extra row
        // is an error block and none is stale, so a budget that counts only
        // `kind = 'stale'` sees 33 nodes and happily proposes the window, while
        // the true cost is 33 + 469. `/tree` would then reject the very window
        // the navigator advertised with `range_too_large`.
        client
            .execute(
                "INSERT INTO block ( \
                    btc_header_hash, btc_prev_header_hash, btc_height, btc_height_source, \
                    kind, error_block_reason, btc_header_bytes, btc_header_time, \
                    btc_coinbase_status, total_attestations, distinct_sources, \
                    auxpow_chain_count, live_observed, core_attested, pow_validated, \
                    created_at, updated_at \
                 ) \
                 SELECT decode(lpad(to_hex(5000000 + g), 64, '0'), 'hex'), \
                        decode(lpad(to_hex(4999999 + g), 64, '0'), 'hex'), \
                        34 + (g % 33), 'error-block-catalog', \
                        'error_block', 'bip34_coinbase_height_mismatch', \
                        decode(lpad(to_hex(5000000 + g), 160, '0'), 'hex'), $1::bigint + g, \
                        'not_attempted', 0, 0, 0, FALSE, FALSE, TRUE, $1::bigint + g, $1::bigint + g \
                 FROM generate_series(1, 468) AS g",
                &[&ts],
            )
            .await?;

        // The navigator degrades to a narrower window rather than refusing, so
        // the observable contract is that whatever window it DOES advertise
        // fits the node budget once error blocks are counted.
        let off_spine_in = |from: i32, to: i32| {
            let client = &client;
            async move {
                let row = client
                    .query_one(
                        "SELECT count(*)::bigint FROM block \
                         WHERE kind IN ('stale', 'error_block') \
                           AND btc_height BETWEEN $1 AND $2",
                        &[&from, &to],
                    )
                    .await?;
                anyhow::Ok(row.get::<_, i64>(0))
            }
        };

        // Guard against a vacuous test: the full padded window MUST be over
        // budget, otherwise nothing here exercises the cap at all.
        let full_window_nodes = i64::from(66 - 34 + 1) + off_spine_in(34, 66).await?;
        assert!(
            full_window_nodes > 500,
            "fixture must put the full padded window over the 500-node cap, got {full_window_nodes}"
        );

        // Anchor on the height-50 target specifically: `limit=1` alone would
        // return the NEWEST catalogued block, which the crowd above supplies.
        let payload = fetch_error_block_navigator(
            &client,
            &format!("anchor_hash={}&limit=1", display_hash(&target)),
        )
        .await?;
        let item = &payload.items[0];
        assert_eq!(item.primary_hash, display_hash(&target));
        let view = serde_json::to_value(item.view.as_ref().expect("renderable window"))?;
        let tree_from = view["tree_from"].as_i64().expect("tree_from") as i32;
        let tree_to = view["tree_to"].as_i64().expect("tree_to") as i32;

        let advertised_nodes =
            i64::from(tree_to - tree_from + 1) + off_spine_in(tree_from, tree_to).await?;
        assert!(
            advertised_nodes <= 500,
            "navigator advertised [{tree_from}, {tree_to}] costing {advertised_nodes} nodes, \
             which /tree would reject as range_too_large"
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
