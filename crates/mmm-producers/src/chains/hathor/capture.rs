//! Hathor per-height capture state machine for live polling and bounded backfills.
//!
//! Self-verifies RFC 0006 reconstruction identity, `"Hath"` marker, child weight
//! target, and Bitcoin parent PoW/lineage against the untrusted REST API, and
//! records `child_chain_head` under [`may_record_current_block`]. A parent that
//! misses Bitcoin's target is captured as a near (its observation keeps the parent
//! header and coinbase; no sidecar), a block without AuxPoW is recorded as a
//! `not_merge_mined` observation, and a version-3 block whose reconstruction or
//! parent coinbase is malformed as `unverified_parent`. Those checks protect Bitcoin-side evidence and
//! the chain head, not the child inventory: which block sits at a height comes from
//! the REST answer, and a near's declared weight is the block's own claim.

use anyhow::{Context, Result};
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash as _;
use bitcoin::{BlockHash, Transaction};
use tokio_postgres::{Client, GenericClient};
use tracing::{debug, error, warn};

use crate::chains::hathor::auxpow::HathorReconstruction;
use crate::chains::hathor::convert::{block_hash_internal, derive_output_addresses};
use crate::chains::hathor::identity::upsert_hathor_reward_pool_identities;
use crate::chains::hathor::reconstruct::{
    DeclaredWork, HathorReconstructedParent, meets_hathor_target, reconstruct_or_skip,
};
use crate::chains::hathor::reward::{HATHOR_REWARD_ADDRESS_NAMESPACE, parse_hathor_reward_outputs};
use crate::chains::hathor::rpc::{HathorBlockMeta, HathorRpc, HathorTransaction};
use crate::poller::RescanOutcome;
use crate::producer_runtime::ProducerContext;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::capture::{
    ChildBlockObservation, ClassificationProof, HATHOR_PROOF_FORMAT_RFC0006, HathorEvidencePayload,
    NormalizedEventEvidence, ObservationKind, ResolvedPoolAttributions,
    build_event_payload_from_evidence, now_epoch_seconds,
    resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_capture::source_registry::HATHOR_SOURCE_CODE;
use mmm_read_model::CaptureOutcome;
use mmm_store::{
    ChildChainHeadOutcome, ChildChainHeadRecord, CurrentBlockParent, EvidenceMarker,
    finish_child_chain_height_operation, hathor_evidence_event_at_height,
    hathor_sidecar_graph_heads_at_height, load_child_chain_head, load_pool_identities_by_namespace,
    lock_child_chain_height_session, record_child_chain_block,
    record_child_chain_block_in_own_transaction, reobserve_child_chain_block_in_own_transaction,
    upsert_child_only_observation, write_hathor_capture_in_txn,
};

/// The most Hathor's difficulty adjustment moves a block's weight from its
/// parent's (hathor-core `BLOCK_DIFFICULTY_MAX_DW`). Two blocks at one height
/// on branches that diverged `d` blocks below them therefore differ by at
/// most `2 * 0.25 * d`, each branch moving its own way.
const HATHOR_MAX_WEIGHT_STEP: f64 = 0.25;

/// The deepest fork displacement reconciles on its own, in blocks: a quarter
/// of an hour of 30-second blocks, beyond the default rescan window of 20 and
/// any reorg the child DAG has shown. It bounds the fork window a run may
/// set, so the work floor a replacement must clear never falls below a
/// captured block's weight less 16, whatever the configuration.
const HATHOR_MAX_DISPLACEMENT_FORK_DEPTH: i32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainObservation {
    Live {
        fork_window: i32,
    },
    /// Archive replay writes events without updating `child_chain_head`.
    ArchiveReplay,
}

#[derive(Debug)]
pub struct HathorCaptureContext {
    resolver: PoolResolver,
    base: ProducerContext,
    reward_identities: PoolIdentityLookup,
    observation: ChainObservation,
}

impl HathorCaptureContext {
    pub async fn new_with_classifier(
        client: &Client,
        parent_classifier: ConfiguredParentClassifier,
        observation: ChainObservation,
    ) -> Result<Self> {
        let resolver = PoolResolver::from_default_snapshot()?;
        let mut base = ProducerContext::bootstrap_with(
            client,
            HATHOR_SOURCE_CODE,
            &resolver,
            parent_classifier,
        )
        .await?;
        upsert_hathor_reward_pool_identities(client, base.pool_ids_by_slug_mut()).await?;
        let reward_identities =
            load_pool_identities_by_namespace(client, &[HATHOR_REWARD_ADDRESS_NAMESPACE]).await?;
        let observation = match observation {
            ChainObservation::Live { fork_window } => ChainObservation::Live {
                fork_window: fork_window.clamp(1, HATHOR_MAX_DISPLACEMENT_FORK_DEPTH),
            },
            ChainObservation::ArchiveReplay => ChainObservation::ArchiveReplay,
        };
        Ok(Self {
            resolver,
            base,
            reward_identities,
            observation,
        })
    }

    pub fn source_id(&self) -> i64 {
        self.base.source_id()
    }

    pub fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        self.base.parent_classifier()
    }

    pub(super) async fn refresh_core_header_cache(&self, client: &mut Client) -> Result<()> {
        self.base.refresh_core_header_cache(client).await
    }

    pub(super) fn non_bitcoin_parents(&self) -> usize {
        self.base.non_bitcoin_parents()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HathorHeightOutcome {
    AuxpowWritten,
    NonAuxpowSkipped,
    VoidedSkipped,
    MalformedSkipped,
    NonBitcoinParent,
    LineagePending,
    AbsentHold,
    TransientHold,
}

struct BuiltCapture {
    evidence: NormalizedEventEvidence,
    sidecar: HathorEvidencePayload,
    pool_attributions: ResolvedPoolAttributions,
}

enum HathorBlockDecision {
    Voided,
    NonAuxpow {
        current_hash: Vec<u8>,
    },
    Auxpow {
        current_hash: Vec<u8>,
        tx: HathorTransaction,
    },
}

type HathorBlockLoad = std::result::Result<HathorBlockDecision, HathorHeightOutcome>;

/// Capture one Hathor height under the session-level `(source, height)` lock.
pub async fn process_hathor_height(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    height: i32,
) -> Result<HathorHeightOutcome> {
    let source_id = context.source_id();
    lock_child_chain_height_session(client, source_id, height).await?;
    let result = process_locked_height(client, rpc, context, height).await;
    finish_child_chain_height_operation(client, source_id, height, result).await
}

/// Re-observe a height in the trailing rescan window: when `/block_at_height`
/// matches a final `child_chain_head` with unchanged sidecar evidence, run
/// displacement maintenance only; otherwise run full capture.
pub async fn rescan_hathor_height(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    height: i32,
) -> Result<RescanOutcome<HathorHeightOutcome>> {
    let source_id = context.source_id();
    lock_child_chain_height_session(client, source_id, height).await?;
    let result = async {
        let block = match fetch_hathor_block_meta(rpc, height).await? {
            Ok(block) => block,
            Err(outcome) => return Ok(RescanOutcome::Captured(outcome)),
        };
        if let Ok(Ok(Some(current_hash))) = gate_hathor_block_meta(height, &block)
            && let Some(head) =
                load_child_chain_head(&*client, source_id, height, EvidenceMarker::HathorSidecars)
                    .await?
            && head.is_final()
            && !head.evidence_changed
            && head.block_hash == current_hash
        {
            reobserve_child_chain_block_in_own_transaction(
                client,
                source_id,
                height,
                &current_hash,
                head.current_parent(),
                now_epoch_seconds()?,
            )
            .await?;
            return Ok(RescanOutcome::Unchanged);
        }
        process_locked_block(client, rpc, context, height, block)
            .await
            .map(RescanOutcome::Captured)
    }
    .await;
    finish_child_chain_height_operation(client, source_id, height, result).await
}

async fn process_locked_height(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    height: i32,
) -> Result<HathorHeightOutcome> {
    let block = match fetch_hathor_block_meta(rpc, height).await? {
        Ok(block) => block,
        Err(outcome) => return Ok(outcome),
    };
    process_locked_block(client, rpc, context, height, block).await
}

async fn process_locked_block(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    height: i32,
    block: HathorBlockMeta,
) -> Result<HathorHeightOutcome> {
    let decision = match classify_hathor_block(rpc, height, &block).await? {
        Ok(decision) => decision,
        Err(outcome) => return Ok(outcome),
    };
    let (current_hash, tx) = match decision {
        HathorBlockDecision::Voided => return Ok(HathorHeightOutcome::VoidedSkipped),
        HathorBlockDecision::NonAuxpow { current_hash } => {
            let kind = ObservationKind::NotMergeMined;
            return record_child_only_block(
                client,
                context,
                height,
                current_hash,
                &block,
                kind,
                None,
            )
            .await;
        }
        HathorBlockDecision::Auxpow { current_hash, tx } => (current_hash, tx),
    };

    let unverified = ObservationKind::UnverifiedParent;
    let Some(parent) = reconstruct_or_skip(height, &tx)? else {
        return record_child_only_block(
            client,
            context,
            height,
            current_hash,
            &block,
            unverified,
            None,
        )
        .await;
    };
    let work = parent.work;
    let (outcome, core_cache_generation) =
        write_reconstructed(client, context, height, &current_hash, &tx, parent).await?;
    if outcome == HathorHeightOutcome::MalformedSkipped {
        // Reconstruction authenticated the block; only its parent coinbase is malformed.
        return record_child_only_block(
            client,
            context,
            height,
            current_hash,
            &block,
            unverified,
            Some(work),
        )
        .await;
    }

    if outcome != HathorHeightOutcome::AuxpowWritten
        && may_record_current_block(&*client, context, height, work).await?
    {
        let head_outcome = if outcome == HathorHeightOutcome::LineagePending {
            ChildChainHeadOutcome::Held
        } else {
            ChildChainHeadOutcome::Recorded
        };
        record_child_chain_block_in_own_transaction(
            client,
            context.source_id(),
            height,
            ChildChainHeadRecord {
                block_hash: &current_hash,
                parent: CurrentBlockParent::Known(&current_hash),
                outcome: head_outcome,
                evidence: EvidenceMarker::HathorSidecars,
                observed_at: now_epoch_seconds()?,
                core_cache_generation,
            },
            None,
        )
        .await?;
    }
    Ok(outcome)
}

async fn fetch_hathor_block_meta(
    rpc: &impl HathorRpc,
    height: i32,
) -> Result<std::result::Result<HathorBlockMeta, HathorHeightOutcome>> {
    match rpc.get_block_at_height(height).await {
        Ok(Some(block)) => Ok(Ok(block)),
        Ok(None) => Ok(Err(HathorHeightOutcome::AbsentHold)),
        Err(err) => {
            warn!(height, error = %err, "Hathor block_at_height fetch failed; holding");
            Ok(Err(HathorHeightOutcome::TransientHold))
        }
    }
}

async fn classify_hathor_block(
    rpc: &impl HathorRpc,
    height: i32,
    block: &HathorBlockMeta,
) -> Result<HathorBlockLoad> {
    let current_hash = match gate_hathor_block_meta(height, block)? {
        Ok(Some(hash)) => hash,
        Ok(None) => return Ok(Ok(HathorBlockDecision::Voided)),
        Err(outcome) => return Ok(Err(outcome)),
    };

    if block.version != 3 {
        return Ok(Ok(HathorBlockDecision::NonAuxpow { current_hash }));
    }

    let tx = match fetch_validated_hathor_transaction(rpc, height, &block.tx_id).await? {
        Ok(tx) => tx,
        Err(outcome) => return Ok(Err(outcome)),
    };
    Ok(Ok(HathorBlockDecision::Auxpow { current_hash, tx }))
}

fn gate_hathor_block_meta(
    height: i32,
    block: &HathorBlockMeta,
) -> Result<std::result::Result<Option<Vec<u8>>, HathorHeightOutcome>> {
    if block.height != height {
        warn!(
            requested = height,
            returned = block.height,
            "Hathor /block_at_height answered for a different height; holding"
        );
        return Ok(Err(HathorHeightOutcome::TransientHold));
    }

    if block.is_voided {
        return Ok(Ok(None));
    }

    Ok(hathor_block_hash_or_skip(height, &block.tx_id)?.map(Some))
}

fn hathor_block_hash_or_skip(
    height: i32,
    tx_id: &str,
) -> Result<std::result::Result<Vec<u8>, HathorHeightOutcome>> {
    match block_hash_internal(tx_id) {
        Ok(hash) => Ok(Ok(hash)),
        Err(err) => {
            error!(height, error = %err, "Hathor block tx_id is not a valid hash; skipping");
            Ok(Err(HathorHeightOutcome::MalformedSkipped))
        }
    }
}

async fn fetch_validated_hathor_transaction(
    rpc: &impl HathorRpc,
    height: i32,
    tx_id: &str,
) -> Result<std::result::Result<HathorTransaction, HathorHeightOutcome>> {
    let tx = match rpc.get_transaction(tx_id).await {
        Ok(Some(tx)) => tx,
        Ok(None) => return Ok(Err(HathorHeightOutcome::AbsentHold)),
        Err(err) => {
            warn!(height, error = %err, "Hathor transaction fetch failed; holding");
            return Ok(Err(HathorHeightOutcome::TransientHold));
        }
    };
    if tx.hash != tx_id {
        error!(
            height,
            "Hathor tx.hash != block tx_id; skipping inconsistent pair"
        );
        return Ok(Err(HathorHeightOutcome::MalformedSkipped));
    }
    Ok(Ok(tx))
}

/// Record a block with no Bitcoin parent on record from its `/block_at_height` answer
/// (no further fetch): `not_merge_mined` for a block without AuxPoW, or
/// `unverified_parent` for a version-3 block whose RFC 0006 reconstruction or parent
/// coinbase is malformed. `reconstructed` is the work a successful reconstruction
/// already checked the hash against; otherwise the hash is checked against the weight
/// the answer declares, and one that misses it is malformed and records nothing, as
/// does an `unverified_parent` answer without a weight (the archive cache carries
/// none). The block becomes the chain's head at the height under
/// [`may_record_current_block`]; a `not_merge_mined` answer without a weight leaves
/// only the observation.
async fn record_child_only_block(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    current_hash: Vec<u8>,
    block: &HathorBlockMeta,
    kind: ObservationKind,
    reconstructed: Option<DeclaredWork>,
) -> Result<HathorHeightOutcome> {
    let (head_parent, head_outcome, outcome) = match kind {
        ObservationKind::NotMergeMined => (
            CurrentBlockParent::NoAuxpow,
            ChildChainHeadOutcome::NoAuxpow,
            HathorHeightOutcome::NonAuxpowSkipped,
        ),
        _ => (
            CurrentBlockParent::Unknown,
            ChildChainHeadOutcome::Unverified,
            HathorHeightOutcome::MalformedSkipped,
        ),
    };
    if reconstructed.is_none()
        && kind == ObservationKind::UnverifiedParent
        && block.weight.is_none()
    {
        // Without a declared weight the hash cannot be checked against its own target.
        debug!(
            height,
            "Hathor answer carries no weight; block not recorded"
        );
        return Ok(outcome);
    }
    let parent_block: Option<[u8; 32]> = block
        .parents
        .first()
        .and_then(|parent| hex::decode(parent).ok()?.try_into().ok())
        .or(reconstructed.and_then(|work| work.parent_block));
    let work = match (reconstructed, block.weight) {
        (Some(work), _) => Some(work),
        (None, Some(weight)) => {
            let hash = BlockHash::from_slice(&current_hash).context("read Hathor block hash")?;
            if !meets_hathor_target(hash, weight) {
                error!(
                    height,
                    weight, "Hathor block hash does not meet its declared target; skipping"
                );
                return Ok(HathorHeightOutcome::MalformedSkipped);
            }
            Some(DeclaredWork {
                weight,
                parent_block,
            })
        }
        (None, None) => None,
    };
    let observed_at = now_epoch_seconds()?;
    let observation = ChildBlockObservation::child_only(
        kind,
        height,
        current_hash.clone(),
        block.timestamp,
        parent_block.map(internal_order),
        observed_at,
    );
    let records_head = match work {
        Some(work) => may_record_current_block(&*client, context, height, work).await?,
        None => false,
    };
    if records_head {
        record_child_chain_block_in_own_transaction(
            client,
            context.source_id(),
            height,
            ChildChainHeadRecord {
                block_hash: &current_hash,
                parent: head_parent,
                outcome: head_outcome,
                evidence: EvidenceMarker::HathorSidecars,
                observed_at,
                core_cache_generation: None,
            },
            Some(&observation),
        )
        .await
        .map(|_| ())
    } else {
        upsert_child_only_observation(&*client, context.source_id(), &observation)
            .await
            .map(|_| ())
    }
    .with_context(|| {
        format!(
            "record Hathor {} block at height {height}",
            kind.as_db_str()
        )
    })?;
    Ok(outcome)
}

async fn write_reconstructed(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    current_hash: &[u8],
    tx: &HathorTransaction,
    parent: HathorReconstructedParent,
) -> Result<(HathorHeightOutcome, Option<i64>)> {
    let HathorReconstructedParent {
        raw,
        aux_pow,
        recon,
        work,
    } = parent;
    let funds_graph = &raw[..recon.funds_graph_len];
    match build_hathor_capture(
        context,
        tx,
        height,
        &aux_pow,
        &recon,
        funds_graph,
        work.parent_block,
    )? {
        Some(built) => write_capture(client, context, height, current_hash, work, built)
            .await
            .with_context(|| format!("Hathor capture at height {height}")),
        None => Ok((HathorHeightOutcome::MalformedSkipped, None)),
    }
}

/// Gate `child_chain_head` updates against untrusted REST responses: where a block
/// was captured at `height`, require `work.weight >= captured.weight - 2 * 0.25 * fork_depth`
/// (`fork_depth = 1` when sharing a parent block, else `fork_window`). Where none was,
/// the head is recorded only if no event is stored at the height (an imported event has no
/// sidecar to hold work against), so the REST answer can displace observations there but
/// never Bitcoin-relevant evidence.
async fn may_record_current_block<C: GenericClient>(
    client: &C,
    context: &HathorCaptureContext,
    height: i32,
    work: DeclaredWork,
) -> Result<bool> {
    let ChainObservation::Live { fork_window } = context.observation else {
        return Ok(false);
    };
    let floor = hathor_sidecar_graph_heads_at_height(client, context.source_id(), height)
        .await?
        .iter()
        .filter_map(|head| {
            let captured = DeclaredWork::read(head, 0)?;
            let fork_depth =
                if work.parent_block.is_some() && work.parent_block == captured.parent_block {
                    1
                } else {
                    fork_window
                };
            Some(captured.weight - 2.0 * HATHOR_MAX_WEIGHT_STEP * f64::from(fork_depth))
        })
        .reduce(f64::max);
    let Some(floor) = floor else {
        let event_held =
            hathor_evidence_event_at_height(client, context.source_id(), height).await?;
        if event_held {
            debug!(
                height,
                "an event without a captured Hathor block holds the height; not recorded"
            );
        }
        return Ok(!event_held);
    };
    let holds = work.weight >= floor;
    if !holds {
        warn!(
            height,
            declared = work.weight,
            floor,
            "Hathor block declares far less work than a block captured at this height; \
             not recorded as the chain's block"
        );
    }
    Ok(holds)
}

async fn write_capture(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    current_hash: &[u8],
    work: DeclaredWork,
    built: BuiltCapture,
) -> Result<(HathorHeightOutcome, Option<i64>)> {
    let now = now_epoch_seconds()?;
    let mut payload = build_event_payload_from_evidence(
        built.evidence,
        built.pool_attributions,
        ClassificationProof::default(),
        now,
    )?;
    let sidecar = built.sidecar;

    let captured = context
        .base
        .capture(
            client,
            &mut payload,
            "Hathor",
            async |txn, source_id, payload| {
                let write = write_hathor_capture_in_txn(txn, source_id, payload, &sidecar).await?;
                if may_record_current_block(txn, context, height, work).await? {
                    record_child_chain_block(
                        txn,
                        source_id,
                        height,
                        ChildChainHeadRecord {
                            block_hash: current_hash,
                            parent: CurrentBlockParent::Known(current_hash),
                            outcome: ChildChainHeadOutcome::captured(
                                payload.classification_provisional,
                            ),
                            evidence: EvidenceMarker::HathorSidecars,
                            observed_at: now,
                            core_cache_generation: None,
                        },
                    )
                    .await?;
                }
                Ok(write)
            },
        )
        .await?;
    Ok(match captured {
        CaptureOutcome::Written(_) => (HathorHeightOutcome::AuxpowWritten, None),
        CaptureOutcome::NotBitcoin {
            core_cache_generation,
            ..
        } => (
            HathorHeightOutcome::NonBitcoinParent,
            Some(core_cache_generation),
        ),
        CaptureOutcome::LineagePending(_) => (HathorHeightOutcome::LineagePending, None),
    })
}

/// A display-order hash (as Hathor lists parents) in internal byte order.
fn internal_order(mut display: [u8; 32]) -> Vec<u8> {
    display.reverse();
    display.to_vec()
}

/// Returns `Ok(None)` when reconstructed coinbase bytes fail consensus decode or
/// lack a coinbase input (e.g. archive height 1292779 with trailing bytes), so a
/// malformed coinbase skips the block rather than failing the tick.
fn build_hathor_capture(
    context: &HathorCaptureContext,
    tx: &HathorTransaction,
    hathor_height: i32,
    aux_pow: &[u8],
    recon: &HathorReconstruction,
    funds_graph: &[u8],
    parent_block: Option<[u8; 32]>,
) -> Result<Option<BuiltCapture>> {
    let coinbase: Transaction = match deserialize(&recon.full_coinbase) {
        Ok(tx) => tx,
        Err(err) => {
            error!(
                height = hathor_height,
                error = %err,
                "reconstructed BTC parent coinbase does not deserialize; skipping"
            );
            return Ok(None);
        }
    };
    if !coinbase.is_coinbase() {
        error!(
            height = hathor_height,
            "reconstructed BTC parent transaction is not coinbase; skipping"
        );
        return Ok(None);
    }
    let coinbase_input = &coinbase.input[0];
    let script_sig = coinbase_input.script_sig.as_bytes().to_vec();
    let output_addresses = derive_output_addresses(&coinbase);
    let coinbase_txid = coinbase.compute_txid();

    let block_hash_bytes = recon.header.block_hash().to_byte_array().to_vec();
    let parent_attribution = resolve_parent_pool_attribution_from_coinbase(
        &script_sig,
        &output_addresses,
        &context.resolver,
        context.base.pool_ids_by_slug(),
    );
    let reward_parse = match parse_hathor_reward_outputs(
        funds_graph,
        recon.funds_graph_split as i32,
    ) {
        Ok(parsed) => Some(parsed),
        Err(err) => {
            warn!(
                height = hathor_height,
                error = %err,
                "Hathor funds_graph reward parser failed; preserving parent evidence without child reward attribution"
            );
            None
        }
    };
    let mut attributions = parent_attribution.into_iter().collect::<Vec<_>>();
    if let Some(parsed) = reward_parse.as_ref() {
        attributions.extend(parsed.reward_attributions(&context.reward_identities));
    }
    let pool_attributions = ResolvedPoolAttributions { attributions };

    let evidence = NormalizedEventEvidence {
        child_height: Some(hathor_height),
        child_block_hash: Some(block_hash_bytes.clone()),
        child_header_bytes: None,
        child_block_time: Some(tx.timestamp),
        child_nbits: None,
        child_prev_block_hash: parent_block.map(internal_order),
        btc_parent_header: recon.header,
        pow_validates_child_target: None,
        btc_parent_coinbase_txid: Some(coinbase_txid.to_byte_array().to_vec()),
        btc_parent_coinbase_script: Some(script_sig),
        btc_parent_coinbase_outputs: Some(serialize(&coinbase.output)),
        btc_parent_coinbase_outputs_text: None,
        btc_parent_coinbase_tx_bytes: Some(recon.full_coinbase.clone()),
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: None,
    };

    let sidecar = HathorEvidencePayload {
        hathor_block_hash: block_hash_bytes,
        hathor_height,
        aux_pow: aux_pow.to_vec(),
        funds_graph: funds_graph.to_vec(),
        funds_graph_split: recon.funds_graph_split as i32,
        reward_output_details: reward_parse
            .as_ref()
            .map(|parsed| parsed.output_details_json()),
        reward_addresses: reward_parse
            .as_ref()
            .map(|parsed| parsed.reward_addresses_json()),
        proof_format: HATHOR_PROOF_FORMAT_RFC0006,
    };

    Ok(Some(BuiltCapture {
        evidence,
        sidecar,
        pool_attributions,
    }))
}

#[cfg(test)]
#[path = "capture_tests.rs"]
mod tests;
