use anyhow::Result;
use mmm_api::projection::{self};
use mmm_api::query::{self, NavigatorTarget};
use mmm_capture::source_registry::{NAMECOIN_SOURCE_CODE, RSK_SOURCE_CODE};
use mmm_store::get_source_id;
use time::Month;
use tokio_postgres::Client;

use crate::support::header_meeting_bits;
use crate::support::scenario::{ChildEvidence, Scenario, stale_verdict};
use crate::support::seed::{
    StaleEventSeed, day_epoch, display_hash, hash_bytes, header_hash_bytes, insert_orphan,
    insert_stale_with_event,
};
use mmm_capture::test_support::header_meeting_bits_with_prev;

use crate::helpers::{format_api_error, format_projection_error, seed_canonical_chain};

async fn fetch_navigator(
    client: &Client,
    target: NavigatorTarget,
    raw_query: Option<&str>,
) -> Result<projection::NavigatorPayload> {
    let query = query::parse_navigator_query(target, raw_query).map_err(format_api_error)?;
    projection::navigator(client, &query)
        .await
        .map_err(format_projection_error)
}

async fn seed_single_block_stale_competition(
    client: &Client,
    namecoin: i64,
    ts: i64,
) -> Result<()> {
    // One-block stale competitions stay in /stales/page, not in the branch index.
    let canonical = seed_canonical_chain(client, 50..=50, 0x0050, 0x004f, ts, None).await?;
    let c050 = canonical[&50].clone();
    let s050 = hash_bytes(0x1050);
    insert_stale_with_event(
        client,
        StaleEventSeed {
            source_id: namecoin,
            hash: s050.clone(),
            prev_hash: hash_bytes(0x004f),
            canonical_competitor_hash: c050.clone(),
            height: 50,
            child_height: 50,
            header_time: ts + 1,
        },
    )
    .await?;

    Ok(())
}

async fn seed_linear_stale_branch(
    client: &Client,
    namecoin: i64,
    ts: i64,
) -> Result<(Vec<u8>, Vec<u8>)> {
    // Older linear branch: s101 -> s102.
    let canonical = seed_canonical_chain(client, 100..=102, 0x0100, 0x00ff, ts + 3, None).await?;
    let c100 = canonical[&100].clone();
    let c101 = canonical[&101].clone();
    let c102 = canonical[&102].clone();
    let s101 = hash_bytes(0x1101);
    let s102 = hash_bytes(0x1102);
    insert_stale_with_event(
        client,
        StaleEventSeed {
            source_id: namecoin,
            hash: s101.clone(),
            prev_hash: c100.clone(),
            canonical_competitor_hash: c101.clone(),
            height: 101,
            child_height: 101,
            header_time: ts + 6,
        },
    )
    .await?;
    insert_stale_with_event(
        client,
        StaleEventSeed {
            source_id: namecoin,
            hash: s102.clone(),
            prev_hash: s101.clone(),
            canonical_competitor_hash: c102.clone(),
            height: 102,
            child_height: 102,
            header_time: ts + 7,
        },
    )
    .await?;

    Ok((s101, s102))
}

async fn seed_forked_stale_branch(client: &Client, namecoin: i64, ts: i64) -> Result<[Vec<u8>; 4]> {
    // Newer forked branch: s201 has two stale children, and one fork extends
    // another height so tip order proves height-desc sorting.
    let canonical = seed_canonical_chain(client, 200..=203, 0x0200, 0x01ff, ts + 10, None).await?;
    let c200 = canonical[&200].clone();
    let c201 = canonical[&201].clone();
    let c202 = canonical[&202].clone();
    let c203 = canonical[&203].clone();
    let s201 = hash_bytes(0x2201);
    let s202a = hash_bytes(0x2202);
    let s202b = hash_bytes(0x2300);
    let s203 = hash_bytes(0x2303);
    insert_stale_with_event(
        client,
        StaleEventSeed {
            source_id: namecoin,
            hash: s201.clone(),
            prev_hash: c200.clone(),
            canonical_competitor_hash: c201.clone(),
            height: 201,
            child_height: 201,
            header_time: ts + 14,
        },
    )
    .await?;
    insert_stale_with_event(
        client,
        StaleEventSeed {
            source_id: namecoin,
            hash: s202a.clone(),
            prev_hash: s201.clone(),
            canonical_competitor_hash: c202.clone(),
            height: 202,
            child_height: 202,
            header_time: ts + 15,
        },
    )
    .await?;
    insert_stale_with_event(
        client,
        StaleEventSeed {
            source_id: namecoin,
            hash: s202b.clone(),
            prev_hash: s201.clone(),
            canonical_competitor_hash: c202.clone(),
            height: 202,
            child_height: 203,
            header_time: ts + 16,
        },
    )
    .await?;
    insert_stale_with_event(
        client,
        StaleEventSeed {
            source_id: namecoin,
            hash: s203.clone(),
            prev_hash: s202b.clone(),
            canonical_competitor_hash: c203.clone(),
            height: 203,
            child_height: 204,
            header_time: ts + 17,
        },
    )
    .await?;

    Ok([s201, s202a, s202b, s203])
}

