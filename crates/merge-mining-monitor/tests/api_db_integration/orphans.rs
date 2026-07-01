use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use mmm_api::projection::{self};
use mmm_api::query::{self, NavigatorTarget};
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_capture::source_registry::SYSCOIN_SOURCE_CODE;
use mmm_read_model::restore_merge_mining_event;
use tokio_postgres::Client;

use crate::support::scenario::{
    ChildEvidence, capture_child_event, orphan_candidate_verdict, revoke_event,
};
use crate::support::seed::{display_hash, hash_bytes, header_hash_bytes};

use crate::helpers::{
    classify_all_unknowns_strict, format_api_error, format_projection_error, insert_unknown_block,
    set_orphan_class,
};

/// Bitcoin mainnet block 400000 (2016-02-25): raw 80-byte header and the
/// coinbase scriptSig (BIP34 push of 400000, "/BW Pool/" tag). Embedded so the
/// orphan scenario test below can drive the production strict-orphan
/// classifier with genuine PoW, era nBits, and BIP34 height evidence.
const BTC_400000_HEADER_HEX: &str = "0400000039fa821848781f027a2e6dfabbf6bda920d9ae61b63400030000000000000000ecae536a304042e3154be0e3e9a8220e5568c3433a9ab49ac4cbb74f8df8e8b0cc2acf569fb9061806652c27";
const BTC_400000_COINBASE_SCRIPTSIG_HEX: &str = "03801a060004cc2acf560433c30f37085d4a39ad543b0c000a425720537570706f727420384d200a666973686572206a696e78696e092f425720506f6f6c2f";

async fn fetch_orphans_page(
    client: &Client,
    raw_query: &str,
) -> Result<projection::NavigatorPayload> {
    let query = query::parse_navigator_query(NavigatorTarget::Orphan, Some(raw_query))
        .map_err(format_api_error)?;
    projection::navigator(client, &query)
        .await
        .map_err(format_projection_error)
}

async fn fetch_default_orphans_page(
    client: &Client,
    limit: usize,
) -> Result<projection::NavigatorPayload> {
    fetch_orphans_page(client, &format!("limit={limit}")).await
}

async fn btc_orphan_class(client: &Client, hash: &[u8]) -> Result<Option<String>> {
    Ok(client
        .query_one(
            "SELECT btc_orphan_class FROM block WHERE btc_header_hash = $1",
            &[&hash],
        )
        .await?
        .get(0))
}

fn orphan_hashes(page: &projection::NavigatorPayload) -> Vec<String> {
    page.items
        .iter()
        .map(|entry| entry.primary_hash.clone())
        .collect()
}

fn orphan_counts(page: &projection::NavigatorPayload) -> &projection::OrphanClassCounts {
    page.facets
        .orphan_classes
        .as_ref()
        .expect("orphan navigator carries class counts")
}

fn orphan_class(item: &projection::NavigatorItem) -> Option<&str> {
    item.orphan
        .as_ref()
        .and_then(|orphan| orphan.btc_orphan_class.as_deref())
}

