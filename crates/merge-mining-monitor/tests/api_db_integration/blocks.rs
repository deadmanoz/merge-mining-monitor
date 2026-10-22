use anyhow::{Context, Result};
use bitcoin::block::{Header, Version};
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, CompactTarget, TxMerkleNode};
use mmm_api::projection::{self, ProjectionError, ProtocolCommitments, TreeProtocolCommitment};
use mmm_bitcoin_core::BitcoinCoreBlockCoinbase;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::capture::{RSK_PROOF_FORMAT_OPAQUE, RskEvidencePayload};
use mmm_capture::protocol_commitment::DECODER_VERSION;
use mmm_capture::source_registry::{NAMECOIN_SOURCE_CODE, RSK_SOURCE_CODE};
use mmm_producers::seed_protocol_account_identities;
use mmm_read_model::{
    CoreCanonicalWrite, ReconcileReadModelConfig, run_reconcile_read_model, write_core_canonical,
};
use mmm_store::{
    ChildChainHeadOutcome, ChildChainHeadRecord, CurrentBlockParent, EvidenceMarker, get_source_id,
    record_child_chain_block, record_rsk_observation,
};
use serde_json::json;
use time::Month;
use tokio_postgres::Client;

use crate::support::scenario::{ChildEvidence, Scenario, canonical_verdict, capture_child_event};
use crate::support::seed::{
    EventSeed, day_epoch, display_hash, hash_bytes, header_hash_bytes, insert_attestation_proof,
    insert_block, insert_event, insert_near_observation, insert_pool, insert_pool_identity,
    set_block_pool,
};
use crate::support::{
    coinbase_outputs_fixture, default_pool_snapshot, header_meeting_bits, secpool_blocks,
};

