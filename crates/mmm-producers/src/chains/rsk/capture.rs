//! RSK-side capture path. Mirrors `src/chains/auxpow_family.rs` for the RSK
//! producer.
//!
//! The structure/capture slice deliberately scopes itself to raw evidence
//! ingestion:
//!
//! - Walk canonical RSK blocks and their uncles via the RSK RPC client.
//! - Convert each block into a [`NormalizedEventEvidence`] +
//!   [`RskEvidencePayload`] pair.
//! - Write both rows and the initial parent read-model reconcile inside a
//!   single Postgres transaction, so `merge_mining_event`,
//!   `rsk_merge_mining_evidence`, and the parent read-model row never disagree
//!   on whether the captured row exists.
//!
//! `btc_parent_kind` for RSK rows is computed exactly as for Namecoin:
//! `near` when the embedded 80-byte BTC parent header fails its own
//! `nBits`, `unknown` when it passes but no further chain context is
//! available.

use std::collections::HashMap;

use anyhow::{Context, Result};
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use tokio_postgres::Client;
use tracing::{debug, warn};

use crate::chains::rsk::rpc::{RskBlock, RskRpcClient, decode_hex_bytes, decode_quantity_i64};
use crate::chains::rsk::traverse::{RskHeightBundle, fetch_rsk_height_bundle};
use crate::producer_runtime::ProducerContext;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::capture::{
    ClassificationProof, EventPoolAttribution, MergeMiningEventPayload, NormalizedEventEvidence,
    RSK_PROOF_FORMAT_OPAQUE, ResolvedPoolAttributions, RskEvidencePayload,
    build_event_payload_from_evidence, now_epoch_seconds,
};
use mmm_capture::pool_resolver::{PoolIdentityRegistry, PoolResolver};
use mmm_capture::source_registry::RSK_SOURCE_CODE;
use mmm_read_model::capture_in_txn;
use mmm_store::{upsert_rsk_only_pools, upsert_rsk_pool_identities, write_rsk_capture_in_txn};

/// Startup-resolved state shared by every RSK capture: the embedded
/// `rsk_miner_address` registry, the `miner_hex -> pool_identity.id` lookup it
/// seeds, and the base [`ProducerContext`] (source id, pool-id-by-slug map,
/// parent classifier). Built once per poll/backfill run, then read-only.
#[derive(Debug)]
pub struct RskCaptureContext {
    registry: PoolIdentityRegistry,
    /// Lower-case, unprefixed miner hex -> `pool_identity.id`, keyed identically
    /// to the registry's normalized form so a resolved miner maps straight to
    /// its sidecar identity row.
    identity_ids_by_address: HashMap<String, i64>,
    base: ProducerContext,
}

impl RskCaptureContext {
    /// Bootstrap the context: seed the BTC pool snapshot first (so RSK miners
    /// mapping to existing slugs reuse those pool rows), then upsert RSK-only
    /// slugs and the per-miner `pool_identity` rows. `parent_classifier` is
    /// threaded through so live capture can attest BTC parent absence.
    pub async fn new_with_classifier(
        client: &Client,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Result<Self> {
        // Load the BTC pool snapshot first so RSK miners that map to
        // existing pool slugs (f2pool, antpool, viabtc, ...) resolve straight
        // to the snapshot's pool rows. RSK-only slugs (btc-com, poolin,
        // secpool today) get created with empty BTC fields and are reachable
        // exclusively via pool_identity.
        let resolver = PoolResolver::from_default_snapshot()?;
        let mut base =
            ProducerContext::bootstrap_with(client, RSK_SOURCE_CODE, &resolver, parent_classifier)
                .await?;

        let registry = PoolIdentityRegistry::from_default_rsk_registry()?;
        upsert_rsk_only_pools(client, &registry, base.pool_ids_by_slug_mut()).await?;
        let identity_ids_by_address =
            upsert_rsk_pool_identities(client, &registry, base.pool_ids_by_slug()).await?;

        Ok(Self {
            registry,
            identity_ids_by_address,
            base,
        })
    }

    /// `source.id` for the RSK source row, stamped on every `merge_mining_event`
    /// this context writes.
    pub fn source_id(&self) -> i64 {
        self.base.source_id()
    }

    /// The BTC parent classifier for this run, passed to `capture_in_txn` so an
    /// enabled classifier can attest Core-absence and set `btc_orphan_class`.
    pub fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        self.base.parent_classifier()
    }

