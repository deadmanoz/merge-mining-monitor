use anyhow::Result;
use mmm_api::projection::{self};
use mmm_api::query::{self, NavigatorTarget};
use time::Month;
use tokio_postgres::Client;

use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::get_source_id;

use crate::support::seed::{
    EventSeed, day_epoch, display_hash, hash_bytes, insert_block, insert_error_block,
    insert_error_block_with_sources, insert_event,
};

use crate::helpers::{format_api_error, format_projection_error, project_tree};

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
/// with genuine same-height groups. The committed research catalogue holds 35
/// blocks across only 28 heights, with one height carrying six of them, so
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

        // And back: stepping newer from the last page must revisit the whole
        // catalogue. Each newer-ward page is itself returned newest-first, so
        // the reconstruction reverses the PAGE order and keeps each page's
        // internal order, rather than reversing the flattened list.
        let mut back_pages: Vec<Vec<String>> = Vec::new();
        loop {
            back_pages.push(
                page.items
                    .iter()
                    .map(|item| item.primary_hash.clone())
                    .collect(),
            );
            let Some(cursor) = page.prev_cursor.clone() else {
                break;
            };
            page = fetch_error_block_navigator(
                &client,
                &format!("cursor={cursor}&direction=newer&limit=2"),
            )
            .await?;
        }
        back_pages.reverse();
        let back: Vec<String> = back_pages.into_iter().flatten().collect();
        // Uniqueness alone would still pass if the walk skipped blocks or
        // stopped early, so assert the full reconstruction: newer-ward paging
        // visits every block in catalogue order and terminates only once it has
        // reached the newest edge.
        assert_eq!(
            back, expected,
            "a newer-ward walk must reconstruct the whole catalogue"
        );
        assert_eq!(
            page.prev_cursor, None,
            "the newer-ward walk must terminate at the newest edge"
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
        // A catalogued error block is witnessed by definition. The tree derives
        // a node's source count from events, not from the block rollup column,
        // so without a real witnessing event it renders as sourceless, is
        // dropped under the default `min_sources`, and the round-trip below
        // cannot pass.
        witness_error_block(&client, &target, &hash_bytes(0x7776), 50, ts + 1).await?;

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

        // Round-trip the emitted window through the real tree projection: the
        // navigator must not advertise a view whose tree response omits the
        // block it selects, which would leave the UI falling back to the tip.
        let tree = project_tree(
            &client,
            Some(&format!(
                "from_height={}&to_height={}",
                view["tree_from"], view["tree_to"]
            )),
        )
        .await?;
        let rendered = serde_json::to_value(&tree)?;
        let selected = rendered["nodes"]
            .as_array()
            .expect("tree nodes")
            .iter()
            .any(|node| node["hash"] == view["select_hash"]);
        assert!(
            selected,
            "the advertised window must actually render its select_hash"
        );
        Ok(())
    })
}

/// Attach a real witnessing event to a catalogued error block.
///
/// The tree derives a node's source count from events rather than the block
/// rollup column, so a catalogue row without one renders as sourceless, is
/// dropped under the default `min_sources`, and never appears in a window.
async fn witness_error_block(
    client: &Client,
    hash: &[u8],
    prev_hash: &[u8],
    height: i32,
    ts: i64,
) -> Result<()> {
    let namecoin = get_source_id(client, NAMECOIN_SOURCE_CODE).await?;
    insert_event(
        client,
        EventSeed {
            source_id: namecoin,
            child_height: 1,
            child_hash: hash_bytes(0x7ccc),
            parent_hash: hash.to_vec(),
            prev_hash: prev_hash.to_vec(),
            parent_time: ts,
            kind: "error_block",
            pow_validates_btc_target: true,
            btc_height: Some(height),
            pool_id: None,
        },
    )
    .await?;
    Ok(())
}

