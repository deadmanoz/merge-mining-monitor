use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail};
use bitcoin::BlockHash;
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::Hash as _;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::source_registry::NAMECOIN_SOURCE_CODE;
use mmm_capture::test_support::load_raw_namecoin_fixture;
use mmm_producers::chains::{
    AuxpowCaptureContext, AuxpowHeightOutcome, AuxpowRescanOutcome, BitcoindRpc, ChainId, by_id,
    process_auxpow_height, rescan_auxpow_height,
};
use mmm_store::{get_source_id, upsert_merge_mining_event};
use tokio_postgres::Client;

use crate::support::db::advisory_locks_held;
use crate::support::exact_observation;

const HEIGHT: i32 = 700;

/// A `BitcoindRpc` serving one scripted block per height, so the runner's
/// per-height path runs end to end against a fixture the way `FixtureHathorRpc`
/// drives the Hathor path. Each tick of a test builds a new chain state.
struct FixtureBitcoindRpc {
    blocks: HashMap<i32, Vec<u8>>,
    /// `getblockhash` and `getblock` calls served, so a test can pin what a
    /// rescan costs at the node: one hash lookup for an unchanged height.
    hash_calls: AtomicUsize,
    block_calls: AtomicUsize,
}

impl FixtureBitcoindRpc {
    fn carrying(height: i32, raw: Vec<u8>) -> Self {
        Self {
            blocks: HashMap::from([(height, raw)]),
            hash_calls: AtomicUsize::new(0),
            block_calls: AtomicUsize::new(0),
        }
    }

    /// `(getblockhash calls, getblock calls)` served so far.
    fn calls(&self) -> (usize, usize) {
        (
            self.hash_calls.load(Ordering::SeqCst),
            self.block_calls.load(Ordering::SeqCst),
        )
    }
}

impl BitcoindRpc for FixtureBitcoindRpc {
    async fn get_block_count(&self) -> Result<i32> {
        Ok(self.blocks.keys().copied().max().unwrap_or(0))
    }

    async fn get_block_hash(&self, height: i32) -> Result<BlockHash> {
        self.hash_calls.fetch_add(1, Ordering::SeqCst);
        let raw = self
            .blocks
            .get(&height)
            .with_context(|| format!("fixture chain has no block at {height}"))?;
        Ok(block_hash_of(raw))
    }

    async fn get_block_raw(&self, hash: &BlockHash) -> Result<Vec<u8>> {
        self.block_calls.fetch_add(1, Ordering::SeqCst);
        self.blocks
            .values()
            .find(|raw| block_hash_of(raw) == *hash)
            .cloned()
            .with_context(|| format!("fixture chain has no block {hash}"))
    }

    async fn get_header_with_auxpow(&self, _hash: &BlockHash) -> Result<Vec<u8>> {
        bail!("the Namecoin fetch strategy never calls getblockheader")
    }
}

/// The child block hash of a raw Namecoin block: the hash of its first 80
/// bytes, which is what `getblockhash` reports for it.
fn block_hash_of(raw: &[u8]) -> BlockHash {
    deserialize::<Header>(&raw[..80])
        .expect("fixture block starts with an 80-byte header")
        .block_hash()
}

/// A raw block whose header version lacks the AuxPoW bit, so the runner
/// classifies it as non-AuxPoW and writes no event for it.
fn non_auxpow_block() -> Vec<u8> {
    let mut raw = Vec::with_capacity(81);
    raw.extend_from_slice(&0x2000_0000i32.to_le_bytes());
    raw.extend_from_slice(&[0x11; 32]);
    raw.extend_from_slice(&[0x22; 32]);
    raw.extend_from_slice(&1_700_000_000u32.to_le_bytes());
    raw.extend_from_slice(&0x1d00_ffffu32.to_le_bytes());
    raw.extend_from_slice(&7u32.to_le_bytes());
    raw.push(0);
    raw
}

/// `(child_block_hash, child_displaced_by, revoked_at)` for every event at
/// `HEIGHT`, ordered by hash.
async fn rows_at_height(
    client: &Client,
    source_id: i64,
) -> Result<Vec<(Vec<u8>, Option<Vec<u8>>, Option<i64>)>> {
    let rows = client
        .query(
            "SELECT child_block_hash, child_displaced_by, revoked_at \
             FROM merge_mining_event \
             WHERE source_id = $1 AND child_height = $2 \
             ORDER BY child_block_hash",
            &[&source_id, &HEIGHT],
        )
        .await?;
    Ok(rows
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect())
}