    /// Test constructor: build a context from its constituent parts so unit and
    /// integration tests can exercise [`prepare_rsk_capture`] (and downstream
    /// transactional writes) without standing up the full startup flow.
    #[cfg(any(test, feature = "db-integration"))]
    #[doc(hidden)]
    pub fn from_parts(
        registry: PoolIdentityRegistry,
        identity_ids_by_address: HashMap<String, i64>,
        pool_ids_by_slug: HashMap<String, i64>,
        source_id: i64,
    ) -> Self {
        Self::from_parts_with_classifier(
            registry,
            identity_ids_by_address,
            pool_ids_by_slug,
            source_id,
            ConfiguredParentClassifier::Disabled,
        )
    }

    /// As [`from_parts`](Self::from_parts) but with an explicit classifier, so a
    /// test can exercise the enabled-classifier path without the startup flow.
    #[cfg(any(test, feature = "db-integration"))]
    #[doc(hidden)]
    pub fn from_parts_with_classifier(
        registry: PoolIdentityRegistry,
        identity_ids_by_address: HashMap<String, i64>,
        pool_ids_by_slug: HashMap<String, i64>,
        source_id: i64,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Self {
        Self {
            registry,
            identity_ids_by_address,
            base: ProducerContext::from_parts(pool_ids_by_slug, source_id, parent_classifier),
        }
    }
}

/// Per-block capture outcome. Aggregated by callers into a height-level
/// summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockOutcome {
    /// Block decoded cleanly and its `(event, evidence)` pair committed.
    Written,
    /// Block predates RSKIP-92: no (or short) 80-byte BTC parent header, so
    /// nothing is written. Retryable-clean, not an error.
    PreRskip92Skipped,
    /// A merge-mining field was undecodable (bad hex, wrong byte length, height
    /// overflow). Skipped so one bad block never aborts the backfill.
    MalformedSkipped,
}

/// Inputs ready for transactional RSK capture. Produced by
/// [`prepare_rsk_capture`] when the block parses cleanly and the BTC parent
/// header is post-RSKIP-92.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RskCaptureInputs {
    pub payload: MergeMiningEventPayload,
    pub evidence: RskEvidencePayload,
}

/// Decision produced by [`prepare_rsk_capture`]. Either the block is ready
/// to write (with its `(payload, evidence)` pair) or a structural property
/// of the RPC response prevents writing (pre-RSKIP-92 era or malformed
/// fields).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureDecision {
    Ready(Box<RskCaptureInputs>),
    PreRskip92Skipped,
    MalformedSkipped,
}

/// Per-height summary the bounded backfill rolls up into its grand totals.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HeightOutcome {
    /// Whether a canonical block existed at this height. `false` (the default)
    /// drives `Hold` in the live poller so the height is retried, not skipped.
    pub canonical_present: bool,
    /// The canonical block's outcome, `None` only when its RSK number could not
    /// be decoded to i32 (uncle traversal is then abandoned for the height).
    pub canonical: Option<BlockOutcome>,
    pub uncles_seen: usize,
    pub uncles_written: usize,
    pub uncles_pre_rskip92: usize,
    pub uncles_malformed: usize,
}

/// Live-poll path: fetch one height's bundle over RPC and write it on the same
/// connection, stamping each written block with [`now_epoch_seconds`].
pub async fn process_rsk_height(
    client: &mut Client,
    rpc: &RskRpcClient,
    context: &RskCaptureContext,
    height: i64,
) -> Result<HeightOutcome> {
    let bundle = fetch_rsk_height_bundle(rpc.clone(), height).await?;
    write_rsk_bundle(client, context, bundle, &now_epoch_seconds).await
}