#[tokio::test]
async fn stale_branches_indexes_multi_block_components_with_forked_tips() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);

        let empty = fetch_navigator(&client, NavigatorTarget::StaleBranch, None).await?;
        assert_eq!(empty.total, 0);
        assert!(empty.items.is_empty());

        seed_single_block_stale_competition(&client, namecoin, ts).await?;
        let (s101, s102) = seed_linear_stale_branch(&client, namecoin, ts).await?;
        let [s201, s202a, s202b, s203] = seed_forked_stale_branch(&client, namecoin, ts).await?;

        let payload = fetch_navigator(&client, NavigatorTarget::StaleBranch, None).await?;
        assert_eq!(payload.total, 2);
        assert_eq!(payload.items.len(), 1);

        let forked = &payload.items[0];
        let forked_branch = forked.branch.as_ref().expect("branch metadata");
        assert_eq!(forked.id, format!("stale-201-{}", display_hash(&s201)));
        assert_eq!(
            forked_branch.branch_id,
            format!("stale-201-{}", display_hash(&s201))
        );
        assert_eq!(forked_branch.root_hash, display_hash(&s201));
        assert_eq!(
            forked_branch.tip_hashes,
            vec![display_hash(&s203), display_hash(&s202a)]
        );
        assert_eq!(forked.position.min, 201);
        assert_eq!(forked.position.max, 203);
        assert_eq!(forked_branch.depth, 4);
        let projection::NavigatorView::TreeWindow {
            target_height,
            tree_from,
            tree_to,
            select_hash,
            center_hash,
        } = forked.view.as_ref().expect("forked branch has navigation")
        else {
            panic!("expected tree_window view");
        };
        assert_eq!(*target_height, 201);
        assert_eq!(*tree_from, 200);
        assert_eq!(*tree_to, 203);
        assert_eq!(select_hash, &forked_branch.root_hash);
        assert_eq!(center_hash, &forked_branch.root_hash);
        assert!(forked.view_error.is_none());

        let anchored_by_inner_member = fetch_navigator(
            &client,
            NavigatorTarget::StaleBranch,
            Some(&format!("anchor_hash={}&limit=1", display_hash(&s202b))),
        )
        .await?;
        assert_eq!(anchored_by_inner_member.total, 2);
        assert_eq!(anchored_by_inner_member.items.len(), 1);
        let anchored = &anchored_by_inner_member.items[0];
        let anchored_branch = anchored.branch.as_ref().expect("branch metadata");
        assert_eq!(anchored_branch.root_hash, display_hash(&s201));
        assert_eq!(
            anchored_branch.tip_hashes,
            vec![display_hash(&s203), display_hash(&s202a)]
        );
        assert_eq!(anchored.position.min, 201);
        assert_eq!(anchored.position.max, 203);
        assert!(anchored_by_inner_member.next_cursor.is_some());
        assert!(anchored_by_inner_member.prev_cursor.is_none());

        let older = fetch_navigator(
            &client,
            NavigatorTarget::StaleBranch,
            Some(&format!(
                "cursor={}&direction=older",
                payload.next_cursor.as_ref().expect("older cursor")
            )),
        )
        .await?;
        let linear = &older.items[0];
        let linear_branch = linear.branch.as_ref().expect("branch metadata");
        assert_eq!(
            linear_branch.branch_id,
            format!("stale-101-{}", display_hash(&s101))
        );
        assert_eq!(linear_branch.root_hash, display_hash(&s101));
        assert_eq!(linear_branch.tip_hashes, vec![display_hash(&s102)]);
        assert_eq!(linear.position.min, 101);
        assert_eq!(linear.position.max, 102);
        assert_eq!(linear_branch.depth, 2);
        let projection::NavigatorView::TreeWindow {
            target_height,
            tree_from,
            tree_to,
            select_hash,
            ..
        } = linear.view.as_ref().expect("linear branch has navigation")
        else {
            panic!("expected tree_window view");
        };
        assert_eq!(*target_height, 101);
        assert_eq!(*tree_from, 100);
        assert_eq!(*tree_to, 102);
        assert_eq!(select_hash, &linear_branch.root_hash);

        Ok::<_, anyhow::Error>(())
    })
}

