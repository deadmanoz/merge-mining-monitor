use anyhow::{Context, Result};
use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};
use mmm_api::projection::{self, ProjectionError};
use mmm_bitcoin_core::BitcoinCoreBlockCoinbase;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::source_registry::{NAMECOIN_SOURCE_CODE, RSK_SOURCE_CODE};
use mmm_read_model::{CoreCanonicalWrite, write_core_canonical};
use mmm_store::get_source_id;
use serde_json::json;
use time::Month;
use tokio_postgres::Client;

use crate::support::scenario::{ChildEvidence, canonical_verdict, capture_child_event};
use crate::support::seed::{
    EventSeed, day_epoch, display_hash, hash_bytes, header_hash_bytes, insert_attestation_proof,
    insert_block, insert_event, insert_pool, set_block_pool,
};
use crate::support::{default_pool_snapshot, header_meeting_bits};

use crate::helpers::format_projection_error;

async fn project_block(client: &Client, hash: &[u8]) -> Result<projection::BlockPayload> {
    projection::block(client, &display_hash(hash))
        .await
        .map_err(format_projection_error)
}

#[tokio::test]
async fn block_projects_direct_near_without_double_counting_sources() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let antpool = insert_pool(&client, "antpool", "AntPool").await?;
        let ts = day_epoch(2026, Month::May, 10);
        let parent = hash_bytes(0x9101);
        for child_height in [100, 101] {
            insert_event(
                &client,
                EventSeed {
                    source_id: namecoin,
                    child_height,
                    child_hash: hash_bytes(0x9200 + child_height as u32),
                    parent_hash: parent.clone(),
                    prev_hash: hash_bytes(0x9100),
                    parent_time: ts,
                    kind: "near",
                    pow_validates_btc_target: false,
                    btc_height: None,
                    pool_id: Some(antpool),
                },
            )
            .await?;
        }

        let payload = project_block(&client, &parent).await?;
        assert_eq!(payload.block.kind, "near");
        assert_eq!(payload.block.source_summary.distinct_sources, 1);
        assert_eq!(
            payload.block.source_summary.sources,
            vec![NAMECOIN_SOURCE_CODE]
        );
        assert_eq!(payload.block.bitcoin_miner_pool.id, None);
        assert_eq!(payload.event_details.len(), 2);
        assert!(payload.proofs.is_empty());

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn block_hydrates_canonical_proof_contributing_events() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let ts = day_epoch(2026, Month::May, 10);

        // Read-model scenario: one canonical-classified capture whose evidence
        // carries the parent coinbase scriptSig. The block row, attestation
        // proof, and contributing-event linkage are reconciler-derived.
        let parent_header = header_meeting_bits(0x207f_ffff, ts as u32, 0x9301);
        let parent = header_hash_bytes(&parent_header);
        let coinbase_script = b"\x00/pooltag/\x00  CORE  ".to_vec();
        let event_id = capture_child_event(
            &mut client,
            ChildEvidence::new(
                "canonical-evt",
                NAMECOIN_SOURCE_CODE,
                110,
                0x94,
                parent_header,
                canonical_verdict(&parent_header, 10),
                ts,
            )
            .with_parent_coinbase_script(coinbase_script),
        )
        .await?;

        let payload = project_block(&client, &parent).await?;
        assert_eq!(payload.block.kind, "canonical");
        assert_eq!(
            payload.block.coinbase_tag.as_deref(),
            Some("/pooltag/ CORE")
        );
        assert_eq!(payload.proofs.len(), 1);
        assert_eq!(payload.event_details.len(), 1);
        assert_eq!(payload.event_details[0].id, event_id);
        assert_eq!(
            payload.proofs[0].evidence["contributing_event_ids"],
            json!([event_id])
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn block_projects_core_coinbase_tag_before_and_after_event_details() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (header, hash, kncminer) =
            seed_core_kncminer_block(&mut client, 331_675, 1_401_234_567, 331_675).await?;
        let payload = project_block(&client, &hash).await?;

        assert_eq!(payload.block.kind, "canonical");
        assert_eq!(payload.block.height, Some(331_675));
        assert_eq!(payload.block.coinbase_tag.as_deref(), Some("KnCMiner"));
        assert_eq!(payload.block.bitcoin_miner_pool.id, Some(kncminer));
        assert!(payload.event_details.is_empty());
        assert!(payload.commitment.is_none());

        let event_coinbase_script = b"\x00/eventpool/\x00  EVENT  ".to_vec();
        let event_id = capture_child_event(
            &mut client,
            ChildEvidence::new(
                "canonical-evt",
                NAMECOIN_SOURCE_CODE,
                111,
                0x95,
                header,
                canonical_verdict(&header, 331_675),
                1_401_234_577,
            )
            .with_parent_coinbase_script(event_coinbase_script),
        )
        .await?;
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        insert_attestation_proof(&client, &hash, namecoin, &[event_id], 1_401_234_577).await?;

        let payload = project_block(&client, &hash).await?;
        assert_eq!(payload.block.kind, "canonical");
        assert_eq!(payload.block.coinbase_tag.as_deref(), Some("KnCMiner"));
        assert_eq!(payload.block.bitcoin_miner_pool.id, Some(kncminer));
        assert_eq!(payload.event_details.len(), 1);
        assert_eq!(payload.event_details[0].id, event_id);
        assert!(payload.commitment.is_some());

        Ok::<_, anyhow::Error>(())
    })
}