/// Write one prefetched [`RskHeightBundle`] on the single DB connection, in
/// strict canonical-then-uncles order. `observed_at` is an observed-time
/// PROVIDER invoked once per written block (canonical and each uncle), so
/// production passes [`now_epoch_seconds`] (byte-identical to the historical
/// per-block timestamp behavior) and tests pass a constant provider for
/// determinism.
///
/// A listed uncle the source returned `null` for (`Ok(None)`) is counted as
/// `uncles_seen` + `uncles_malformed`, matching the serial path. A captured
/// uncle fetch `Err(e)` returns `e` AFTER the canonical and prior uncles are
/// committed, mirroring `process_rsk_height`'s historical `?` on a uncle fetch.
pub(crate) async fn write_rsk_bundle<F>(
    client: &mut Client,
    context: &RskCaptureContext,
    bundle: RskHeightBundle,
    observed_at: &F,
) -> Result<HeightOutcome>
where
    F: Fn() -> Result<i64>,
{
    let Some(canonical) = bundle.canonical else {
        return Ok(HeightOutcome::default());
    };

    let mut outcome = HeightOutcome {
        canonical_present: true,
        ..HeightOutcome::default()
    };

    let canonical_result =
        capture_block(client, context, &canonical, false, None, None, observed_at).await?;
    outcome.canonical = Some(canonical_result);

    // The fetch stage only populated `uncles` when the canonical RSK height
    // decoded cleanly into i32 range, so this is the parent height for every
    // uncle in the bundle.
    let canonical_height_i32 = match decode_quantity_i64(&canonical.number)
        .ok()
        .and_then(|n| i32::try_from(n).ok())
    {
        Some(n) => n,
        None => return Ok(outcome),
    };

    if canonical_result == BlockOutcome::PreRskip92Skipped {
        debug!(
            rsk_hash = %canonical.hash,
            "pre-RSKIP-92 RSK canonical block; uncles at the same era will also skip"
        );
    }

    for fetched in bundle.uncles {
        outcome.uncles_seen += 1;
        let uncle = match fetched.result.with_context(|| {
            format!(
                "get RSK uncle at canonical height {canonical_height_i32} index {}",
                fetched.index
            )
        })? {
            Some(u) => u,
            None => {
                outcome.uncles_malformed += 1;
                continue;
            }
        };
        let uncle_result = capture_block(
            client,
            context,
            &uncle,
            true,
            Some(fetched.index),
            Some(canonical_height_i32),
            observed_at,
        )
        .await?;
        match uncle_result {
            BlockOutcome::Written => outcome.uncles_written += 1,
            BlockOutcome::PreRskip92Skipped => outcome.uncles_pre_rskip92 += 1,
            BlockOutcome::MalformedSkipped => outcome.uncles_malformed += 1,
        }
    }

    Ok(outcome)
}

async fn capture_block<F>(
    client: &mut Client,
    context: &RskCaptureContext,
    block: &RskBlock,
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
    observed_at: &F,
) -> Result<BlockOutcome>
where
    F: Fn() -> Result<i64>,
{
    match prepare_rsk_capture(
        context,
        block,
        is_uncle,
        uncle_index,
        uncle_parent_height,
        observed_at()?,
    )? {
        CaptureDecision::Ready(inputs) => capture_ready_inputs(client, context, *inputs).await,
        CaptureDecision::PreRskip92Skipped => Ok(BlockOutcome::PreRskip92Skipped),
        CaptureDecision::MalformedSkipped => Ok(BlockOutcome::MalformedSkipped),
    }
}

async fn capture_ready_inputs(
    client: &mut Client,
    context: &RskCaptureContext,
    mut inputs: RskCaptureInputs,
) -> Result<BlockOutcome> {
    capture_in_txn(
        client,
        context.source_id(),
        context.parent_classifier(),
        &mut inputs.payload,
        "RSK",
        async |txn, source_id, payload| {
            write_rsk_capture_in_txn(txn, source_id, payload, &inputs.evidence).await
        },
    )
    .await?;
    Ok(BlockOutcome::Written)
}