/// Fill heights 34..=66 with 468 witnessed error blocks, pushing that window
/// past `TREE_NODE_LIMIT` (500) so the navigator must narrow it.
async fn crowd_window_with_error_blocks(client: &Client, ts: i64) -> Result<()> {
    let namecoin = get_source_id(client, NAMECOIN_SOURCE_CODE).await?;
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
                    'not_attempted', 1, 1, 1, FALSE, FALSE, TRUE, $1::bigint + g, $1::bigint + g \
             FROM generate_series(1, 468) AS g",
            &[&ts],
        )
        .await?;
    client
        .execute(
            "INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, child_block_time, \
                btc_parent_header_hash, btc_parent_prev_header_hash, \
                btc_parent_header_bytes, btc_parent_header_time, btc_parent_height, \
                btc_parent_kind, pow_validates_btc_target, pow_validates_child_target, \
                discovered_at, confirmed_at \
             ) \
             SELECT $2, 1000 + g, decode(lpad(to_hex(6000000 + g), 64, '0'), 'hex'), \
                    $1::bigint + g, \
                    decode(lpad(to_hex(5000000 + g), 64, '0'), 'hex'), \
                    decode(lpad(to_hex(4999999 + g), 64, '0'), 'hex'), \
                    decode(lpad(to_hex(5000000 + g), 160, '0'), 'hex'), \
                    $1::bigint + g, 34 + (g % 33), 'error_block', TRUE, TRUE, \
                    $1::bigint + g, $1::bigint + g \
             FROM generate_series(1, 468) AS g",
            &[&ts, &namecoin],
        )
        .await?;
    Ok(())
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

        witness_error_block(&client, &target, &hash_bytes(0x7776), 50, ts + 1).await?;
        crowd_window_with_error_blocks(&client, ts).await?;

        // Guard against a vacuous test: the full padded window MUST be one the
        // tree refuses, otherwise nothing here exercises the cap at all.
        assert!(
            project_tree(&client, Some("from_height=34&to_height=66"))
                .await
                .is_err(),
            "fixture must put the full padded window beyond what /tree will render"
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
        let tree_from = view["tree_from"].as_i64().expect("tree_from");
        let tree_to = view["tree_to"].as_i64().expect("tree_to");

        // The navigator degrades to a narrower window rather than refusing, so
        // the contract is that whatever window it advertises, `/tree` actually
        // serves. Round-trip it rather than recounting rows, which is what makes
        // this catch a budget that disagrees with tree visibility.
        let tree = project_tree(
            &client,
            Some(&format!("from_height={tree_from}&to_height={tree_to}")),
        )
        .await
        .map_err(|err| {
            anyhow::anyhow!(
                "navigator advertised [{tree_from}, {tree_to}] but /tree refused: {err}"
            )
        })?;
        let rendered = serde_json::to_value(&tree)?;
        assert!(
            rendered["nodes"]
                .as_array()
                .expect("tree nodes")
                .iter()
                .any(|node| node["hash"] == view["select_hash"]),
            "the narrowed window must still render its select_hash"
        );
        Ok(())
    })
}

