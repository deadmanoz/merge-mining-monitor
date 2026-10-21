use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash as _;
use bitcoin::{Amount, ScriptBuf, Transaction, TxOut};
use mmm_bitcoin_core::{BitcoinCoreBlockCoinbase, ConfiguredParentClassifier};
use mmm_capture::capture::ClassificationProof;
use mmm_capture::identity_registry::IdentityRegistryEntry;
use mmm_capture::protocol_commitment::{DECODER_VERSION, EXSAT_ACCOUNT_NAMESPACE};
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_producers::seed_protocol_account_identities;
use mmm_read_model::{CaptureOutcome, ReconcileReadModelConfig, run_reconcile_read_model};
use mmm_store::{CaptureWrite, reconcile_identity_registry};
use tokio_postgres::Client;

use crate::support::scenario::{
    ChildEvidence, Scenario, canonical_verdict, revoke_event, unknown_verdict,
};
use crate::support::seed::{hash_bytes, pool_id_for_slug};
use crate::support::{
    NamecoinEventFixture, capture_test_outcome, default_pool_snapshot, namecoin_event_payload,
    parse_auxpow_fixture,
};

/// A real Core block: its header and the coinbase columns Core sync stores.
struct CoreBlock {
    height: i32,
    hash: Vec<u8>,
    header: Header,
    coinbase: BitcoinCoreBlockCoinbase,
}

/// SecPool blocks 895,743 (CORE only) and 895,755 (CORE and exSat `antpoole.sat`).
fn secpool_blocks() -> Result<[CoreBlock; 2]> {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/bitcoin/secpool-exsat-transition.json"
    )))?;
    let block = |index: usize| -> Result<CoreBlock> {
        let entry = &fixture["blocks"][index];
        let text = |field: &str| entry[field].as_str().context("fixture field");
        let header: Header = deserialize(&hex::decode(text("header")?)?)?;
        assert_eq!(header.block_hash().to_string(), text("hash")?);
        let tx: Transaction = deserialize(&hex::decode(text("coinbase_tx")?)?)?;
        Ok(CoreBlock {
            height: i32::try_from(entry["height"].as_i64().context("height")?)?,
            hash: header.block_hash().to_byte_array().to_vec(),
            header,
            coinbase: BitcoinCoreBlockCoinbase {
                txid: tx.compute_txid().to_byte_array().to_vec(),
                script: tx.input[0].script_sig.to_bytes(),
                outputs: serialize(&tx.output),
            },
        })
    };
    Ok([block(0)?, block(1)?])
}

fn event_evidence(
    key: &'static str,
    child_height: i32,
    block: &CoreBlock,
    verdict: mmm_bitcoin_core::ParentClassification,
) -> ChildEvidence {
    ChildEvidence::new(
        key,
        NAMECOIN_SOURCE_CODE,
        child_height,
        0x11,
        block.header,
        verdict,
        100,
    )
    .with_parent_coinbase_script(block.coinbase.script.clone())
    .with_parent_coinbase_outputs(block.coinbase.outputs.clone())
}

/// (vout, protocol, decode_status, upstream_selected, evidence_source, declared account pool).
type Commitment = (i32, String, String, Option<bool>, String, Option<String>);

async fn commitments(client: &Client, hash: &[u8]) -> Result<Vec<Commitment>> {
    let rows = client
        .query(
            "SELECT c.vout, s.protocol, s.decode_status, c.upstream_selected, c.evidence_source, \
                    p.slug \
               FROM protocol_commitment c \
               JOIN protocol_commitment_script s ON s.id = c.script_id \
               LEFT JOIN pool_identity i \
                 ON (i.namespace, i.identifier) = (s.account_namespace, s.account_identifier) \
               LEFT JOIN pool p ON p.id = i.pool_id \
              WHERE c.btc_header_hash = $1 ORDER BY c.vout",
            &[&hash],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            (
                row.get(0),
                row.get(1),
                row.get(2),
                row.get(3),
                row.get(4),
                row.get(5),
            )
        })
        .collect())
}

fn commitment(
    vout: i32,
    protocol: &str,
    upstream_selected: Option<bool>,
    evidence_source: &str,
    pool: &str,
) -> Commitment {
    (
        vout,
        protocol.to_owned(),
        "valid".to_owned(),
        upstream_selected,
        evidence_source.to_owned(),
        Some(pool.to_owned()),
    )
}