async fn seed_core_kncminer_block(
    client: &mut Client,
    height: i32,
    time: u32,
    nonce: u32,
) -> Result<(Header, Vec<u8>, i64)> {
    let (_, pool_ids_by_slug) = default_pool_snapshot(client).await?;
    let kncminer = *pool_ids_by_slug
        .get("kncminer")
        .context("default snapshot missing kncminer")?;
    let header = Header {
        version: Version::ONE,
        prev_blockhash: BlockHash::all_zeros(),
        merkle_root: TxMerkleNode::all_zeros(),
        time,
        bits: CompactTarget::from_consensus(0x1d00_ffff),
        nonce,
    };
    write_core_canonical(
        client,
        CoreCanonicalWrite {
            header: &header,
            height,
            coinbase: Some(BitcoinCoreBlockCoinbase {
                txid: vec![0x33; 32],
                script: b"/KnCMiner/".to_vec(),
                outputs: vec![0],
            }),
        },
        async |_txn| Ok(()),
        "core KnCMiner projection seed",
    )
    .await?
    .cascade(client, &ConfiguredParentClassifier::Disabled)
    .await?;
    let hash = header.block_hash().to_byte_array().to_vec();
    Ok((header, hash, kncminer))
}

#[tokio::test]
async fn block_hydrates_rsk_uncle_sidecar_fields() -> Result<()> {
    crate::run_db_test!(client, {
        let rsk = get_source_id(&client, RSK_SOURCE_CODE).await?;
        let rskpool = insert_pool(&client, "rskpool", "RSKPool").await?;
        let remapped_pool = insert_pool(&client, "remapped-rskpool", "Remapped RSKPool").await?;
        let rsk_miner = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let identity: i64 = client
            .query_one(
                "INSERT INTO pool_identity (pool_id, namespace, identifier) \
                 VALUES ($1, 'rsk_miner_address', $2) \
                 RETURNING id",
                &[&rskpool, &rsk_miner],
            )
            .await?
            .get(0);
        let ts = day_epoch(2026, Month::May, 10);
        let parent = hash_bytes(0xc101);
        let child_hash = hash_bytes(0xc201);
        let event_id = insert_event(
            &client,
            EventSeed {
                source_id: rsk,
                child_height: 810_002,
                child_hash: child_hash.clone(),
                parent_hash: parent.clone(),
                prev_hash: hash_bytes(0xc100),
                parent_time: ts,
                kind: "near",
                pow_validates_btc_target: false,
                btc_height: None,
                pool_id: Some(rskpool),
            },
        )
        .await?;
        client
            .execute(
                "UPDATE merge_mining_event SET pow_validates_child_target = NULL WHERE id = $1",
                &[&event_id],
            )
            .await?;
        let rsk_miner_bytes = hex::decode(rsk_miner)?;
        client
            .execute(
                "INSERT INTO rsk_merge_mining_evidence ( \
                    event_id, rsk_block_hash, rsk_height, is_uncle, uncle_index, \
                    uncle_parent_height, rsk_miner, pool_identity_id, merge_mining_hash, \
                    proof_format \
                 ) VALUES ( \
                    $1, $2, 810002, TRUE, 1, 810005, $3, $4, $5, 'rskj_rpc_opaque' \
                 )",
                &[
                    &event_id,
                    &child_hash,
                    &rsk_miner_bytes,
                    &identity,
                    &hash_bytes(0xc301),
                ],
            )
            .await?;
        insert_rsk_child_pool_attribution(&client, event_id, rsk_miner, rskpool, identity, ts)
            .await?;
        client
            .execute(
                "UPDATE pool_identity SET pool_id = $1 WHERE id = $2",
                &[&remapped_pool, &identity],
            )
            .await?;

        let payload = project_block(&client, &parent).await?;
        assert_eq!(payload.block.coinbase_tag.as_deref(), None);
        let detail = &payload.event_details[0];
        let rsk_detail = detail.rsk.as_ref().expect("rsk sidecar");
        assert_eq!(detail.source, RSK_SOURCE_CODE);
        assert_eq!(detail.pow_validates_child_target, None);
        assert_eq!(rsk_detail.block_hash, hex::encode(&child_hash));
        assert!(rsk_detail.is_uncle);
        assert_eq!(rsk_detail.uncle_index, Some(1));
        assert_eq!(rsk_detail.uncle_referencing_height, Some(810_005));
        assert_eq!(
            rsk_detail
                .pool_identity
                .as_ref()
                .map(|identity| identity.identifier.as_str()),
            Some(rsk_miner)
        );
        assert_identity_backed_child_pool(detail, identity, remapped_pool);

        Ok::<_, anyhow::Error>(())
    })
}