#[cfg(any(test, feature = "db-integration"))]
#[doc(hidden)]
pub async fn capture_ready_rsk_inputs_for_test(
    client: &mut Client,
    context: &RskCaptureContext,
    inputs: RskCaptureInputs,
) -> Result<BlockOutcome> {
    capture_ready_inputs(client, context, inputs).await
}

/// Decode the 80-byte BTC parent header field, mapping absent/short payloads
/// to the pre-RSKIP-92 skip and undecodable bytes to the malformed skip.
fn decode_rsk_parent_header(block: &RskBlock) -> Result<Result<Header, CaptureDecision>> {
    let Some(header_hex) = block.bitcoin_merged_mining_header.as_deref() else {
        return Ok(Err(CaptureDecision::PreRskip92Skipped));
    };
    if header_hex.trim_start_matches("0x").is_empty() {
        return Ok(Err(CaptureDecision::PreRskip92Skipped));
    }
    let header_bytes = match decode_hex_bytes(header_hex) {
        Ok(b) => b,
        Err(err) => {
            warn!(
                rsk_hash = %block.hash,
                error = %err,
                "malformed bitcoinMergedMiningHeader hex; skipping"
            );
            return Ok(Err(CaptureDecision::MalformedSkipped));
        }
    };
    if header_bytes.len() != 80 {
        // Pre-RSKIP-92 blocks have shorter merge-mining payloads.
        return Ok(Err(CaptureDecision::PreRskip92Skipped));
    }
    let header: Header = match deserialize(&header_bytes) {
        Ok(h) => h,
        Err(err) => {
            warn!(
                rsk_hash = %block.hash,
                error = %err,
                "could not deserialize 80-byte BTC parent header from RSK RPC; skipping"
            );
            return Ok(Err(CaptureDecision::MalformedSkipped));
        }
    };

    Ok(Ok(header))
}

/// The decoded, validated RSK merge-mining fields a capture decision is
/// built from.
struct DecodedRskFields {
    header: Header,
    block_hash_bytes: Vec<u8>,
    miner_bytes: Vec<u8>,
    merge_mining_hash: Vec<u8>,
    merkle_proof: Option<Vec<u8>>,
    coinbase_tail: Option<Vec<u8>>,
    rsk_height: i32,
    timestamp: i64,
}

struct RequiredRskEvidenceFields {
    block_hash_bytes: Vec<u8>,
    miner_bytes: Vec<u8>,
    merge_mining_hash: Vec<u8>,
}

struct OptionalRskProofFields {
    merkle_proof: Option<Vec<u8>>,
    coinbase_tail: Option<Vec<u8>>,
}

struct RskBlockQuantities {
    rsk_height: i32,
    timestamp: i64,
}

/// Decode and validate every merge-mining field on the RSK block, mapping
/// each defect onto the documented skip decision (pre-RSKIP-92 short/absent
/// payloads vs malformed evidence) with the original warn! diagnostics.
fn decode_rsk_block_fields(block: &RskBlock) -> Result<Result<DecodedRskFields, CaptureDecision>> {
    let header = match decode_rsk_parent_header(block)? {
        Ok(parsed) => parsed,
        Err(decision) => return Ok(Err(decision)),
    };

    let required = match decode_required_rsk_evidence_fields(block)? {
        Ok(fields) => fields,
        Err(decision) => return Ok(Err(decision)),
    };
    let optional = match decode_optional_rsk_proof_fields(block)? {
        Ok(fields) => fields,
        Err(decision) => return Ok(Err(decision)),
    };
    let quantities = match decode_rsk_block_quantities(block)? {
        Ok(fields) => fields,
        Err(decision) => return Ok(Err(decision)),
    };

    Ok(Ok(DecodedRskFields {
        header,
        block_hash_bytes: required.block_hash_bytes,
        miner_bytes: required.miner_bytes,
        merge_mining_hash: required.merge_mining_hash,
        merkle_proof: optional.merkle_proof,
        coinbase_tail: optional.coinbase_tail,
        rsk_height: quantities.rsk_height,
        timestamp: quantities.timestamp,
    }))
}