/// Every derived row, by script bytes rather than dimension id.
async fn derived_rows(client: &Client) -> Result<Vec<String>> {
    let rows = client
        .query(
            "SELECT format('%s/%s %s %s %s %s', encode(c.btc_header_hash, 'hex'), c.vout, \
                           encode(s.script, 'hex'), c.upstream_selected, c.btc_header_time, \
                           c.evidence_source) \
               FROM protocol_commitment c JOIN protocol_commitment_script s ON s.id = c.script_id \
             UNION ALL \
             SELECT format('%s undecodable %s %s', encode(btc_header_hash, 'hex'), \
                           evidence_source, error) \
               FROM protocol_commitment_undecodable \
             ORDER BY 1",
            &[],
        )
        .await?;
    Ok(rows.iter().map(|row| row.get(0)).collect())
}

async fn decoder_version(client: &Client) -> Result<Option<i32>> {
    Ok(client
        .query_one(
            "SELECT protocol_commitments_decoder_version FROM read_model_invariant",
            &[],
        )
        .await?
        .get(0))
}

async fn rebuild(client: &mut Client) -> Result<()> {
    run_reconcile_read_model(
        client,
        &ConfiguredParentClassifier::Disabled,
        ReconcileReadModelConfig {
            rebuild_protocol_commitments: true,
            ..ReconcileReadModelConfig::default()
        },
    )
    .await?;
    Ok(())
}

/// `outputs` plus a CORE declaration followed by 10,000 incompressible bytes, and its vout.
fn with_long_core_output(outputs: &[u8]) -> Result<(Vec<u8>, i32)> {
    let mut outputs: Vec<TxOut> = deserialize(outputs)?;
    let mut script = b"\x6a\x2dCORE\x01".to_vec();
    let mut state: u32 = 1;
    script.extend((0..10_000).map(|_| {
        state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        state.to_be_bytes()[0]
    }));
    outputs.push(TxOut {
        value: Amount::ZERO,
        script_pubkey: ScriptBuf::from_bytes(script),
    });
    Ok((serialize(&outputs), i32::try_from(outputs.len() - 1)?))
}