#[tokio::test]
async fn rescanned_height_records_the_chains_block_and_displaces_the_replaced_one() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let context = AuxpowCaptureContext::new_with_classifier(
            &client,
            by_id(ChainId::Namecoin),
            ConfiguredParentClassifier::Disabled,
        )
        .await?;
        let block_a = load_raw_namecoin_fixture("500000-valid-parent");
        let block_b = load_raw_namecoin_fixture("500001-near-parent");
        let block_n = non_auxpow_block();
        let hash = |raw: &[u8]| block_hash_of(raw).to_byte_array().to_vec();

        // Tick 1: the chain carries A. Its event is written and current.
        let rpc = FixtureBitcoindRpc::carrying(HEIGHT, block_a.clone());
        let outcome = process_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(outcome, AuxpowHeightOutcome::AuxpowWritten);
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![(hash(&block_a), None, None)]
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);

        // Tick 2: a reorg replaces A with B at the same height. B is written
        // and current, A is displaced by B, and nothing is revoked.
        let rpc = FixtureBitcoindRpc::carrying(HEIGHT, block_b.clone());
        let outcome = process_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(outcome, AuxpowHeightOutcome::AuxpowWritten);
        let mut expected = vec![
            (hash(&block_a), Some(hash(&block_b)), None),
            (hash(&block_b), None, None),
        ];
        expected.sort();
        assert_eq!(rows_at_height(&client, source_id).await?, expected);

        // Tick 3: the chain flips back to A. A is restored, B is displaced.
        let rpc = FixtureBitcoindRpc::carrying(HEIGHT, block_a.clone());
        process_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        let mut expected = vec![
            (hash(&block_a), None, None),
            (hash(&block_b), Some(hash(&block_a)), None),
        ];
        expected.sort();
        assert_eq!(rows_at_height(&client, source_id).await?, expected);

        // Tick 4: a block with no AuxPoW takes the height. No event is written
        // for it, but it is still recorded as the chain's block, so A is
        // displaced by it and the height has no current event.
        let rpc = FixtureBitcoindRpc::carrying(HEIGHT, block_n.clone());
        let outcome = process_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(outcome, AuxpowHeightOutcome::NonAuxpowSkipped);
        let rows = rows_at_height(&client, source_id).await?;
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.2.is_none()));
        assert_eq!(advisory_locks_held(&client).await?, 0);
        assert_eq!(
            rows.iter()
                .find(|row| row.0 == hash(&block_a))
                .map(|row| row.1.clone()),
            Some(Some(hash(&block_n)))
        );
        Ok::<_, anyhow::Error>(())
    })
}

/// The `(outcome, observed_at)` of the `child_chain_head` row at `HEIGHT`.
async fn head_at_height(client: &Client, source_id: i64) -> Result<Option<(String, i64)>> {
    let row = client
        .query_opt(
            "SELECT outcome, observed_at FROM child_chain_head \
             WHERE source_id = $1 AND child_height = $2",
            &[&source_id, &HEIGHT],
        )
        .await?;
    Ok(row.map(|row| (row.get(0), row.get(1))))
}