#[tokio::test]
async fn orphans_classifies_real_header_weak_then_strict_and_tracks_revocation() -> Result<()> {
    crate::run_mut_db_test!(client, {
        // Read-model scenario: a genuine mainnet header (Bitcoin block 400000,
        // 2016-02-25, fetched once from Bitcoin Core) captured as a
        // Core-absence-attested unknown. Strict/weak orphan classification
        // through the production path needs a header with real BTC PoW and
        // era nBits plus BIP34 coinbase evidence - every committed AuxPoW
        // fixture's parent is a sub-BTC-target share (pow false, forced
        // near), and crafted regtest-bits headers classify
        // btc_stale_excluded. The fake verdict's absence attestation is what
        // makes the (in reality canonical) header an orphan candidate here.
        let parent: Header = deserialize(&hex::decode(BTC_400000_HEADER_HEX)?)?;
        let parent_hash = header_hash_bytes(&parent);

        // First capture the header WITHOUT its coinbase scriptSig: no BIP34
        // strict-height evidence reaches the rollup, so the offline classifier
        // falls through to the timestamp-selected nBits match and renders the
        // WEAK verdict.
        let weak_event_id = capture_child_event(
            &mut client,
            ChildEvidence::new(
                "weak_orphan",
                SYSCOIN_SOURCE_CODE,
                2_248_408,
                0x5b,
                parent,
                orphan_candidate_verdict(&parent),
                1_000,
            ),
        )
        .await?;
        let class = btc_orphan_class(&client, &parent_hash).await?;
        assert_eq!(class.as_deref(), Some("weak_btc_orphan"));
        let listed = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(listed.total, 1);
        assert_eq!(orphan_class(&listed.items[0]), Some("weak_btc_orphan"));
        revoke_event(&mut client, weak_event_id, "weak scenario revocation").await?;
        assert_eq!(fetch_default_orphans_page(&client, 10).await?.total, 0);

        // Re-capture with the real coinbase scriptSig: it carries the BIP34
        // height (400000), and the header's genuine nBits match the committed
        // table at that height, so the offline classifier renders the strict
        // verdict.
        let event_id = capture_child_event(
            &mut client,
            ChildEvidence::new(
                "strict_orphan",
                SYSCOIN_SOURCE_CODE,
                2_248_408,
                0x5a,
                parent,
                orphan_candidate_verdict(&parent),
                1_000,
            )
            .with_parent_coinbase_script(hex::decode(BTC_400000_COINBASE_SCRIPTSIG_HEX)?),
        )
        .await?;
        let class = btc_orphan_class(&client, &parent_hash).await?;
        assert_eq!(class.as_deref(), Some("strict_btc_orphan"));

        let listed = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(listed.total, 1);
        assert_eq!(listed.items[0].primary_hash, display_hash(&parent_hash));

        // Revoking the only contributing event removes the orphan from the
        // index through the production cascade; restoring under a re-attested
        // absence verdict re-derives the class and brings it back (a
        // transient Disabled-classifier restore would leave it unclassified,
        // per the preserve-under-transient-unknown contract).
        revoke_event(&mut client, event_id, "scenario revocation").await?;
        let revoked = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(revoked.total, 0);

        restore_merge_mining_event(
            &mut client,
            event_id,
            &ConfiguredParentClassifier::Fake(FakeParentClassifier::new(orphan_candidate_verdict(
                &parent,
            ))),
        )
        .await?;
        let restored = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(restored.total, 1);
        assert_eq!(restored.items[0].primary_hash, display_hash(&parent_hash));

        Ok::<_, anyhow::Error>(())
    })
}

async fn seed_paginated_unknowns(client: &Client) -> Result<[Vec<u8>; 4]> {
    // Unknown blocks have NULL height; located by btc_header_time. u2 and u3
    // share a header time (2000) to exercise the stored-hash tie-break and the
    // cursor boundary.
    let u1 = hash_bytes(0x00a1);
    let u2 = hash_bytes(0x00a2);
    let u3 = hash_bytes(0x00a3);
    let u4 = hash_bytes(0x00a4);
    for (hash, prev, time) in [
        (&u1, hash_bytes(0x10a1), 1000),
        (&u2, hash_bytes(0x10a2), 2000),
        (&u3, hash_bytes(0x10a3), 2000),
        (&u4, hash_bytes(0x10a4), 3000),
    ] {
        insert_unknown_block(client, hash, &prev, time).await?;
    }

    // A revocation husk: demote_zero_active_block leaves a fully-revoked,
    // non-core block as kind='unknown' with pow_validated=false. Its header
    // time (2500) would sort between u4 and the 2000 group if it leaked, so
    // the index must exclude it and total must not count it.
    let husk = hash_bytes(0x00a5);
    insert_unknown_block(client, &husk, &hash_bytes(0x10a5), 2500).await?;
    client
        .execute(
            "UPDATE block SET pow_validated = false WHERE btc_header_hash = $1",
            &[&husk],
        )
        .await?;
    classify_all_unknowns_strict(client).await?;

    Ok([u1, u2, u3, u4])
}