async fn seed_orphan_branch_components(client: &Client, ts: i64) -> Result<[Vec<u8>; 9]> {
    // Singleton orphan (prev is not an orphan): depth 1, excluded.
    let sg = hash_bytes(0x9005);
    insert_orphan(
        client,
        &sg,
        &hash_bytes(0x9004),
        ts + 5,
        "strict_btc_orphan",
    )
    .await?;

    // Linear strict branch: r_l -> t_l.
    let r_l = hash_bytes(0x9110);
    let t_l = hash_bytes(0x9111);
    insert_orphan(
        client,
        &r_l,
        &hash_bytes(0x910f),
        ts + 10,
        "strict_btc_orphan",
    )
    .await?;
    insert_orphan(client, &t_l, &r_l, ts + 11, "strict_btc_orphan").await?;

    // Forked strict branch: r_f -> { f_a, f_b } (two members share the root, so
    // they have no orphan child and both are tips, sorted by header_time DESC).
    let r_f = hash_bytes(0x9220);
    let f_a = hash_bytes(0x9221);
    let f_b = hash_bytes(0x9222);
    insert_orphan(
        client,
        &r_f,
        &hash_bytes(0x921f),
        ts + 20,
        "strict_btc_orphan",
    )
    .await?;
    insert_orphan(client, &f_a, &r_f, ts + 21, "strict_btc_orphan").await?;
    insert_orphan(client, &f_b, &r_f, ts + 22, "strict_btc_orphan").await?;

    // Weak linear branch: r_w -> t_w (in the default strict+weak filter).
    let r_w = hash_bytes(0x9330);
    let t_w = hash_bytes(0x9331);
    insert_orphan(
        client,
        &r_w,
        &hash_bytes(0x932f),
        ts + 30,
        "weak_btc_orphan",
    )
    .await?;
    insert_orphan(client, &t_w, &r_w, ts + 31, "weak_btc_orphan").await?;

    // Broken branch: r_b -> t_b, but t_b is not PoW-valid, so the component
    // collapses to the singleton r_b and is excluded.
    let r_b = hash_bytes(0x9440);
    let t_b = hash_bytes(0x9441);
    insert_orphan(
        client,
        &r_b,
        &hash_bytes(0x943f),
        ts + 40,
        "strict_btc_orphan",
    )
    .await?;
    insert_orphan(client, &t_b, &r_b, ts + 41, "strict_btc_orphan").await?;
    client
        .execute(
            "UPDATE block SET pow_validated = FALSE WHERE btc_header_hash = $1",
            &[&t_b],
        )
        .await?;

    Ok([sg, r_l, t_l, r_f, f_a, f_b, r_w, t_w, r_b])
}