#[tokio::test]
async fn protocol_commitments_follow_evidence_and_the_rebuild_reproduces_them() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let (_, mut pool_ids) = default_pool_snapshot(&client).await?;
        seed_protocol_account_identities(&client, &mut pool_ids).await?;
        let [early, transition] = secpool_blocks()?;

        Scenario::new()
            .child_event(event_evidence(
                "transition",
                700_000,
                &transition,
                canonical_verdict(&transition.header, transition.height),
            ))
            .apply(&mut client)
            .await?;
        assert_eq!(
            commitments(&client, &transition.hash).await?,
            [
                commitment(3, "core", Some(true), "auxpow_event", "secpool"),
                commitment(4, "exsat", None, "auxpow_event", "antpool"),
            ]
        );

        Scenario::new()
            .core_canonical_with_coinbase(
                transition.header,
                transition.height,
                transition.coinbase.clone(),
            )
            .apply(&mut client)
            .await?;
        assert_eq!(
            commitments(&client, &transition.hash).await?,
            [
                commitment(3, "core", Some(true), "bitcoin_core", "secpool"),
                commitment(4, "exsat", None, "bitcoin_core", "antpool"),
            ],
            "Core's own coinbase takes over the provenance"
        );

        let applied = Scenario::new()
            .child_event(event_evidence(
                "early",
                700_001,
                &early,
                unknown_verdict(&early.header),
            ))
            .apply(&mut client)
            .await?;
        assert_eq!(
            commitments(&client, &early.hash).await?,
            [commitment(3, "core", Some(true), "auxpow_event", "secpool")]
        );

        // A declared exSat account never reaches Bitcoin-side pool attribution.
        let secpool = pool_id_for_slug(&client, "secpool").await?;
        for block in [&early, &transition] {
            let pool: Option<i64> = client
                .query_one(
                    "SELECT bitcoin_miner_pool_id FROM block WHERE btc_header_hash = $1",
                    &[&block.hash],
                )
                .await?
                .get(0);
            assert_eq!(pool, Some(secpool), "block {}", block.height);
        }

        // The rebuild derives exactly what the inline writes left.
        let inline = derived_rows(&client).await?;
        assert_eq!(decoder_version(&client).await?, None);
        client
            .batch_execute(
                "TRUNCATE protocol_commitment, protocol_commitment_undecodable, \
                          protocol_commitment_script",
            )
            .await?;
        rebuild(&mut client).await?;
        assert_eq!(derived_rows(&client).await?, inline);
        assert_eq!(decoder_version(&client).await?, Some(DECODER_VERSION));

        revoke_event(&mut client, applied.event_id("early")?, "test").await?;
        assert!(
            commitments(&client, &early.hash).await?.is_empty(),
            "revoking the last event of a non-Core parent removes its commitments"
        );

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn near_commitments_survive_corrupt_blobs_and_the_rebuild_drops_orphans() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let fixture = NamecoinEventFixture::new(&client).await?;
        let [_, transition] = secpool_blocks()?;
        let near = parse_auxpow_fixture("500001-near-parent")?;
        let mut payload = namecoin_event_payload(
            &near,
            &fixture.resolver,
            &fixture.pool_ids_by_slug,
            500_001,
            ClassificationProof::default(),
            300,
        )?;
        payload.btc_parent_coinbase_outputs = Some(transition.coinbase.outputs.clone());
        let outcome = capture_test_outcome(
            &mut client,
            fixture.source_id,
            &ConfiguredParentClassifier::Disabled,
            &mut payload,
        )
        .await?;
        assert!(matches!(
            outcome,
            CaptureOutcome::Written(CaptureWrite::Near { .. })
        ));
        let parent = payload.btc_parent_header_hash.clone();
        let decoded = |source: &str| {
            vec![
                (3, "core".to_owned(), source.to_owned()),
                (4, "exsat".to_owned(), source.to_owned()),
            ]
        };
        let near_rows = async |client: &Client| -> Result<Vec<(i32, String, String)>> {
            Ok(commitments(client, &parent)
                .await?
                .into_iter()
                .map(|(vout, protocol, _, _, source, _)| (vout, protocol, source))
                .collect())
        };
        assert_eq!(near_rows(&client).await?, decoded("near_observation"));

        let set_outputs = async |client: &Client, outputs: &[u8]| -> Result<()> {
            client
                .execute(
                    "UPDATE child_block_observation SET btc_parent_coinbase_outputs = $2 \
                      WHERE observation_kind = 'near' AND btc_parent_header_hash = $1",
                    &[&parent, &outputs],
                )
                .await?;
            Ok(())
        };
        set_outputs(&client, &[0xff]).await?;
        rebuild(&mut client).await?;
        assert!(near_rows(&client).await?.is_empty());
        let undecodable: String = client
            .query_one(
                "SELECT evidence_source FROM protocol_commitment_undecodable \
                  WHERE btc_header_hash = $1",
                &[&parent],
            )
            .await?
            .get(0);
        assert_eq!(undecodable, "near_observation");

        set_outputs(&client, &transition.coinbase.outputs).await?;
        let orphan = hash_bytes(9);
        client
            .execute(
                "INSERT INTO protocol_commitment \
                        (btc_header_hash, vout, script_id, btc_header_time, evidence_source) \
                 SELECT $1, 1, min(id), 0, 'auxpow_event' FROM protocol_commitment_script",
                &[&orphan],
            )
            .await?;
        rebuild(&mut client).await?;
        assert_eq!(near_rows(&client).await?, decoded("near_observation"));
        let remaining: i64 = client
            .query_one(
                "SELECT (SELECT count(*) FROM protocol_commitment_undecodable) \
                      + (SELECT count(*) FROM protocol_commitment WHERE btc_header_hash = $1)",
                &[&orphan],
            )
            .await?
            .get(0);
        assert_eq!(
            remaining, 0,
            "a valid blob clears the undecodable row; orphans go"
        );

        // A coinbase output can be far longer than a B-tree index entry allows.
        let (outputs, vout) = with_long_core_output(&transition.coinbase.outputs)?;
        set_outputs(&client, &outputs).await?;
        rebuild(&mut client).await?;
        let mut expected = decoded("near_observation");
        expected.push((vout, "core".to_owned(), "near_observation".to_owned()));
        assert_eq!(near_rows(&client).await?, expected);

        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn protocol_account_registries_reconcile_their_namespaces() -> Result<()> {
    crate::run_db_test!(client, {
        let (_, mut pool_ids) = default_pool_snapshot(&client).await?;
        seed_protocol_account_identities(&client, &mut pool_ids).await?;

        // A later registry without most entries and with one remapped replaces the namespace.
        reconcile_identity_registry(
            &client,
            EXSAT_ACCOUNT_NAMESPACE,
            &[IdentityRegistryEntry {
                identifier: "f2pool.sat",
                pool_slug: "viabtc",
                pool_canonical_name: "ViaBTC",
            }],
            &mut pool_ids,
        )
        .await?;
        let exsat: Vec<(String, String)> = client
            .query(
                "SELECT i.identifier, p.slug FROM pool_identity i JOIN pool p ON p.id = i.pool_id \
                  WHERE i.namespace = 'exsat_account'",
                &[],
            )
            .await?
            .iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();
        assert_eq!(exsat, [("f2pool.sat".to_owned(), "viabtc".to_owned())]);

        Ok::<_, anyhow::Error>(())
    })
}