async fn assert_older_cursor_walk(client: &Client, expected: &[String]) -> Result<()> {
    // Walk the whole index one row at a time via the opaque item cursor; this
    // crosses the same-header-time boundary (u3 -> u2) with no skip or duplicate.
    // The cursor is precise: the last page returns its row with next_cursor =
    // None, so there is no trailing empty request.
    let mut walked = Vec::new();
    let mut cursor: Option<String> = None;
    let mut first = true;
    loop {
        let raw_query = cursor
            .as_ref()
            .map(|cursor| format!("cursor={cursor}&direction=older&limit=1"))
            .unwrap_or_else(|| "limit=1".to_owned());
        let page = fetch_orphans_page(client, &raw_query).await?;
        assert_eq!(page.total, 4);
        assert_eq!(page.items.len(), 1);
        // The Latest page (no cursor) sits at the global newest edge, so it has
        // no prev_cursor; every later (older) page has a newer neighbour.
        if first {
            assert!(
                page.prev_cursor.is_none(),
                "latest page must not expose a prev_cursor"
            );
        } else {
            assert!(
                page.prev_cursor.is_some(),
                "an older page must expose a prev_cursor toward newer rows"
            );
        }
        first = false;
        walked.push(page.items[0].primary_hash.clone());
        assert!(walked.len() <= 4, "cursor walk did not terminate");
        match page
            .items
            .first()
            .and_then(|item| page.next_cursor.as_ref().map(|_| item.cursor.clone()))
        {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(walked, expected);

    Ok(())
}

async fn assert_older_then_newer_round_trip(client: &Client) -> Result<()> {
    // Round trip: from the newest anchor, one older step then one newer step
    // returns to the same row.
    let newest = fetch_default_orphans_page(client, 1).await?.items.remove(0);
    let stepped_older = fetch_orphans_page(
        client,
        &format!("cursor={}&direction=older&limit=1", newest.cursor),
    )
    .await?
    .items
    .remove(0);
    let stepped_back = fetch_orphans_page(
        client,
        &format!("cursor={}&direction=newer&limit=1", stepped_older.cursor),
    )
    .await?
    .items
    .remove(0);
    assert_eq!(
        stepped_back.primary_hash, newest.primary_hash,
        "older-then-newer step returns to the same anchor"
    );

    Ok(())
}

#[tokio::test]
async fn orphans_paginate_newest_first_by_header_time() -> Result<()> {
    crate::run_db_test!(client, {
        // Empty DB: no orphans, no cursor, all per-class counts zero.
        let empty = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(empty.total, 0);
        assert!(empty.items.is_empty());
        assert_eq!(orphan_counts(&empty).strict, 0);
        assert_eq!(orphan_counts(&empty).pending, 0);
        assert!(empty.next_cursor.is_none());
        assert!(empty.prev_cursor.is_none());

        let [u1, u2, u3, u4] = seed_paginated_unknowns(&client).await?;

        // Newest-first by (btc_header_time DESC, stored hash DESC), husk excluded:
        // 3000:u4, then 2000:{u3,u2} (stored a3 > a2), then 1000:u1.
        let expected = vec![
            display_hash(&u4),
            display_hash(&u3),
            display_hash(&u2),
            display_hash(&u1),
        ];

        assert_older_cursor_walk(&client, &expected).await?;

        // A page large enough for the whole index returns it in order, no cursor
        // either way (it is both the newest and oldest edge).
        let all = fetch_default_orphans_page(&client, 100).await?;
        assert_eq!(orphan_hashes(&all), expected);
        assert_eq!(all.total, 4);
        // counts is the full per-class breakdown over the PoW-valid unknown
        // population (the pow_validated=false husk is excluded from counts too).
        assert_eq!(orphan_counts(&all).strict, 4);
        assert_eq!(orphan_counts(&all).weak, 0);
        assert_eq!(orphan_counts(&all).excluded, 0);
        assert_eq!(orphan_counts(&all).pending, 0);
        assert!(all.next_cursor.is_none());
        assert!(all.prev_cursor.is_none());

        // Newer walk: from the oldest row, step newer one row at a time via the
        // opaque cursor, crossing the same-time boundary (u2 -> u3) with no skip
        // or duplicate. It reconstructs the
        // ascending order, and every after page exposes a next_cursor (the anchor
        // it stepped off is always an older neighbour).
        let oldest = all.items.last().expect("non-empty index").clone();
        let mut newer_walked = Vec::new();
        let mut after_cursor: Option<String> = Some(oldest.cursor.clone());
        while let Some(cursor) = after_cursor.clone() {
            let page =
                fetch_orphans_page(&client, &format!("cursor={cursor}&direction=newer&limit=1"))
                    .await?;
            assert_eq!(page.total, 4);
            assert_eq!(page.items.len(), 1);
            assert!(
                page.next_cursor.is_some(),
                "an after page must expose a next_cursor toward older rows"
            );
            newer_walked.push(page.items[0].primary_hash.clone());
            assert!(newer_walked.len() <= 3, "newer walk did not terminate");
            after_cursor = page
                .items
                .first()
                .and_then(|item| page.prev_cursor.as_ref().map(|_| item.cursor.clone()));
        }
        let mut ascending = expected.clone();
        ascending.reverse();
        assert_eq!(newer_walked, ascending[1..].to_vec());

        assert_older_then_newer_round_trip(&client).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn orphans_filter_by_classification() -> Result<()> {
    crate::run_db_test!(client, {
        // One PoW-valid unknown per class, plus one pending (NULL class).
        let strict = hash_bytes(0x0b01);
        let weak = hash_bytes(0x0b02);
        let excluded = hash_bytes(0x0b03);
        let pending = hash_bytes(0x0b04);
        for (hash, prev, time) in [
            (&strict, hash_bytes(0x0c01), 4000),
            (&weak, hash_bytes(0x0c02), 3000),
            (&excluded, hash_bytes(0x0c03), 2000),
            (&pending, hash_bytes(0x0c04), 1000),
        ] {
            insert_unknown_block(&client, hash, &prev, time).await?;
        }
        set_orphan_class(&client, &strict, "strict_btc_orphan").await?;
        set_orphan_class(&client, &weak, "weak_btc_orphan").await?;
        set_orphan_class(&client, &excluded, "btc_stale_excluded").await?;
        // `pending` keeps its NULL class.

        // Default (strict+weak): only the strict and weak rows, newest-first.
        let default = fetch_default_orphans_page(&client, 100).await?;
        assert_eq!(
            orphan_hashes(&default),
            vec![display_hash(&strict), display_hash(&weak)]
        );
        assert_eq!(default.total, 2);
        // `counts` is the full per-class breakdown, INDEPENDENT of the filter.
        assert_eq!(orphan_counts(&default).strict, 1);
        assert_eq!(orphan_counts(&default).weak, 1);
        assert_eq!(orphan_counts(&default).excluded, 1);
        assert_eq!(orphan_counts(&default).pending, 1);
        // Each returned row carries its own class.
        assert_eq!(orphan_class(&default.items[0]), Some("strict_btc_orphan"));
        assert_eq!(orphan_class(&default.items[1]), Some("weak_btc_orphan"));

        // Excluded-only filter returns just the excluded row.
        let excluded_page =
            fetch_orphans_page(&client, "classification=btc_stale_excluded&limit=100").await?;
        assert_eq!(orphan_hashes(&excluded_page), vec![display_hash(&excluded)]);
        assert_eq!(excluded_page.total, 1);

        // Pending-only filter returns the NULL-class row (with a null row class).
        let pending_page = fetch_orphans_page(&client, "classification=pending&limit=100").await?;
        assert_eq!(orphan_hashes(&pending_page), vec![display_hash(&pending)]);
        assert_eq!(pending_page.total, 1);
        assert!(orphan_class(&pending_page.items[0]).is_none());

        // All four classes selected returns every PoW-valid unknown.
        let all = fetch_orphans_page(
            &client,
            "classification=strict_btc_orphan,weak_btc_orphan,btc_stale_excluded,pending&limit=100",
        )
        .await?;
        assert_eq!(all.total, 4);
        assert_eq!(all.items.len(), 4);

        Ok::<_, anyhow::Error>(())
    })
}
