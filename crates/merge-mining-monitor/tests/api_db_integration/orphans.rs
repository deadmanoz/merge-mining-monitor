use anyhow::Result;
use bitcoin::block::Header;
use bitcoin::hashes::Hash as _;
use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness, absolute,
    consensus::{deserialize, serialize},
    transaction,
};
use mmm_api::projection::{self};
use mmm_api::query::{self, NavigatorTarget};
use mmm_bitcoin_core::{ConfiguredParentClassifier, FakeParentClassifier};
use mmm_capture::source_registry::{HATHOR_SOURCE_CODE, SYSCOIN_SOURCE_CODE};
use mmm_read_model::{reconcile_from_merge_mining_event, restore_merge_mining_event};
use tokio_postgres::Client;

use crate::support::scenario::{
    ChildEvidence, capture_child_event, orphan_candidate_verdict, revoke_event,
};
use crate::support::seed::{display_hash, hash_bytes, header_hash_bytes, insert_block};

use crate::helpers::{
    classify_all_unknowns_strict, format_api_error, format_projection_error, insert_unknown_block,
    project_tree, set_orphan_class,
};

/// Bitcoin mainnet block 400000 (2016-02-25) header and BIP34 coinbase scriptSig.
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

fn serialized_coinbase_tx(script_sig: &[u8]) -> Vec<u8> {
    serialize(&Transaction {
        version: transaction::Version::ONE,
        lock_time: absolute::LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::null(),
            script_sig: ScriptBuf::from_bytes(script_sig.to_vec()),
            sequence: Sequence::MAX,
            witness: Witness::default(),
        }],
        output: vec![TxOut {
            value: Amount::ZERO,
            script_pubkey: ScriptBuf::new(),
        }],
    })
}