async fn insert_rsk_child_pool_attribution(
    client: &Client,
    event_id: i64,
    rsk_miner: &str,
    pool_id: i64,
    pool_identity_id: i64,
    seen_at: i64,
) -> Result<()> {
    client
        .execute(
            "INSERT INTO event_pool_attribution ( \
                event_id, side, namespace, match_kind, matched_value, pool_id, pool_identity_id, \
                source, confidence, details, first_seen_at, last_seen_at \
             ) VALUES ( \
                $1, 'child_block', 'rsk_miner_address', 'rsk_miner_registry', $2, $3, $4, \
                'rsk_miner_registry', 'high', '{}'::jsonb, $5, $5 \
             )",
            &[&event_id, &rsk_miner, &pool_id, &pool_identity_id, &seen_at],
        )
        .await?;
    Ok(())
}

fn assert_identity_backed_child_pool(
    detail: &mmm_api::projection::EventDetail,
    identity_id: i64,
    expected_pool_id: i64,
) {
    let child_attribution = detail
        .pool_attributions
        .child_block
        .iter()
        .find(|attribution| attribution.namespace == "rsk_miner_address")
        .expect("rsk miner child attribution");
    assert_eq!(
        child_attribution.pool.id,
        Some(expected_pool_id),
        "identity-backed attribution should project the current pool_identity.pool_id"
    );
    assert_eq!(
        child_attribution
            .pool_identity
            .as_ref()
            .map(|identity| identity.id),
        Some(identity_id)
    );
}