use crate::helpers::{format_projection_error, project_tree};

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
            insert_near_observation(
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
        // The parent coinbase names AntPool; display still infers no miner for a near.
        assert_eq!(payload.block.bitcoin_miner_pool.id, Some(antpool));
        assert_eq!(payload.block.display_miner_pool.id, None);
        // A near has no events: one observation per child block, one distinct source.
        assert!(payload.event_details.is_empty());
        assert_eq!(payload.observations.len(), 2);
        assert!(payload.proofs.is_empty());

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn block_hydrates_canonical_proof_contributing_events() -> Result<()> {
    crate::run_mut_db_test!(client, {
        crate::support::seed_synthetic_fixture_history(&client).await?;
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

        // The seeded header misses its own target, so a capture would record a near; the
        // projection under test needs evidence, so the event is seeded directly.
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let event_id = insert_event(
            &client,
            EventSeed {
                source_id: namecoin,
                child_height: 111,
                child_hash: hash_bytes(0x95),
                parent_hash: hash.clone(),
                prev_hash: header.prev_blockhash.to_byte_array().to_vec(),
                parent_time: 1_401_234_577,
                kind: "canonical",
                pow_validates_btc_target: true,
                btc_height: Some(331_675),
                pool_id: None,
            },
        )
        .await?;
        client
            .execute(
                "UPDATE merge_mining_event SET btc_parent_coinbase_script = $2 WHERE id = $1",
                &[&event_id, &b"\x00/eventpool/\x00  EVENT  ".to_vec()],
            )
            .await?;
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
    let header = synthetic_core_header(time, nonce);
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
async fn block_hydrates_an_rsk_uncle_near_observation() -> Result<()> {
    crate::run_db_test!(client, {
        let rsk = get_source_id(&client, RSK_SOURCE_CODE).await?;
        let rskpool = insert_pool(&client, "rskpool", "RSKPool").await?;
        let remapped_pool = insert_pool(&client, "remapped-rskpool", "Remapped RSKPool").await?;
        let rsk_miner = "abcdefabcdefabcdefabcdefabcdefabcdefabcd";
        let identity =
            insert_pool_identity(&client, rskpool, "rsk_miner_address", rsk_miner).await?;
        let ts = day_epoch(2026, Month::May, 10);
        let parent = hash_bytes(0xc101);
        let child_hash = hash_bytes(0xc201);
        let observation_id = insert_near_observation(
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
                "UPDATE child_block_observation SET pow_validates_child_target = NULL \
                 WHERE id = $1",
                &[&observation_id],
            )
            .await?;
        let rsk_miner_bytes = hex::decode(rsk_miner)?;
        // The RSK capture writer's facts, so the reader sees what capture records.
        record_rsk_observation(
            &client,
            rsk,
            &RskEvidencePayload {
                rsk_block_hash: child_hash.clone(),
                rsk_height: 810_002,
                is_uncle: true,
                uncle_index: Some(1),
                uncle_parent_height: Some(810_005),
                rsk_miner: rsk_miner_bytes,
                merge_mining_hash: hash_bytes(0xc301),
                merkle_proof: None,
                coinbase_tail: None,
                proof_format: RSK_PROOF_FORMAT_OPAQUE,
                child_difficulty_reported: None,
            },
        )
        .await?;
        client
            .execute(
                "UPDATE pool_identity SET pool_id = $1 WHERE id = $2",
                &[&remapped_pool, &identity],
            )
            .await?;

        let payload = project_block(&client, &parent).await?;
        assert_eq!(payload.block.coinbase_tag.as_deref(), None);
        assert!(
            payload.event_details.is_empty(),
            "a near page has no event details"
        );
        let observation = &payload.observations[0];
        assert_eq!(observation.source, RSK_SOURCE_CODE);
        assert_eq!(observation.pow_validates_child_target, None);
        assert_eq!(
            observation.child_block_hash.as_deref(),
            Some(hex::encode(&child_hash).as_str())
        );
        assert_eq!(observation.child_chain_role.as_deref(), Some("uncle"));
        assert_eq!(observation.uncle_index, Some(1));
        assert_eq!(observation.uncle_parent_height, Some(810_005));
        let miner = observation
            .child_identities
            .iter()
            .find(|identity| identity.namespace == "rsk_miner_address")
            .expect("rsk miner identity");
        assert_eq!(miner.identifier, rsk_miner);
        assert_eq!(
            miner
                .pool_identity
                .as_ref()
                .map(|pool_identity| pool_identity.id),
            Some(identity)
        );
        assert_eq!(
            miner.pool.id,
            Some(remapped_pool),
            "the identity projects the current pool_identity.pool_id"
        );

        Ok::<_, anyhow::Error>(())
    })
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
        assert!(
            matches!(&err, ProjectionError::Api(api) if api.code() == "not_found"),
            "expected not_found, got {err:?}"
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn block_projects_child_displacement_on_near_observations() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let ts = day_epoch(2026, Month::May, 10);
        // Two Namecoin blocks at one child height, each committing to its own
        // Bitcoin parent, so each parent's block payload carries one event.
        let displaced_child = hash_bytes(0x9a01);
        let current_child = hash_bytes(0x9a02);
        let displaced_parent = hash_bytes(0x9b01);
        let current_parent = hash_bytes(0x9b02);
        for (child_hash, parent_hash) in [
            (displaced_child.clone(), displaced_parent.clone()),
            (current_child.clone(), current_parent.clone()),
        ] {
            insert_near_observation(
                &client,
                EventSeed {
                    source_id: namecoin,
                    child_height: 120,
                    child_hash,
                    parent_hash,
                    prev_hash: hash_bytes(0x9b00),
                    parent_time: ts,
                    kind: "near",
                    pow_validates_btc_target: false,
                    btc_height: None,
                    pool_id: None,
                },
            )
            .await?;
        }
        let txn = client.transaction().await?;
        record_child_chain_block(
            &txn,
            namecoin,
            120,
            ChildChainHeadRecord {
                block_hash: &current_child,
                parent: CurrentBlockParent::Known(&current_parent),
                outcome: ChildChainHeadOutcome::Captured,
                evidence: EvidenceMarker::None,
                observed_at: ts + 60,
                core_cache_generation: None,
            },
        )
        .await?;
        txn.commit().await?;

        let displaced = project_block(&client, &displaced_parent).await?;
        assert_eq!(displaced.observations.len(), 1);
        assert_eq!(displaced.observations[0].child_displaced_at, Some(ts + 60));
        assert_eq!(
            displaced.observations[0].child_displaced_by.as_deref(),
            Some(display_hash(&current_child).as_str())
        );
        // Displacement is child-side only: the parent still projects as before.
        assert_eq!(displaced.block.kind, "near");
        assert_eq!(displaced.block.source_summary.distinct_sources, 1);

        let current = project_block(&client, &current_parent).await?;
        assert_eq!(current.observations.len(), 1);
        assert_eq!(current.observations[0].child_displaced_at, None);
        assert_eq!(current.observations[0].child_displaced_by, None);

        Ok::<_, anyhow::Error>(())
    })
}

fn synthetic_core_header(time: u32, nonce: u32) -> Header {
    Header {
        version: Version::ONE,
        prev_blockhash: BlockHash::all_zeros(),
        merkle_root: TxMerkleNode::all_zeros(),
        time,
        bits: CompactTarget::from_consensus(0x1d00_ffff),
        nonce,
    }
}

async fn protocol_commitments(client: &Client, hash: &[u8]) -> Result<Option<ProtocolCommitments>> {
    Ok(project_block(client, hash).await?.protocol_commitments)
}

async fn tree_protocol_commitments(
    client: &Client,
    height: i32,
) -> Result<Option<Vec<TreeProtocolCommitment>>> {
    let payload = project_tree(client, Some(&format!("at_height={height}"))).await?;
    let [node] = payload.nodes.as_slice() else {
        anyhow::bail!("expected one node at height {height}");
    };
    Ok(node.protocol_commitments.clone())
}

/// The tree node lists the block page's declarations: protocol, vout, declared account and pool.
async fn assert_tree_matches_block(
    client: &Client,
    height: i32,
    section: &ProtocolCommitments,
) -> Result<()> {
    let tree = tree_protocol_commitments(client, height)
        .await?
        .context("derived for the running decoder")?;
    let tree: Vec<_> = tree
        .iter()
        .map(|entry| {
            let account = entry.declared_account.as_deref();
            (
                entry.protocol.as_str(),
                entry.vout,
                account,
                entry.declared_account_pool.slug.as_deref(),
            )
        })
        .collect();
    let block: Vec<_> = section
        .entries
        .iter()
        .map(|entry| {
            let account = entry
                .core_reward_address
                .as_deref()
                .or(entry.exsat_account.as_deref());
            (
                entry.protocol.as_str(),
                entry.vout,
                account,
                entry.declared_account_pool.slug.as_deref(),
            )
        })
        .collect();
    assert_eq!(tree, block);
    Ok(())
}

#[tokio::test]
async fn block_and_tree_serve_protocol_commitments_once_derived_for_the_decoder() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (_, mut pool_ids) = default_pool_snapshot(&client).await?;
        seed_protocol_account_identities(&client, &mut pool_ids).await?;
        let [_, secpool] = secpool_blocks()?;
        Scenario::new()
            .core_canonical_with_coinbase(secpool.header, secpool.height, secpool.coinbase.clone())
            .apply(&mut client)
            .await?;
        assert!(
            protocol_commitments(&client, &secpool.hash)
                .await?
                .is_none(),
            "null until a rebuild records the running decoder's version"
        );
        assert!(
            tree_protocol_commitments(&client, secpool.height)
                .await?
                .is_none()
        );
        let nested = synthetic_core_header(1_722_000_000, 853_776);
        Scenario::new()
            .core_canonical_with_coinbase(
                nested,
                853_776,
                BitcoinCoreBlockCoinbase {
                    txid: vec![0x34; 32],
                    script: b"/Braiins/".to_vec(),
                    outputs: coinbase_outputs_fixture(853_776)?,
                },
            )
            .apply(&mut client)
            .await?;
        client
            .execute(
                "UPDATE read_model_invariant SET protocol_commitments_decoder_version = $1",
                &[&DECODER_VERSION],
            )
            .await?;

        let section = protocol_commitments(&client, &secpool.hash)
            .await?
            .context("derived for the running decoder")?;
        assert_eq!(section.outputs, "decoded");
        let entries: Vec<_> = section
            .entries
            .iter()
            .map(|entry| {
                (
                    entry.vout,
                    entry.protocol.as_str(),
                    entry.upstream_selected,
                    entry.core_reward_address.as_deref(),
                    entry.exsat_account.as_deref(),
                    entry.declared_account_pool.slug.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            entries,
            [
                (
                    3,
                    "core",
                    Some(true),
                    Some("0x942c01262fa046fdb8ba0dfce3753405c74aed0e"),
                    None,
                    Some("secpool")
                ),
                (
                    4,
                    "exsat",
                    None,
                    None,
                    Some("antpoole.sat"),
                    Some("antpool")
                ),
            ]
        );
        assert_tree_matches_block(&client, secpool.height, &section).await?;
        let section = protocol_commitments(&client, &nested.block_hash().to_byte_array())
            .await?
            .context("derived for the running decoder")?;
        let entry = &section.entries[0];
        assert_eq!(
            (
                entry.decode_status.as_str(),
                entry.malformed_reason.as_deref(),
                entry.declared_account_pool.known,
                entry.evidence_source.as_str(),
            ),
            ("malformed", Some("nested_header"), false, "bitcoin_core")
        );
        assert_tree_matches_block(&client, 853_776, &section).await?;

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn block_protocol_commitments_report_the_state_of_stored_outputs() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let namecoin = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let f2pool_outputs = coinbase_outputs_fixture(868_400)?;
        let seed = |child_height: i32, parent: u32, kind: &'static str| EventSeed {
            source_id: namecoin,
            child_height,
            child_hash: hash_bytes(0x9700 + parent),
            parent_hash: hash_bytes(parent),
            prev_hash: hash_bytes(0x9600),
            parent_time: 1_730_000_000,
            kind,
            pow_validates_btc_target: kind != "near",
            btc_height: None,
            pool_id: None,
        };
        let near = hash_bytes(0x9601);
        insert_near_observation(&client, seed(301, 0x9601, "near")).await?;
        let unreconciled = hash_bytes(0x9602);
        let event_id = insert_event(&client, seed(302, 0x9602, "unknown")).await?;
        let (no_outputs, corrupt) = (hash_bytes(0x9603), hash_bytes(0x9604));
        for hash in [&no_outputs, &corrupt] {
            insert_block(
                &client,
                hash,
                &hash_bytes(0x9600),
                Some(868_400),
                "canonical",
                1_730_000_000,
                None,
            )
            .await?;
        }
        client
            .execute(
                "UPDATE child_block_observation SET btc_parent_coinbase_outputs = $2 \
                  WHERE observation_kind = 'near' AND btc_parent_header_hash = $1",
                &[&near, &f2pool_outputs],
            )
            .await?;
        client
            .execute(
                "UPDATE merge_mining_event SET btc_parent_coinbase_outputs = $2 WHERE id = $1",
                &[&event_id, &f2pool_outputs],
            )
            .await?;
        client
            .execute(
                "UPDATE block SET btc_coinbase_outputs = '\\xff' WHERE btc_header_hash = $1",
                &[&corrupt],
            )
            .await?;
        run_reconcile_read_model(
            &mut client,
            &ConfiguredParentClassifier::Disabled,
            ReconcileReadModelConfig {
                rebuild_protocol_commitments: true,
                ..ReconcileReadModelConfig::default()
            },
        )
        .await?;

        let near_section = protocol_commitments(&client, &near)
            .await?
            .context("near")?;
        assert_eq!(near_section.outputs, "decoded");
        assert_eq!(
            near_section
                .entries
                .iter()
                .map(|entry| (entry.vout, entry.evidence_source.as_str()))
                .collect::<Vec<_>>(),
            [(3, "near_observation"), (4, "near_observation")]
        );
        for (hash, outputs) in [(&no_outputs, "unavailable"), (&corrupt, "undecodable")] {
            let section = protocol_commitments(&client, hash)
                .await?
                .context(outputs)?;
            assert_eq!((section.outputs, section.entries.len()), (outputs, 0));
        }
        assert!(
            protocol_commitments(&client, &unreconciled)
                .await?
                .is_none(),
            "a parent without a block row has no derived rows yet"
        );

        Ok::<_, anyhow::Error>(())
    })
}