#[tokio::test]
async fn rescan_of_an_unchanged_height_costs_one_hash_lookup() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let context = AuxpowCaptureContext::new_with_classifier(
            &client,
            by_id(ChainId::Namecoin),
            ConfiguredParentClassifier::Disabled,
        )
        .await?;
        let block_a = load_raw_namecoin_fixture("500000-valid-parent");
        let hash = |raw: &[u8]| block_hash_of(raw).to_byte_array().to_vec();

        // A captured height: the first observation fetches the block; the
        // rescan of the unchanged chain fetches only the hash and writes no
        // event, so the capture path (and the classifier behind it) never runs.
        let rpc = FixtureBitcoindRpc::carrying(HEIGHT, block_a.clone());
        process_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(rpc.calls(), (1, 1));
        let head = head_at_height(&client, source_id).await?;
        assert_eq!(head.as_ref().map(|head| head.0.as_str()), Some("captured"));

        let outcome = rescan_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(outcome, AuxpowRescanOutcome::Unchanged);
        assert_eq!(rpc.calls(), (2, 1));
        assert_eq!(
            rows_at_height(&client, source_id).await?,
            vec![(hash(&block_a), None, None)]
        );
        assert_eq!(advisory_locks_held(&client).await?, 0);

        // A block with no AuxPoW leaves no event row, but its head row makes
        // the next rescan just as cheap.
        let rpc = FixtureBitcoindRpc::carrying(HEIGHT, non_auxpow_block());
        let outcome = rescan_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(
            outcome,
            AuxpowRescanOutcome::Captured(AuxpowHeightOutcome::NonAuxpowSkipped)
        );
        assert_eq!(rpc.calls(), (1, 1));
        let head = head_at_height(&client, source_id).await?;
        assert_eq!(
            head.as_ref().map(|head| head.0.as_str()),
            Some("non_auxpow")
        );

        let outcome = rescan_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(outcome, AuxpowRescanOutcome::Unchanged);
        assert_eq!(rpc.calls(), (2, 1));
        assert_eq!(advisory_locks_held(&client).await?, 0);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rescan_with_a_matching_hash_still_displaces_an_imported_sibling() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let context = AuxpowCaptureContext::new_with_classifier(
            &client,
            by_id(ChainId::Namecoin),
            ConfiguredParentClassifier::Disabled,
        )
        .await?;
        let block_a = load_raw_namecoin_fixture("500000-valid-parent");
        let hash_a = block_hash_of(&block_a).to_byte_array().to_vec();
        let rpc = FixtureBitcoindRpc::carrying(HEIGHT, block_a);
        process_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;

        // A write that bypasses observation (a historical publication import
        // for a live chain) adds another block's event at the height with no
        // displacement recorded.
        let sibling_hash = [0x5b; 32];
        let sibling = exact_observation("500001-near-parent", HEIGHT, sibling_hash, 2_030)?;
        upsert_merge_mining_event(&client, source_id, &sibling).await?;
        assert!(
            rows_at_height(&client, source_id)
                .await?
                .iter()
                .all(|row| row.1.is_none())
        );

        // The chain still carries A, so the rescan skips the capture, and the
        // displacement maintenance it keeps marks the sibling displaced by A.
        let outcome = rescan_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(outcome, AuxpowRescanOutcome::Unchanged);
        assert_eq!(rpc.calls(), (2, 1));
        let mut expected = vec![
            (hash_a.clone(), None, None),
            (sibling_hash.to_vec(), Some(hash_a), None),
        ];
        expected.sort();
        assert_eq!(rows_at_height(&client, source_id).await?, expected);
        Ok::<_, anyhow::Error>(())
    })
}

#[tokio::test]
async fn rescan_of_a_non_final_record_captures_the_height_again() -> Result<()> {
    crate::run_mut_db_test!(client, {
        let source_id = get_source_id(&client, NAMECOIN_SOURCE_CODE).await?;
        let context = AuxpowCaptureContext::new_with_classifier(
            &client,
            by_id(ChainId::Namecoin),
            ConfiguredParentClassifier::Disabled,
        )
        .await?;
        // A proof that did not parse leaves an unverified record, so the same
        // block is fetched and examined again on every rescan.
        let rpc =
            FixtureBitcoindRpc::carrying(HEIGHT, load_raw_namecoin_fixture("500003-malformed"));
        let outcome = process_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(outcome, AuxpowHeightOutcome::MalformedSkipped);
        let head = head_at_height(&client, source_id).await?;
        assert_eq!(
            head.as_ref().map(|head| head.0.as_str()),
            Some("unverified")
        );

        let outcome = rescan_auxpow_height(&mut client, &rpc, &context, HEIGHT).await?;
        assert_eq!(
            outcome,
            AuxpowRescanOutcome::Captured(AuxpowHeightOutcome::MalformedSkipped)
        );
        assert_eq!(rpc.calls(), (2, 2));
        assert_eq!(advisory_locks_held(&client).await?, 0);
        Ok::<_, anyhow::Error>(())
    })
}
