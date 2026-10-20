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
/// with genuine same-height groups. Height alone is not a unique key, so the
/// navigator must tie-break on hash.
///
/// Hash values are chosen so stored-byte order and display-hex order DISAGREE
/// (see the assertion in `assert_stored_not_display_order`): `hash_bytes` writes
/// the value big-endian into the trailing bytes, so stored order tracks the
/// value while the display form reverses the bytes and keys on the low byte
/// first. A regression to display-hex ordering flips the groups and fails.
struct SeededCatalogue {
    expected: Vec<Vec<u8>>,
}

async fn seed_catalogue(client: &Client) -> Result<SeededCatalogue> {
    let ts = day_epoch(2026, Month::May, 10);

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

    let mut expected = vec![h300];
    let mut g200 = group200;
    g200.sort();
    expected.extend(g200);
    let mut g100 = group100;
    g100.sort();
    expected.extend(g100);

    Ok(SeededCatalogue { expected })
}

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

        for (offset, item) in full.items.iter().enumerate() {
            assert_eq!(item.index, offset as u64 + 1);
        }
        assert_eq!(full.items[0].position.max, 300);
        assert_eq!(full.items[0].position.min, 300);
        assert_eq!(full.items[0].kind, "error-block");
        assert!(full.items[0].branch.is_none());
        assert!(full.items[0].orphan.is_none());

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

        let mut walked: Vec<String> = Vec::new();
        let mut page = fetch_error_block_navigator(&client, "limit=2").await?;
        assert!(page.prev_cursor.is_none(), "first page has nothing newer");
        assert_eq!(page.items[0].index, 1);
        assert_eq!(page.items[1].index, 2);
        let older = fetch_error_block_navigator(
            &client,
            &format!(
                "cursor={}&direction=older&limit=2",
                page.next_cursor.as_ref().expect("older cursor")
            ),
        )
        .await?;
        assert_eq!(older.items[0].index, 3);
        assert_eq!(older.items[1].index, 4);
        let peeked = fetch_error_block_navigator(
            &client,
            &format!(
                "cursor={}&direction=newer&limit=1",
                older.prev_cursor.as_ref().expect("newer cursor")
            ),
        )
        .await?;
        assert_eq!(peeked.items.len(), 1);
        assert_eq!(peeked.items[0].index, 2);
        assert_eq!(peeked.items[0].primary_hash, page.items[1].primary_hash);
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

        // Each newer-ward page is returned newest-first, so reverse page order
        // while preserving each page's internal order.
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
        witness_error_block(&client, &target, &hash_bytes(0x7776), 50, ts + 1).await?;

        let payload = fetch_error_block_navigator(&client, "limit=1").await?;
        assert_eq!(payload.items.len(), 1);
        let item = &payload.items[0];
        assert!(
            item.view_error.is_none(),
            "a complete backbone must yield a renderable window, got {:?}",
            item.view_error
        );

        let view = serde_json::to_value(item.view.as_ref().expect("renderable window"))?;
        assert_eq!(view["mode"], "tree_window");
        assert_eq!(view["target_height"], 50);
        assert_eq!(view["tree_from"], 34);
        assert_eq!(view["tree_to"], 66);
        assert_eq!(view["select_hash"], display_hash(&target));

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

/// Attach a witnessing event so `/tree` counts a source for the error block.
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

/// Fill heights 34..=66 with 468 witnessed error blocks to exceed `TREE_NODE_LIMIT` (500).
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
            "WITH o AS ( \
                 INSERT INTO child_block_observation ( \
                     source_id, child_height, child_block_hash, observation_kind, \
                     child_block_time, pow_validates_child_target, btc_parent_header_hash, \
                     recorded_at \
                 ) \
                 SELECT $2, 1000 + g, decode(lpad(to_hex(6000000 + g), 64, '0'), 'hex'), \
                        'evidence', $1::bigint + g, TRUE, \
                        decode(lpad(to_hex(5000000 + g), 64, '0'), 'hex'), $1::bigint + g \
                 FROM generate_series(1, 468) AS g \
                 RETURNING id, child_height - 1000 AS g) \
             INSERT INTO merge_mining_event ( \
                source_id, child_height, child_block_hash, \
                btc_parent_header_hash, btc_parent_prev_header_hash, \
                btc_parent_header_bytes, btc_parent_header_time, btc_parent_height, \
                btc_parent_kind, pow_validates_btc_target, \
                discovered_at, confirmed_at, observation_id \
             ) \
             SELECT $2, 1000 + g, decode(lpad(to_hex(6000000 + g), 64, '0'), 'hex'), \
                    decode(lpad(to_hex(5000000 + g), 64, '0'), 'hex'), \
                    decode(lpad(to_hex(4999999 + g), 64, '0'), 'hex'), \
                    decode(lpad(to_hex(5000000 + g), 160, '0'), 'hex'), \
                    $1::bigint + g, 34 + (g % 33), 'error_block', TRUE, \
                    $1::bigint + g, $1::bigint + g, o.id \
             FROM o",
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

        assert!(
            project_tree(&client, Some("from_height=34&to_height=66"))
                .await
                .is_err(),
            "fixture must put the full padded window beyond what /tree will render"
        );

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

        let anchored = fetch_error_block_navigator(
            &client,
            &format!("anchor_hash={}&limit=1", display_hash(&sourceless)),
        )
        .await?;
        assert!(anchored.items.is_empty());
        assert_eq!(anchored.total, 1);
        Ok(())
    })
}

#[tokio::test]
async fn error_block_navigator_anchors_on_a_catalogued_hash() -> Result<()> {
    crate::run_db_test!(client, {
        let seeded = seed_catalogue(&client).await?;
        let anchor = display_hash(&seeded.expected[3]);
        let payload =
            fetch_error_block_navigator(&client, &format!("anchor_hash={anchor}&limit=10")).await?;
        assert_eq!(payload.items.len(), 1);
        assert_eq!(payload.items[0].primary_hash, anchor);
        assert_eq!(payload.items[0].position.max, 200);
        assert_eq!(payload.items[0].index, 4);
        assert_ne!(
            payload.items[0].primary_hash,
            display_hash(&seeded.expected[1]),
            "the anchored member must not be the same-height group's newest"
        );
        Ok(())
    })
}