#[tokio::test]
async fn block_projects_stale_competition_and_branch() -> Result<()> {
    crate::run_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let antpool = insert_pool(&client, "antpool", "AntPool").await?;
        let f2pool = insert_pool(&client, "f2pool", "F2Pool").await?;
        let ts = day_epoch(2026, Month::May, 10);
        let parent = hash_bytes(0xd100);
        let canonical = hash_bytes(0xd101);
        let stale = hash_bytes(0xd201);
        insert_block(
            &client,
            &parent,
            &hash_bytes(0xd000),
            Some(1),
            "canonical",
            ts,
            None,
        )
        .await?;
        insert_block(
            &client,
            &canonical,
            &parent,
            Some(2),
            "canonical",
            ts + 1,
            None,
        )
        .await?;
        set_block_pool(&client, &canonical, f2pool).await?;
        insert_block(
            &client,
            &stale,
            &parent,
            Some(2),
            "stale",
            ts + 2,
            Some(&canonical),
        )
        .await?;
        set_block_pool(&client, &stale, antpool).await?;
        let event_id = insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 150,
                child_hash: hash_bytes(0xd301),
                parent_hash: stale.clone(),
                prev_hash: parent,
                parent_time: ts + 2,
                kind: "stale",
                pow_validates_btc_target: true,
                btc_height: Some(2),
                pool_id: Some(antpool),
            },
        )
        .await?;
        insert_attestation_proof(&client, &stale, namecoin, &[event_id], ts + 4).await?;

        let payload = project_block(&client, &stale).await?;
        assert_eq!(payload.block.kind, "stale");
        assert_eq!(payload.block.bitcoin_miner_pool.id, Some(antpool));
        let competition = payload.competition.as_ref().expect("competition");
        assert_eq!(competition.stale_hash, display_hash(&stale));
        assert_eq!(competition.canonical_hash, display_hash(&canonical));
        assert_eq!(competition.stale_bitcoin_miner_pool.id, Some(antpool));
        assert_eq!(competition.canonical_bitcoin_miner_pool.id, Some(f2pool));
        let branch = payload.stale_branch.as_ref().expect("stale branch");
        assert_eq!(branch.root_hash, display_hash(&stale));
        assert_eq!(branch.tip_hash, display_hash(&stale));
        assert_eq!(branch.member_hashes, vec![display_hash(&stale)]);
        assert_eq!(
            branch.canonical_competitor_hashes,
            vec![display_hash(&canonical)]
        );
        assert_eq!(branch.depth, 1);
        assert_eq!(branch.position, "root_and_tip");
        assert_eq!(branch.parent_stale_hash, None);
        assert!(branch.child_stale_hashes.is_empty());

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn block_display_miner_infers_child_pool_when_coinbase_unknown_and_drops_on_revoke()
-> Result<()> {
    crate::run_db_test!(client, {
        // A stale Bitcoin block with no recoverable coinbase pool (the RSK-only
        // case): bitcoin_miner_pool stays Unknown while display_miner_pool falls
        // back to the single known child miner pool. Modelled with a Namecoin
        // event (the mechanism is chain-agnostic and needs no RSK sidecar).
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let f2pool = insert_pool(&client, "f2pool", "F2Pool").await?;
        let ts = day_epoch(2026, Month::May, 10);
        let parent = hash_bytes(0xe100);
        let canonical = hash_bytes(0xe101);
        let stale = hash_bytes(0xe201);
        insert_block(
            &client,
            &parent,
            &hash_bytes(0xe000),
            Some(1),
            "canonical",
            ts,
            None,
        )
        .await?;
        insert_block(
            &client,
            &canonical,
            &parent,
            Some(2),
            "canonical",
            ts + 1,
            None,
        )
        .await?;
        insert_block(
            &client,
            &stale,
            &parent,
            Some(2),
            "stale",
            ts + 2,
            Some(&canonical),
        )
        .await?;
        // Deliberately no set_block_pool(&stale): the coinbase miner is unknown.
        let event_id = insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 150,
                child_hash: hash_bytes(0xe301),
                parent_hash: stale.clone(),
                prev_hash: parent,
                parent_time: ts + 2,
                kind: "stale",
                pow_validates_btc_target: true,
                btc_height: Some(2),
                pool_id: Some(f2pool),
            },
        )
        .await?;
        insert_attestation_proof(&client, &stale, namecoin, &[event_id], ts + 4).await?;

        let payload = project_block(&client, &stale).await?;
        assert_eq!(payload.block.kind, "stale");
        assert!(
            !payload.block.bitcoin_miner_pool.known,
            "strict Bitcoin coinbase miner stays Unknown"
        );
        assert_eq!(payload.block.display_miner_pool.id, Some(f2pool));
        assert_eq!(payload.block.display_miner_basis, "child_inferred");

        // Revoking the only child-evidence event drops the inferred label.
        client
            .execute(
                "UPDATE merge_mining_event SET revoked_at = $2, revocation_reason = 'test' \
                 WHERE id = $1",
                &[&event_id, &(ts + 5)],
            )
            .await?;
        let after = project_block(&client, &stale).await?;
        assert!(!after.block.display_miner_pool.known);
        assert_eq!(after.block.display_miner_basis, "unknown");

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn block_returns_not_found_for_valid_hash_without_evidence() -> Result<()> {
    crate::run_db_test!(client, {
        let missing = hash_bytes(0x9501);
        let err = match projection::block(&client, &display_hash(&missing)).await {
            Ok(_) => anyhow::bail!("expected not_found"),
            Err(err) => err,
        };
        assert!(matches!(err, ProjectionError::Api(_)));

        Ok::<_, anyhow::Error>(())
    })
}
