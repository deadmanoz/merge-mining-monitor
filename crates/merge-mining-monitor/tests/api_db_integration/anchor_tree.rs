use anyhow::Result;
use mmm_api::projection::{TreeEdge, TreePayload};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_store::get_source_id;

use crate::support::seed::{
    EventSeed, display_hash, hash_bytes, insert_block, insert_event, insert_orphan,
};

use crate::helpers::{
    classify_all_unknowns_strict, expect_tree_api_error, insert_unknown_block, project_tree,
    seed_canonical_chain, set_orphan_class,
};

async fn project_unheighted_anchor_tree(
    client: &tokio_postgres::Client,
    anchor: &[u8],
) -> Result<TreePayload> {
    project_tree(
        client,
        Some(&format!("unheighted_anchor={}", display_hash(anchor))),
    )
    .await
}

fn orphan_edges(payload: &TreePayload) -> Vec<&TreeEdge> {
    payload
        .edges
        .iter()
        .filter(|edge| matches!(edge.edge_kind, "orphan" | "orphan_approx"))
        .collect()
}

#[tokio::test]
async fn anchor_tree_orphan_filter_excludes_pending_and_excluded() -> Result<()> {
    crate::run_db_test!(client, {
        // The anchor (strict) and a weak neighbor are in the default strict+weak
        // view; an excluded and a pending neighbor are NOT and must be filtered
        // out of the anchor strip.
        let pending = hash_bytes(0x0a01);
        let excluded = hash_bytes(0x0a02);
        let anchor = hash_bytes(0x0a03);
        let weak = hash_bytes(0x0a04);
        for (hash, prev, time) in [
            (&pending, hash_bytes(0x0b01), 1000),
            (&excluded, hash_bytes(0x0b02), 2000),
            (&anchor, hash_bytes(0x0b03), 3000),
            (&weak, hash_bytes(0x0b04), 4000),
        ] {
            insert_unknown_block(&client, hash, &prev, time).await?;
        }
        set_orphan_class(&client, &anchor, "strict_btc_orphan").await?;
        set_orphan_class(&client, &weak, "weak_btc_orphan").await?;
        set_orphan_class(&client, &excluded, "btc_stale_excluded").await?;
        // `pending` keeps its NULL class.

        let payload = project_unheighted_anchor_tree(&client, &anchor).await?;

        // Default strict+weak: only the anchor (strict) and the weak neighbor,
        // ascending by header time; the excluded and pending neighbors are gone.
        assert_eq!(
            payload
                .nodes
                .iter()
                .map(|n| n.hash.clone())
                .collect::<Vec<_>>(),
            vec![display_hash(&anchor), display_hash(&weak)],
            "anchor strip excludes pending and excluded neighbors"
        );
        assert_eq!(
            payload.nodes[0].btc_orphan_class.as_deref(),
            Some("strict_btc_orphan")
        );
        assert_eq!(
            payload.nodes[1].btc_orphan_class.as_deref(),
            Some("weak_btc_orphan")
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn anchor_tree_not_found_for_absent_or_non_unknown_anchor() -> Result<()> {
    crate::run_db_test!(client, {
        let canonical = hash_bytes(0x0c01);
        insert_block(
            &client,
            &canonical,
            &hash_bytes(0x0c00),
            Some(7),
            "canonical",
            1000,
            None,
        )
        .await?;
        let husk = hash_bytes(0x0a02);
        insert_unknown_block(&client, &husk, &hash_bytes(0x0b02), 2100).await?;
        client
            .execute(
                "UPDATE block SET pow_validated = false WHERE btc_header_hash = $1",
                &[&husk],
            )
            .await?;
        let unknown = hash_bytes(0x0a01);
        insert_unknown_block(&client, &unknown, &hash_bytes(0x0b01), 2000).await?;
        // The genuine unknown is classified so it resolves under the default
        // strict+weak anchor filter; the husk stays pending (pow_validated=false).
        classify_all_unknowns_strict(&client).await?;

        // A canonical, a revocation husk, and a never-inserted hash are all not
        // PoW-valid unknowns, so an anchor on any of them is `not_found`.
        for absent in [
            display_hash(&canonical),
            display_hash(&husk),
            display_hash(&hash_bytes(0x0f99)),
        ] {
            let api =
                expect_tree_api_error(&client, &format!("unheighted_anchor={absent}")).await?;
            assert_eq!(
                api.code(),
                "not_found",
                "anchor {absent} should be not_found"
            );
        }

        // A real PoW-valid unknown anchor resolves and is placed as a fork in the
        // canonical window: the inserted canonical at height 7 is the nearest-time
        // placement target, so the orphan node is present alongside spine context.
        let payload = project_unheighted_anchor_tree(&client, &unknown).await?;
        assert!(
            payload
                .nodes
                .iter()
                .any(|node| node.hash == display_hash(&unknown)
                    && node.kind == "unknown"
                    && node.placement_height.is_some()),
            "the resolved anchor orphan is placed in the tree"
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn anchor_tree_renders_whole_orphan_component_with_member_edges_and_branch() -> Result<()> {
    crate::run_db_test!(client, {
        // Canonical context so the placement window has a spine. Times are chosen so
        // the nearest-time canonical fallback places the component root at height 101.
        seed_canonical_chain(&client, 100..=103, 0x0c64, 0x0c63, 1000, None).await?;

        // A 2-member orphan component: root -> tip. root.prev is absent; tip.prev=root.
        let root = hash_bytes(0x0a10);
        let tip = hash_bytes(0x0a11);
        insert_orphan(
            &client,
            &root,
            &hash_bytes(0x0b10),
            1001,
            "strict_btc_orphan",
        )
        .await?;
        insert_orphan(&client, &tip, &root, 1002, "strict_btc_orphan").await?;

        // Anchoring on the TIP must render the WHOLE component, not just the tip.
        let payload = project_unheighted_anchor_tree(&client, &tip).await?;

        // Both members are placed unknown nodes carrying placement_height and, since
        // the component is depth >= 2, the orphan branch id.
        let branch_id = format!("orphan-{}", display_hash(&root));
        let member_count = payload
            .nodes
            .iter()
            .filter(|node| node.kind == "unknown")
            .count();
        assert_eq!(member_count, 2, "both component members are rendered");
        for member in [&root, &tip] {
            let node = payload
                .nodes
                .iter()
                .find(|node| node.hash == display_hash(member))
                .expect("member node present");
            assert_eq!(node.kind, "unknown");
            assert!(
                node.placement_height.is_some(),
                "member carries a placement_height"
            );
            assert_eq!(
                node.branch.as_ref().map(|branch| branch.branch_id.as_str()),
                Some(branch_id.as_str()),
                "member carries the orphan branch id"
            );
        }

        // The proven prev_hash link root -> tip is a SOLID `orphan` edge.
        assert!(
            payload
                .edges
                .iter()
                .any(|edge| edge.from_hash == display_hash(&root)
                    && edge.to_hash == display_hash(&tip)
                    && edge.edge_kind == "orphan"),
            "the proven member-to-member link is a solid orphan edge"
        );
        // The root attaches to the canonical spine (solid orphan or dashed
        // orphan_approx), so the component is never left dangling.
        assert!(
            payload
                .edges
                .iter()
                .any(|edge| edge.to_hash == display_hash(&root)
                    && (edge.edge_kind == "orphan" || edge.edge_kind == "orphan_approx")),
            "the root attaches to the canonical spine"
        );

        // The /tree branches[] array carries the orphan branch (placement-height
        // bounds, no competition).
        let branch = payload
            .branches
            .iter()
            .find(|branch| branch.branch_id == branch_id)
            .expect("orphan branch present in branches[]");
        assert_eq!(branch.depth, 2);
        assert_eq!(branch.root_hash, display_hash(&root));
        assert!(
            branch.canonical_competitor_hashes.is_empty(),
            "orphan branches have no canonical competitors"
        );
        assert!(branch.member_hashes.contains(&display_hash(&tip)));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn anchor_tree_places_weak_orphan_in_canonical_window() -> Result<()> {
    crate::run_db_test!(client, {
        // A weak orphan places by its timestamp-selected DAA-epoch height (the same
        // committed table the weak classifier uses), dangling off the nearest
        // in-window canonical via the approximate edge.
        let orphan_time = 1_240_000_000i64;
        let ph = mmm_capture::nbits_table::table()
            .epoch_height_for_time(orphan_time)
            .expect("a covered header time has an epoch placement height");
        assert!(
            ph > 16,
            "epoch placement height leaves room for the ±16 window"
        );

        seed_canonical_chain(
            &client,
            (ph - 1)..=(ph + 1),
            (ph - 1) as u32,
            (ph - 2) as u32,
            1500 + i64::from(ph - 1),
            None,
        )
        .await?;

        let orphan = hash_bytes(0x0051_5200);
        insert_unknown_block(&client, &orphan, &hash_bytes(0x0000_beef), orphan_time).await?;
        set_orphan_class(&client, &orphan, "weak_btc_orphan").await?;

        let payload = project_unheighted_anchor_tree(&client, &orphan).await?;

        let placed = payload
            .nodes
            .iter()
            .find(|node| node.hash == display_hash(&orphan))
            .expect("orphan node present");
        assert_eq!(placed.kind, "unknown");
        assert!(placed.height.is_none(), "orphan keeps a null btc_height");
        assert_eq!(placed.placement_height, Some(ph));
        assert!(placed.placement_approx, "weak placement is approximate");

        assert!(
            payload
                .nodes
                .iter()
                .any(|node| node.kind == "canonical" && node.height == Some(ph)),
            "the canonical spine is present in the placement window"
        );
        assert_eq!(payload.window.btc_height_min, Some((ph - 16).max(0)));
        assert_eq!(payload.window.btc_height_max, Some(ph + 16));

        let fork_edges = orphan_edges(&payload);
        assert_eq!(fork_edges.len(), 1, "exactly one fork edge");
        assert_eq!(fork_edges[0].edge_kind, "orphan_approx");
        assert_eq!(fork_edges[0].to_hash, display_hash(&orphan));

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn anchor_tree_places_strict_orphan_with_verified_solid_edge() -> Result<()> {
    crate::run_db_test!(client, {
        // A strict orphan places by its validated BIP34 coinbase height with a solid
        // `orphan` edge to the canonical at placement_height - 1, because its stored
        // prev_hash matches that block.
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ph = 227_940i32; // >= the BIP34 floor (227_931)
        let prev = hash_bytes(ph as u32 - 1); // the canonical at ph-1
        seed_canonical_chain(
            &client,
            (ph - 1)..=(ph + 1),
            (ph - 1) as u32,
            (ph - 2) as u32,
            1_300_000_000 + i64::from(ph - 1),
            None,
        )
        .await?;

        let orphan = hash_bytes(0x0051_5200);
        insert_unknown_block(&client, &orphan, &prev, 1_300_000_500).await?;
        set_orphan_class(&client, &orphan, "strict_btc_orphan").await?;
        insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 9,
                child_hash: hash_bytes(0x0c01),
                parent_hash: orphan.clone(),
                prev_hash: prev.clone(),
                parent_time: 1_300_000_500,
                kind: "unknown",
                pow_validates_btc_target: true,
                btc_height: None,
                pool_id: None,
            },
        )
        .await?;
        // A BIP34 coinbase scriptSig: a 3-byte little-endian push of height 227_940.
        let coinbase = vec![0x03u8, 0x64, 0x7a, 0x03];
        assert_eq!(
            mmm_capture::auxpow::parse_bip34_height(&coinbase),
            Some(ph),
            "premise: the crafted coinbase decodes to the strict height"
        );
        client
            .execute(
                "UPDATE merge_mining_event SET btc_parent_coinbase_script = $1 \
                 WHERE btc_parent_header_hash = $2",
                &[&coinbase, &orphan],
            )
            .await?;

        let payload = project_unheighted_anchor_tree(&client, &orphan).await?;

        let placed = payload
            .nodes
            .iter()
            .find(|node| node.hash == display_hash(&orphan))
            .expect("orphan node present");
        assert_eq!(placed.placement_height, Some(ph));
        assert!(!placed.placement_approx, "strict placement is exact");

        let fork_edges = orphan_edges(&payload);
        assert_eq!(fork_edges.len(), 1);
        assert_eq!(
            fork_edges[0].edge_kind, "orphan",
            "a verified predecessor gets the solid edge"
        );
        assert_eq!(
            fork_edges[0].from_hash,
            display_hash(&prev),
            "the solid edge runs from the ph-1 canonical"
        );
        assert_eq!(fork_edges[0].to_hash, display_hash(&orphan));

        Ok::<_, anyhow::Error>(())
    })
}