fn decode_required_rsk_evidence_fields(
    block: &RskBlock,
) -> Result<Result<RequiredRskEvidenceFields, CaptureDecision>> {
    let block_hash_bytes = match validated_byte_length(&block.hash, 32) {
        Some(b) => b,
        None => return Ok(Err(CaptureDecision::MalformedSkipped)),
    };
    let miner_bytes = match validated_byte_length(&block.miner, 20) {
        Some(b) => b,
        None => return Ok(Err(CaptureDecision::MalformedSkipped)),
    };
    let merge_mining_hash = match block.hash_for_merged_mining.as_deref() {
        Some(s) => match validated_byte_length(s, 32) {
            Some(b) => b,
            None => return Ok(Err(CaptureDecision::MalformedSkipped)),
        },
        None => return Ok(Err(CaptureDecision::MalformedSkipped)),
    };

    Ok(Ok(RequiredRskEvidenceFields {
        block_hash_bytes,
        miner_bytes,
        merge_mining_hash,
    }))
}

fn decode_optional_rsk_proof_fields(
    block: &RskBlock,
) -> Result<Result<OptionalRskProofFields, CaptureDecision>> {
    let merkle_proof =
        match decode_optional_hex(block.bitcoin_merged_mining_merkle_proof.as_deref()) {
            Ok(v) => v,
            Err(err) => {
                warn!(
                    rsk_hash = %block.hash,
                    error = %err,
                    "malformed bitcoinMergedMiningMerkleProof hex; skipping"
                );
                return Ok(Err(CaptureDecision::MalformedSkipped));
            }
        };
    let coinbase_tail =
        match decode_optional_hex(block.bitcoin_merged_mining_coinbase_transaction.as_deref()) {
            Ok(v) => v,
            Err(err) => {
                warn!(
                    rsk_hash = %block.hash,
                    error = %err,
                    "malformed bitcoinMergedMiningCoinbaseTransaction hex; skipping"
                );
                return Ok(Err(CaptureDecision::MalformedSkipped));
            }
        };

    Ok(Ok(OptionalRskProofFields {
        merkle_proof,
        coinbase_tail,
    }))
}

fn decode_rsk_block_quantities(
    block: &RskBlock,
) -> Result<Result<RskBlockQuantities, CaptureDecision>> {
    let rsk_height_i64 = match decode_quantity_i64(&block.number) {
        Ok(n) => n,
        Err(err) => {
            warn!(
                rsk_hash = %block.hash,
                raw = %block.number,
                error = %err,
                "malformed RSK block.number hex quantity; skipping"
            );
            return Ok(Err(CaptureDecision::MalformedSkipped));
        }
    };
    let rsk_height: i32 = match rsk_height_i64.try_into() {
        Ok(n) => n,
        Err(_) => {
            warn!(
                rsk_hash = %block.hash,
                height = rsk_height_i64,
                "RSK height overflows i32; skipping"
            );
            return Ok(Err(CaptureDecision::MalformedSkipped));
        }
    };
    let timestamp = match decode_quantity_i64(&block.timestamp) {
        Ok(t) => t,
        Err(err) => {
            warn!(
                rsk_hash = %block.hash,
                raw = %block.timestamp,
                error = %err,
                "malformed RSK block.timestamp hex quantity; skipping"
            );
            return Ok(Err(CaptureDecision::MalformedSkipped));
        }
    };

    Ok(Ok(RskBlockQuantities {
        rsk_height,
        timestamp,
    }))
}

