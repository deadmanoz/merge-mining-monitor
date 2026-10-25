use anyhow::{Context, Result};
use mmm_api::projection::{NavigatorItem, NavigatorPayload, NavigatorView};
use mmm_api::query::NavigatorTarget;
use time::Month;
use tokio_postgres::Client;

use crate::helpers::{fetch_navigator, project_tree, seed_canonical_chain};
use crate::support::seed::{day_epoch, display_hash, hash_bytes, insert_orphan};

async fn fetch_mixed_signals(client: &Client, raw_query: &str) -> Result<NavigatorPayload> {
    fetch_navigator(client, NavigatorTarget::MixedSignal, Some(raw_query)).await
}

/// Store a mixed summary for one block row, as the read model would.
async fn mark_mixed(client: &Client, hash: &[u8]) -> Result<()> {
    client
        .execute(
            "INSERT INTO relationship_summary \
                 (btc_header_hash, btc_header_time, btc_height, mixed, pool_ids) \
             SELECT btc_header_hash, btc_header_time, btc_height, TRUE, '{}' \
               FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await?;
    Ok(())
}

async fn seed_orphan(client: &Client, seed: u32, time: i64, class: &str) -> Result<Vec<u8>> {
    let hash = hash_bytes(seed);
    insert_orphan(client, &hash, &hash_bytes(seed + 1), time, class).await?;
    mark_mixed(client, &hash).await?;
    Ok(hash)
}

/// `(hash, step height, index)` for each item.
fn steps(page: &NavigatorPayload) -> Vec<(String, i64, u64)> {
    page.items
        .iter()
        .map(|item| (item.primary_hash.clone(), item.position.min, item.index))
        .collect()
}

/// The item's advertised tree view renders it with the mixed outline.
async fn assert_view_shows_mixed(client: &Client, item: &NavigatorItem) -> Result<()> {
    let query = match item.view.as_ref().context("a renderable view")? {
        NavigatorView::TreeWindow {
            tree_from, tree_to, ..
        } => format!("from_height={tree_from}&to_height={tree_to}"),
        NavigatorView::UnheightedAnchor { anchor_hash, .. } => {
            format!("unheighted_anchor={anchor_hash}")
        }
    };
    let tree = project_tree(client, Some(&query)).await?;
    let node = tree
        .nodes
        .iter()
        .find(|node| node.hash == item.primary_hash)
        .with_context(|| format!("{query} renders {}", item.primary_hash))?;
    assert!(node.relationship.mixed, "{query}");
    Ok(())
}

#[tokio::test]
async fn mixed_signals_step_by_height_then_time_and_page_both_ways() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);
        let canonical =
            seed_canonical_chain(&client, 1..=100, 0x1_0000, 0x0fff, ts + 1, None).await?;
        mark_mixed(&client, &canonical[&90]).await?;
        mark_mixed(&client, &canonical[&60]).await?;
        // An orphan steps at the canonical block with the latest time at or before its own;
        // two that predate the stored chain step at 0, newer first.
        let at_75 = seed_orphan(&client, 0x2000, ts + 75, "strict_btc_orphan").await?;
        let first = seed_orphan(&client, 0x2010, ts - 10, "weak_btc_orphan").await?;
        let second = seed_orphan(&client, 0x2020, ts - 5, "strict_btc_orphan").await?;
        let excluded = seed_orphan(&client, 0x2030, ts + 50, "excluded").await?;

        let expected = [
            (display_hash(&canonical[&90]), 90, 1),
            (display_hash(&at_75), 75, 2),
            (display_hash(&canonical[&60]), 60, 3),
            (display_hash(&second), 0, 4),
            (display_hash(&first), 0, 5),
        ];
        let mut older = Vec::new();
        let mut page = fetch_mixed_signals(&client, "limit=1").await?;
        assert_eq!((page.total, page.prev_cursor.as_deref()), (5, None));
        loop {
            older.extend(steps(&page));
            for item in &page.items {
                assert_view_shows_mixed(&client, item).await?;
            }
            let Some(cursor) = page.next_cursor.clone() else {
                break;
            };
            page =
                fetch_mixed_signals(&client, &format!("limit=1&cursor={cursor}&direction=older"))
                    .await?;
        }
        assert_eq!(older, expected);

        let mut newer = steps(&page);
        while let Some(cursor) = page.prev_cursor.clone() {
            page =
                fetch_mixed_signals(&client, &format!("limit=1&cursor={cursor}&direction=newer"))
                    .await?;
            newer.extend(steps(&page));
        }
        newer.reverse();
        assert_eq!(newer, expected);

        let anchored =
            fetch_mixed_signals(&client, &format!("anchor_hash={}", display_hash(&at_75))).await?;
        let orphan = anchored.items[0].orphan.as_ref().context("orphan member")?;
        assert_eq!(
            (anchored.items[0].index, orphan.btc_header_time),
            (2, ts + 75)
        );

        let with_excluded = fetch_mixed_signals(
            &client,
            "limit=10&classification=strict_btc_orphan,weak_btc_orphan,excluded",
        )
        .await?;
        assert_eq!(
            steps(&with_excluded)[3],
            (display_hash(&excluded), 50, 4),
            "the class filter admits its orphans"
        );
        Ok(())
    })
}