#[tokio::test]
async fn retained_hathor_script_needs_matching_full_coinbase_for_strict_evidence() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let parent: Header = deserialize(&hex::decode(BTC_400000_HEADER_HEX)?)?;
        let parent_hash = header_hash_bytes(&parent);
        let script_sig = hex::decode(BTC_400000_COINBASE_SCRIPTSIG_HEX)?;
        let absent = ConfiguredParentClassifier::Fake(FakeParentClassifier::new(
            orphan_candidate_verdict(&parent),
        ));

        // Script-only Hathor observations remain weak without the full coinbase tx.
        let event_id = capture_child_event(
            &mut client,
            ChildEvidence::new(
                "retained_hathor",
                HATHOR_SOURCE_CODE,
                4_000_000,
                0x5c,
                parent,
                orphan_candidate_verdict(&parent),
                1_000,
            )
            .with_parent_coinbase_script(script_sig.clone()),
        )
        .await?;
        assert_eq!(
            btc_orphan_class(&client, &parent_hash).await?.as_deref(),
            Some("weak_btc_orphan")
        );
        let weak = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(weak.total, 1);
        assert_eq!(orphan_class(&weak.items[0]), Some("weak_btc_orphan"));

        let tx_bytes = serialized_coinbase_tx(&script_sig);
        client
            .execute(
                "UPDATE merge_mining_event \
                 SET btc_parent_coinbase_tx_bytes = $2 \
                 WHERE id = $1",
                &[&event_id, &tx_bytes],
            )
            .await?;
        reconcile_from_merge_mining_event(&mut client, event_id, &absent, None).await?;

        assert_eq!(
            btc_orphan_class(&client, &parent_hash).await?.as_deref(),
            Some("strict_btc_orphan")
        );
        let strict = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(strict.total, 1);
        assert_eq!(orphan_class(&strict.items[0]), Some("strict_btc_orphan"));

        let canonical_prev = parent.prev_blockhash.to_byte_array().to_vec();
        insert_block(
            &client,
            &canonical_prev,
            &hash_bytes(0x3f_fffe),
            Some(399_999),
            "canonical",
            i64::from(parent.time) - 600,
            None,
        )
        .await?;
        let canonical = hash_bytes(0x40_0000);
        insert_block(
            &client,
            &canonical,
            &canonical_prev,
            Some(400_000),
            "canonical",
            i64::from(parent.time),
            None,
        )
        .await?;
        insert_block(
            &client,
            &hash_bytes(0x40_0001),
            &canonical,
            Some(400_001),
            "canonical",
            i64::from(parent.time) + 600,
            None,
        )
        .await?;
        let tree = project_tree(
            &client,
            Some(&format!("unheighted_anchor={}", display_hash(&parent_hash))),
        )
        .await?;
        let placed = tree
            .nodes
            .iter()
            .find(|node| node.hash == display_hash(&parent_hash))
            .expect("Hathor orphan node present");
        assert_eq!(placed.placement_height, Some(400_000));
        assert!(!placed.placement_approx, "validated height is exact");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn orphans_classifies_real_header_weak_then_strict_and_tracks_revocation() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let parent: Header = deserialize(&hex::decode(BTC_400000_HEADER_HEX)?)?;
        let parent_hash = header_hash_bytes(&parent);

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

    // Revocation husk (`pow_validated = false`) must be excluded from items and counts.
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
        assert_eq!(page.items[0].index, walked.len() as u64);
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
        let empty = fetch_default_orphans_page(&client, 10).await?;
        assert_eq!(empty.total, 0);
        assert!(empty.items.is_empty());
        assert_eq!(orphan_counts(&empty).strict, 0);
        assert_eq!(orphan_counts(&empty).pending, 0);
        assert!(empty.next_cursor.is_none());
        assert!(empty.prev_cursor.is_none());

        let [u1, u2, u3, u4] = seed_paginated_unknowns(&client).await?;

        let expected = vec![
            display_hash(&u4),
            display_hash(&u3),
            display_hash(&u2),
            display_hash(&u1),
        ];

        assert_older_cursor_walk(&client, &expected).await?;

        let all = fetch_default_orphans_page(&client, 100).await?;
        assert_eq!(orphan_hashes(&all), expected);
        assert_eq!(all.total, 4);
        for (offset, item) in all.items.iter().enumerate() {
            assert_eq!(item.index, offset as u64 + 1);
        }
        assert_eq!(orphan_counts(&all).strict, 4);
        assert_eq!(orphan_counts(&all).weak, 0);
        assert_eq!(orphan_counts(&all).excluded, 0);
        assert_eq!(orphan_counts(&all).pending, 0);
        assert!(all.next_cursor.is_none());
        assert!(all.prev_cursor.is_none());

        let oldest = all.items.last().expect("non-empty index").clone();
        let mut newer_walked = Vec::new();
        let mut after_cursor: Option<String> = Some(oldest.cursor.clone());
        while let Some(cursor) = after_cursor.clone() {
            let page =
                fetch_orphans_page(&client, &format!("cursor={cursor}&direction=newer&limit=1"))
                    .await?;
            assert_eq!(page.total, 4);
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].index, 3 - newer_walked.len() as u64);
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

        let peeked = fetch_orphans_page(
            &client,
            &format!("cursor={}&direction=newer&limit=2", oldest.cursor),
        )
        .await?;
        assert_eq!(peeked.items.len(), 2);
        assert_eq!(peeked.items[0].index, 2);
        assert_eq!(peeked.items[1].index, 3);
        assert_eq!(peeked.items[0].primary_hash, all.items[1].primary_hash);

        let missing = display_hash(&hash_bytes(0xffff));
        let empty_page =
            fetch_orphans_page(&client, &format!("anchor_hash={missing}&limit=1")).await?;
        assert!(empty_page.items.is_empty());
        assert_eq!(empty_page.total, 4);
        assert_eq!(orphan_counts(&empty_page).strict, 4);

        assert_older_then_newer_round_trip(&client).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn orphans_filter_by_classification() -> Result<()> {
    crate::run_db_test!(client, {
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
        set_orphan_class(&client, &excluded, "excluded").await?;

        let default = fetch_default_orphans_page(&client, 100).await?;
        assert_eq!(
            orphan_hashes(&default),
            vec![display_hash(&strict), display_hash(&weak)]
        );
        assert_eq!(default.total, 2);
        assert_eq!(default.items[0].index, 1);
        assert_eq!(default.items[1].index, 2);
        assert_eq!(orphan_counts(&default).strict, 1);
        assert_eq!(orphan_counts(&default).weak, 1);
        assert_eq!(orphan_counts(&default).excluded, 1);
        assert_eq!(orphan_counts(&default).pending, 1);
        assert_eq!(orphan_class(&default.items[0]), Some("strict_btc_orphan"));
        assert_eq!(orphan_class(&default.items[1]), Some("weak_btc_orphan"));

        let excluded_page =
            fetch_orphans_page(&client, "classification=excluded&limit=100").await?;
        assert_eq!(orphan_hashes(&excluded_page), vec![display_hash(&excluded)]);
        assert_eq!(excluded_page.total, 1);

        let pending_page = fetch_orphans_page(&client, "classification=pending&limit=100").await?;
        assert_eq!(orphan_hashes(&pending_page), vec![display_hash(&pending)]);
        assert_eq!(pending_page.total, 1);
        assert!(orphan_class(&pending_page.items[0]).is_none());

        let all = fetch_orphans_page(
            &client,
            "classification=strict_btc_orphan,weak_btc_orphan,excluded,pending&limit=100",
        )
        .await?;
        assert_eq!(all.total, 4);
        assert_eq!(all.items.len(), 4);

        Ok::<_, anyhow::Error>(())
    })
}