/// Synchronous, side-effect-free conversion from an [`RskBlock`] to a
/// [`CaptureDecision`]. Exposed for tests that want to validate the produced
/// payload + evidence format without standing up Postgres.
pub fn prepare_rsk_capture(
    context: &RskCaptureContext,
    block: &RskBlock,
    is_uncle: bool,
    uncle_index: Option<i32>,
    uncle_parent_height: Option<i32>,
    observed_at_epoch: i64,
) -> Result<CaptureDecision> {
    let fields = match decode_rsk_block_fields(block)? {
        Ok(fields) => fields,
        Err(decision) => return Ok(decision),
    };
    let DecodedRskFields {
        header,
        block_hash_bytes,
        miner_bytes,
        merge_mining_hash,
        merkle_proof,
        coinbase_tail,
        rsk_height,
        timestamp,
    } = fields;

    // Resolve pool identity from the miner address. The registry normalises
    // 0x-prefixed and case-insensitive forms; identity_ids_by_address is
    // keyed the same way (lower-case, no prefix).
    let miner_hex = hex::encode(&miner_bytes);
    let identity_match = context.registry.resolve_rsk_miner(&miner_hex);
    let pool_identity_id =
        identity_match.and(context.identity_ids_by_address.get(&miner_hex).copied());
    let pool_id = identity_match.and_then(|m| {
        context
            .base
            .pool_ids_by_slug()
            .get(&m.entry.pool_slug)
            .copied()
    });

    let evidence_for_event = NormalizedEventEvidence {
        child_height: rsk_height,
        child_block_hash: block_hash_bytes.clone(),
        child_block_time: timestamp,
        btc_parent_header: header,
        // RSK exposes only an RLP-encoded child header with an integer
        // difficulty; no consensus-grade child target is available in this
        // slice, so the column stays NULL until a future classifier slice
        // can derive one.
        pow_validates_child_target: None,
        // BTC parent coinbase fields are irrecoverable post-RSKIP-92
        // midstate compression; the proof bytes live in
        // rsk_merge_mining_evidence.merkle_proof instead of
        // merge_mining_event.aux_merkle_proof.
        btc_parent_coinbase_txid: None,
        btc_parent_coinbase_script: None,
        btc_parent_coinbase_outputs: None,
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: None,
    };

    let mapped = identity_match.is_some();
    let pool_attributions = ResolvedPoolAttributions {
        attributions: vec![EventPoolAttribution::rsk_miner_address(
            miner_hex,
            pool_id,
            pool_identity_id,
            mapped,
        )],
    };

    let payload = build_event_payload_from_evidence(
        evidence_for_event,
        pool_attributions,
        ClassificationProof::default(),
        observed_at_epoch,
    )?;

    let evidence = RskEvidencePayload {
        rsk_block_hash: block_hash_bytes,
        rsk_height,
        is_uncle,
        uncle_index,
        uncle_parent_height,
        rsk_miner: miner_bytes,
        pool_identity_id,
        merge_mining_hash,
        merkle_proof,
        coinbase_tail,
        proof_format: RSK_PROOF_FORMAT_OPAQUE,
    };

    Ok(CaptureDecision::Ready(Box::new(RskCaptureInputs {
        payload,
        evidence,
    })))
}

/// Decode `0x`-prefixed hex and accept it only at exactly `expected_len` bytes.
/// `None` (bad hex or wrong length) maps to a malformed skip at the call site,
/// guarding the fixed-width hash (32) and miner (20) columns.
fn validated_byte_length(raw_hex: &str, expected_len: usize) -> Option<Vec<u8>> {
    decode_hex_bytes(raw_hex)
        .ok()
        .filter(|bytes| bytes.len() == expected_len)
}

/// Decode an optional `0x`-prefixed hex string into bytes, propagating decode
/// errors so callers can convert them to [`CaptureDecision::MalformedSkipped`]
/// rather than aborting backfill.
fn decode_optional_hex(raw: Option<&str>) -> Result<Option<Vec<u8>>> {
    raw.map(decode_hex_bytes).transpose()
}

use crate::chains::spec::{ChainId, by_id};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress};

/// RSK live capture chain. An absent canonical block yields `Hold` so the
/// height is retried rather than skipped.
pub(crate) struct RskChainPoller {
    state: ChainPollerState,
    rpc: RskRpcClient,
    context: RskCaptureContext,
}

impl RskChainPoller {
    /// Bind the single DB connection, the RPC client, and the startup-resolved
    /// capture context into the poller the registry drives.
    pub(crate) fn new(client: Client, rpc: RskRpcClient, context: RskCaptureContext) -> Self {
        Self {
            state: ChainPollerState::new(by_id(ChainId::Rsk), context.source_id(), client),
            rpc,
            context,
        }
    }
}

impl ChainPoller for RskChainPoller {
    fn poller_state(&self) -> &ChainPollerState {
        &self.state
    }