#[tokio::test]
async fn sourceless_error_blocks_do_not_consume_the_tree_node_budget() -> Result<()> {
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
        witness_error_block(&client, &target, &hash_bytes(0x7776), 50, ts + 1).await?;

        // Crowd the target's window past the 500-node cap with error blocks that
        // carry NO event. The tree never renders them, so they must not narrow
        // the advertised window. Without the eligibility predicate in the budget
        // query these invisible rows would push it over the cap and force a
        // narrower window than the tree actually needs.
        client
            .execute(
                "INSERT INTO block ( \
                    btc_header_hash, btc_prev_header_hash, btc_height, btc_height_source, \
                    kind, error_block_reason, btc_header_bytes, btc_header_time, \
                    btc_coinbase_status, total_attestations, distinct_sources, \
                    auxpow_chain_count, live_observed, core_attested, pow_validated, \
                    created_at, updated_at \
                 ) \
                 SELECT decode(lpad(to_hex(7000000 + g), 64, '0'), 'hex'), \
                        decode(lpad(to_hex(6999999 + g), 64, '0'), 'hex'), \
                        34 + (g % 33), 'error-block-catalog', \
                        'error_block', 'bip34_coinbase_height_mismatch', \
                        decode(lpad(to_hex(7000000 + g), 160, '0'), 'hex'), $1::bigint + g, \
                        'not_attempted', 0, 0, 0, FALSE, FALSE, TRUE, $1::bigint + g, $1::bigint + g \
                 FROM generate_series(1, 600) AS g",
                &[&ts],
            )
            .await?;

        let payload = fetch_error_block_navigator(
            &client,
            &format!("anchor_hash={}&limit=1", display_hash(&target)),
        )
        .await?;
        let view = serde_json::to_value(
            payload.items[0]
                .view
                .as_ref()
                .expect("invisible rows must not deny a window"),
        )?;
        // The full padded window, unnarrowed: 33 heights plus one visible error
        // block, well inside the cap once the 600 invisible rows are excluded.
        assert_eq!(view["tree_from"], 34);
        assert_eq!(view["tree_to"], 66);
        project_tree(&client, Some("from_height=34&to_height=66")).await?;
        Ok(())
    })
}

#[tokio::test]
async fn error_block_navigator_skips_rows_the_tree_would_filter_out() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        seed_canonical_backbone(&client, 100, ts).await?;

        // Genuinely witnessed: the rollup column AND an active event, which is
        // what `/tree` actually counts. Seeding the column alone would make this
        // control indistinguishable from `sourceless` to the tree, and the test
        // would then only prove the navigator's own predicate rather than the
        // renderability parity it claims.
        let witnessed = hash_bytes(0x4001);
        insert_error_block(
            &client,
            &witnessed,
            &hash_bytes(0x4000),
            50,
            ts + 1,
            "time_below_mtp",
        )
        .await?;
        witness_error_block(&client, &witnessed, &hash_bytes(0x4000), 50, ts + 1).await?;
        // No active source at all: `/tree` drops this under the default
        // min_sources of 1, so offering it would advertise a window whose tree
        // response omits the block it selects.
        let sourceless = hash_bytes(0x4002);
        insert_error_block_with_sources(
            &client,
            &sourceless,
            &hash_bytes(0x4000),
            60,
            ts + 2,
            "time_below_mtp",
            0,
        )
        .await?;

        let payload = fetch_error_block_navigator(&client, "limit=10").await?;
        assert_eq!(payload.total, 1, "only the witnessed block is navigable");
        assert_eq!(payload.items.len(), 1);
        assert_eq!(payload.items[0].primary_hash, display_hash(&witnessed));

        // Parity: the block the navigator DOES offer must actually render, and
        // the one it withholds must actually be absent.
        let item = &payload.items[0];
        let view = serde_json::to_value(item.view.as_ref().expect("renderable window"))?;
        let tree = project_tree(
            &client,
            Some(&format!(
                "from_height={}&to_height={}",
                view["tree_from"], view["tree_to"]
            )),
        )
        .await?;
        let rendered = serde_json::to_value(&tree)?;
        let hashes = rendered["nodes"]
            .as_array()
            .expect("tree nodes")
            .iter()
            .map(|node| node["hash"].as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        assert!(
            hashes.contains(&display_hash(&witnessed)),
            "the offered block must render in the window it advertises"
        );
        assert!(
            !hashes.contains(&display_hash(&sourceless)),
            "the withheld block must indeed be one the tree filters out"
        );

        // Anchor mode agrees, so a deep link cannot reach it either.
        let anchored = fetch_error_block_navigator(
            &client,
            &format!("anchor_hash={}&limit=1", display_hash(&sourceless)),
        )
        .await?;
        assert!(anchored.items.is_empty());
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