#[tokio::test]
async fn orphan_branches_indexes_multi_block_components_time_bounded() -> Result<()> {
    crate::run_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);

        // Empty index.
        let empty = fetch_navigator(&client, NavigatorTarget::OrphanBranch, None).await?;
        assert_eq!(empty.total, 0);
        assert!(empty.items.is_empty());

        let [sg, r_l, t_l, r_f, f_a, f_b, r_w, t_w, r_b] =
            seed_orphan_branch_components(&client, ts).await?;

        // Default (strict+weak), newest-first by header_time_max: W(31) > F(22) > L(11).
        let payload = fetch_navigator(&client, NavigatorTarget::OrphanBranch, None).await?;
        assert_eq!(payload.total, 3);
        assert_eq!(payload.items.len(), 1);

        let weak = &payload.items[0];
        let weak_branch = weak.branch.as_ref().expect("branch metadata");
        assert_eq!(
            weak_branch.branch_id,
            format!("orphan-{}", display_hash(&r_w))
        );
        assert_eq!(weak_branch.root_hash, display_hash(&r_w));
        assert_eq!(weak_branch.tip_hashes, vec![display_hash(&t_w)]);
        assert_eq!(weak.position.min, ts + 30);
        assert_eq!(weak.position.max, ts + 31);
        assert_eq!(weak_branch.depth, 2);

        let older = fetch_navigator(
            &client,
            NavigatorTarget::OrphanBranch,
            Some(&format!(
                "cursor={}&direction=older&limit=2",
                payload.next_cursor.as_ref().expect("older cursor")
            )),
        )
        .await?;
        assert_eq!(older.items.len(), 2);
        let forked = &older.items[0];
        let forked_branch = forked.branch.as_ref().expect("branch metadata");
        assert_eq!(
            forked_branch.branch_id,
            format!("orphan-{}", display_hash(&r_f))
        );
        assert_eq!(forked_branch.root_hash, display_hash(&r_f));
        assert_eq!(
            forked_branch.tip_hashes,
            vec![display_hash(&f_b), display_hash(&f_a)]
        );
        assert_eq!(forked.position.min, ts + 20);
        assert_eq!(forked.position.max, ts + 22);
        assert_eq!(forked_branch.depth, 3);

        let anchored_by_member = fetch_navigator(
            &client,
            NavigatorTarget::OrphanBranch,
            Some(&format!("anchor_hash={}&limit=1", display_hash(&f_a))),
        )
        .await?;
        assert_eq!(anchored_by_member.total, 3);
        assert_eq!(anchored_by_member.items.len(), 1);
        let anchored = &anchored_by_member.items[0];
        let anchored_branch = anchored.branch.as_ref().expect("branch metadata");
        assert_eq!(anchored_branch.root_hash, display_hash(&r_f));
        assert_eq!(
            anchored_branch.tip_hashes,
            vec![display_hash(&f_b), display_hash(&f_a)]
        );
        assert_eq!(anchored.position.min, ts + 20);
        assert_eq!(anchored.position.max, ts + 22);
        assert!(anchored_by_member.next_cursor.is_some());
        assert!(anchored_by_member.prev_cursor.is_some());

        let linear = &older.items[1];
        let linear_branch = linear.branch.as_ref().expect("branch metadata");
        assert_eq!(
            linear_branch.branch_id,
            format!("orphan-{}", display_hash(&r_l))
        );
        assert_eq!(linear_branch.tip_hashes, vec![display_hash(&t_l)]);
        assert_eq!(linear_branch.depth, 2);

        // The singleton and the PoW-invalid-collapsed branch never appear.
        let roots: Vec<&str> = payload
            .items
            .iter()
            .chain(older.items.iter())
            .filter_map(|item| item.branch.as_ref())
            .map(|b| b.root_hash.as_str())
            .collect();
        assert!(!roots.contains(&display_hash(&sg).as_str()));
        assert!(!roots.contains(&display_hash(&r_b).as_str()));

        // Strict-only classification drops the weak branch.
        let strict = fetch_navigator(
            &client,
            NavigatorTarget::OrphanBranch,
            Some("classification=strict_btc_orphan&limit=10"),
        )
        .await?;
        assert_eq!(strict.total, 2);
        assert_eq!(strict.items.len(), 2);
        assert_eq!(
            strict.items[0].branch.as_ref().unwrap().root_hash,
            display_hash(&r_f)
        );
        assert_eq!(
            strict.items[1].branch.as_ref().unwrap().root_hash,
            display_hash(&r_l)
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn stale_branches_indexes_production_reconciled_stale_chain() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);

        // Read-model scenario: canonical spine A100 <- A101 <- A102 written
        // through the Core backbone mutation path, and a two-block stale
        // branch B101 <- B102 forking off A100, captured as child evidence
        // from two different sources with stale verdicts naming their
        // canonical competitors. block rows are reconciler-derived, never
        // hand-inserted.
        let a100 = header_meeting_bits(0x207f_ffff, ts as u32, 0xA100);
        let a101 =
            header_meeting_bits_with_prev(0x207f_ffff, ts as u32 + 1, 0xA101, a100.block_hash());
        let a102 =
            header_meeting_bits_with_prev(0x207f_ffff, ts as u32 + 2, 0xA102, a101.block_hash());
        let b101 =
            header_meeting_bits_with_prev(0x207f_ffff, ts as u32 + 3, 0xB101, a100.block_hash());
        let b102 =
            header_meeting_bits_with_prev(0x207f_ffff, ts as u32 + 4, 0xB102, b101.block_hash());

        Scenario::new()
            .core_canonical(a100, 100)
            .core_canonical(a101, 101)
            .core_canonical(a102, 102)
            .child_event(ChildEvidence::new(
                "stale-101",
                NAMECOIN_SOURCE_CODE,
                501_001,
                0x51,
                b101,
                stale_verdict(&b101, 101, header_hash_bytes(&a101)),
                ts + 5,
            ))
            .child_event(ChildEvidence::new(
                "stale-102",
                RSK_SOURCE_CODE,
                6_101_000,
                0x52,
                b102,
                stale_verdict(&b102, 102, header_hash_bytes(&a102)),
                ts + 6,
            ))
            .apply(&mut client)
            .await?;

        let payload = fetch_navigator(&client, NavigatorTarget::StaleBranch, None).await?;
        assert_eq!(payload.total, 1);
        let item = &payload.items[0];
        let branch = item.branch.as_ref().expect("branch metadata");
        let b101_hash = header_hash_bytes(&b101);
        assert_eq!(
            branch.branch_id,
            format!("stale-101-{}", display_hash(&b101_hash))
        );
        assert_eq!(branch.root_hash, display_hash(&b101_hash));
        assert_eq!(
            branch.tip_hashes,
            vec![display_hash(&header_hash_bytes(&b102))]
        );
        assert_eq!(item.position.min, 101);
        assert_eq!(item.position.max, 102);
        assert_eq!(branch.depth, 2);

        Ok::<_, anyhow::Error>(())
    })
}