    fn client_mut(&mut self) -> &mut Client {
        &mut self.state.client
    }

    async fn chain_tip(&self) -> Result<i32> {
        let tip = self.rpc.get_block_number().await?;
        i32::try_from(tip).with_context(|| format!("RSK tip {tip} overflows i32"))
    }

    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome = process_rsk_height(
            &mut self.state.client,
            &self.rpc,
            &self.context,
            height as i64,
        )
        .await?;
        Ok(if outcome.canonical_present {
            HeightProgress::Advance
        } else {
            HeightProgress::Hold
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::rsk::test_fixtures::{
        KNOWN_MINER_HEX, UNKNOWN_MINER_HEX, load_rsk_block_fixture,
    };
    use mmm_capture::capture::ParentKind;
    use mmm_capture::pool_resolver::{PoolIdentityRegistry, RskMinerEntry, RskMinerRegistry};

    fn fixture_context() -> RskCaptureContext {
        let registry = PoolIdentityRegistry::from_rsk_registry(RskMinerRegistry {
            schema_version: 1,
            generated_at: "test".to_owned(),
            scope: "test".to_owned(),
            source: mmm_capture::pool_resolver::PoolSnapshotSource {
                name: "test".to_owned(),
                upstream_url: None,
                license: None,
                notes: None,
            },
            entries: vec![RskMinerEntry {
                miner_address: KNOWN_MINER_HEX.to_owned(),
                pool_slug: "f2pool".to_owned(),
                pool_canonical_name: "F2Pool".to_owned(),
            }],
        })
        .unwrap();

        let mut identity_ids = HashMap::new();
        identity_ids.insert(KNOWN_MINER_HEX.to_owned(), 9001);
        let mut pool_ids = HashMap::new();
        pool_ids.insert("f2pool".to_owned(), 7001);

        RskCaptureContext::from_parts(registry, identity_ids, pool_ids, 42)
    }

    fn assert_single_miner_attribution(
        inputs: &RskCaptureInputs,
        matched_value: &str,
        pool_id: Option<i64>,
        pool_identity_id: Option<i64>,
        source: &str,
    ) {
        assert_eq!(inputs.payload.pool_attributions.len(), 1);
        let attribution = &inputs.payload.pool_attributions[0];
        assert_eq!(attribution.side.as_db_str(), "child_block");
        assert_eq!(attribution.namespace, "rsk_miner_address");
        assert_eq!(attribution.match_kind, "miner_address");
        assert_eq!(attribution.matched_value, matched_value);
        assert_eq!(attribution.pool_id, pool_id);
        assert_eq!(attribution.pool_identity_id, pool_identity_id);
        assert_eq!(attribution.source, source);
        assert_eq!(attribution.confidence.as_db_str(), "high");
    }

    fn ready_capture_inputs(fixture: &str) -> RskCaptureInputs {
        let block = load_rsk_block_fixture(fixture);
        let decision =
            prepare_rsk_capture(&fixture_context(), &block, false, None, None, 9_999_999).unwrap();
        match decision {
            CaptureDecision::Ready(inputs) => *inputs,
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    fn assert_malformed_fixture_skips(fixture: &str) {
        let block = load_rsk_block_fixture(fixture);
        let decision = prepare_rsk_capture(&fixture_context(), &block, false, None, None, 0)
            .expect("must skip cleanly, not propagate");
        assert_eq!(decision, CaptureDecision::MalformedSkipped);
    }

    fn assert_mutated_canonical_skips(mutate: impl FnOnce(&mut RskBlock)) {
        let mut block = load_rsk_block_fixture("canonical-valid");
        mutate(&mut block);
        let decision = prepare_rsk_capture(&fixture_context(), &block, false, None, None, 0)
            .expect("must skip cleanly, not propagate");
        assert_eq!(decision, CaptureDecision::MalformedSkipped);
    }

    #[test]
    fn canonical_blocks_classify_by_parent_pow_and_record_miner_attribution() {
        let inputs = ready_capture_inputs("canonical-valid");
        assert_eq!(inputs.payload.btc_parent_kind, ParentKind::Unknown);
        assert!(inputs.payload.pow_validates_btc_target);
        assert_eq!(inputs.payload.pow_validates_child_target, None);
        assert_eq!(inputs.payload.btc_parent_height, None);
        assert_single_miner_attribution(
            &inputs,
            KNOWN_MINER_HEX,
            Some(7001),
            Some(9001),
            "rsk_miner_registry",
        );
        assert!(inputs.payload.btc_parent_coinbase_script.is_none());
        assert!(inputs.payload.aux_merkle_proof.is_none());

        assert!(!inputs.evidence.is_uncle);
        assert_eq!(inputs.evidence.uncle_index, None);
        assert_eq!(inputs.evidence.uncle_parent_height, None);
        assert_eq!(inputs.evidence.pool_identity_id, Some(9001));
        assert_eq!(inputs.evidence.rsk_height, 729_000);
        assert_eq!(inputs.evidence.proof_format, RSK_PROOF_FORMAT_OPAQUE);

        let inputs = ready_capture_inputs("canonical-near");
        assert_eq!(inputs.payload.btc_parent_kind, ParentKind::Near);
        assert!(!inputs.payload.pow_validates_btc_target);
        assert_eq!(inputs.payload.pow_validates_child_target, None);
        // Unknown miner remains observed provenance without pool resolution.
        assert_eq!(inputs.evidence.pool_identity_id, None);
        assert_single_miner_attribution(&inputs, UNKNOWN_MINER_HEX, None, None, "rsk_rpc_miner");
    }

    #[test]
    fn uncle_block_records_uncle_context_on_evidence() {
        let block = load_rsk_block_fixture("uncle-valid");

        let decision = prepare_rsk_capture(
            &fixture_context(),
            &block,
            true,
            Some(0),
            Some(800_010),
            9_999_999,
        )
        .unwrap();
        let inputs = match decision {
            CaptureDecision::Ready(inputs) => *inputs,
            other => panic!("expected Ready, got {other:?}"),
        };

        assert!(inputs.evidence.is_uncle);
        assert_eq!(inputs.evidence.uncle_index, Some(0));
        assert_eq!(inputs.evidence.uncle_parent_height, Some(800_010));
        // child_height on the main row is the uncle's own RSK block number.
        assert_eq!(inputs.payload.child_height, 800_001);
    }

    #[test]
    fn pre_rskip92_block_is_skipped() {
        let block = load_rsk_block_fixture("pre-rskip92");

        let decision =
            prepare_rsk_capture(&fixture_context(), &block, false, None, None, 0).unwrap();
        assert_eq!(decision, CaptureDecision::PreRskip92Skipped);
    }

    #[test]
    fn malformed_rsk_block_fields_are_skipped_not_propagated() {
        // 19 bytes instead of 20.
        assert_mutated_canonical_skips(|block| {
            block.miner = "0x0123456789abcdef0123456789abcdef012345".to_owned();
        });

        // A malformed bitcoinMergedMiningHeader hex must skip cleanly so a
        // single bad block doesn't abort the bounded backfill.
        assert_malformed_fixture_skips("malformed-header");
        assert_mutated_canonical_skips(|block| {
            block.bitcoin_merged_mining_merkle_proof = Some("0xZZ".to_owned());
        });
        assert_mutated_canonical_skips(|block| {
            block.bitcoin_merged_mining_coinbase_transaction = Some("0xZZ".to_owned());
        });
        assert_mutated_canonical_skips(|block| block.number = "0xnothex".to_owned());
        assert_mutated_canonical_skips(|block| block.timestamp = "0xnothex".to_owned());
        // i32::MAX + 1.
        assert_mutated_canonical_skips(|block| block.number = "0x80000000".to_owned());
    }

    // ── Fetch-stage (fetch_rsk_height_bundle) unit tests ─────────────────────
    //
    // These exercise the prefetch stage format + ordering with a deterministic
    // in-memory `RskBlockSource`; no DB. The persisted-row behavior of
    // `write_rsk_bundle` (including the canonical+prior-uncles-before-error
    // ordering) is covered DB-backed in `tests/db_integration.rs`.

    use std::collections::HashMap;
}
