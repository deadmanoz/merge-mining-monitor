//! Elastos capture: the per-height state machine for live polling and bounded
//! backfills.
//!
//! Elastos is a Namecoin-family AuxPoW producer reachable over a CONFIGURABLE
//! endpoint (the self-hosted node by default, the public RPC as a
//! fallback). Because the endpoint may be untrusted, every written event is
//! self-verified before any DB write: the child header reconstruction + hash
//! guard ([`crate::chains::elastos::rpc::ElastosBlock::reconstruct`]), the full CAuxPow
//! commitment ([`verify_auxpow_commitment`]), the BTC parent-target gate, the
//! child AuxPoW-target gate, and the shared Bitcoin-lineage gate every capture
//! passes (`mmm_read_model::capture_in_txn`).
//!
//! Unlike the own-node Namecoin-family producers, the Elastos child header is an
//! 84-byte header (the height is hashed in), so the child block hash is computed
//! and verified rather than taken as `ParsedHeader::hash()`. The producer builds a
//! [`NormalizedEventEvidence`] directly (the Hathor pattern): the child is the
//! Elastos block, the parent is the BTC block from the CAuxPow.
//!
//! Elastos defaults to `reorg_depth = 0` (DPoS finality). A replay/backfill can
//! still reprocess a height whose lineage verdict changed after a Core-cache
//! change: the shared capture seam then removes the event of a block whose
//! parent turned out to be another chain's.

use anyhow::{Context, Result};
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use tokio_postgres::{Client, Transaction};
use tracing::warn;

use crate::chains::elastos::identity::{
    ELASTOS_MINERINFO_NAMESPACE, ELASTOS_REWARD_ADDRESS_NAMESPACE,
    resolve_elastos_identity_attributions, upsert_elastos_minerinfo_pool_identities,
    upsert_elastos_reward_address_pool_identities,
};
use crate::chains::elastos::rpc::{ElastosBlock, ElastosRpc, ReconstructedBlock};
use crate::producer_runtime::ProducerContext;
use bitcoin::BlockHash;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::auxpow::{
    ELASTOS_AUXPOW_CHAIN_ID, ParsedAuxpowBlock, parse_elastos_auxpow, validates_target,
    verify_auxpow_commitment,
};
use mmm_capture::capture::{
    ClassificationProof, MergeMiningEventPayload, NormalizedEventEvidence,
    ResolvedPoolAttributions, build_event_payload_from_evidence, now_epoch_seconds,
    resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_capture::source_registry::ELASTOS_SOURCE_CODE;
use mmm_read_model::CaptureOutcome;
use mmm_store::{
    ChildChainHeadOutcome, ChildChainHeadRecord, CurrentBlockParent, EventWriteOutcome,
    EvidenceMarker, finish_child_chain_height_operation, load_pool_identities_by_namespace,
    lock_child_chain_height_session, record_child_chain_block,
    record_child_chain_block_in_own_transaction, upsert_merge_mining_event_with_attributions,
};

/// Per-run state shared across heights: the embedded pool resolver, the shared
/// `ProducerContext` (source_id + parent classifier), and the reward-address /
/// minerinfo identity lookup loaded once at startup. Cheap to share by reference;
/// holds no per-height state.
#[derive(Debug)]
pub struct ElastosCaptureContext {
    resolver: PoolResolver,
    base: ProducerContext,
    child_identities: PoolIdentityLookup,
}

impl ElastosCaptureContext {
    /// Bootstrap the context: register the Elastos source, snapshot the pool
    /// resolver, seed the embedded minerinfo registry, and preload the
    /// child-identity lookup for both Elastos namespaces (reward address +
    /// minerinfo). The minerinfo seed runs **before** the identity load, so a fresh
    /// database resolves the reviewed labels on the first capture (the Hathor
    /// pattern). The classifier is supplied so live polling and backfills share one
    /// configured parent classifier.
    pub async fn new_with_classifier(
        client: &Client,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Result<Self> {
        let resolver = PoolResolver::from_default_snapshot()?;
        let mut base = ProducerContext::bootstrap_with(
            client,
            ELASTOS_SOURCE_CODE,
            &resolver,
            parent_classifier,
        )
        .await?;
        upsert_elastos_minerinfo_pool_identities(client, base.pool_ids_by_slug_mut())
            .await
            .context("seed Elastos minerinfo identities at capture bootstrap")?;
        upsert_elastos_reward_address_pool_identities(client, base.pool_ids_by_slug_mut())
            .await
            .context("seed Elastos reward-address identities at capture bootstrap")?;
        let child_identities = load_pool_identities_by_namespace(
            client,
            &[
                ELASTOS_REWARD_ADDRESS_NAMESPACE,
                ELASTOS_MINERINFO_NAMESPACE,
            ],
        )
        .await?;
        Ok(Self {
            resolver,
            base,
            child_identities,
        })
    }

    /// The `source` row id for `ELASTOS_SOURCE_CODE` (every write/revoke is scoped
    /// to it).
    pub fn source_id(&self) -> i64 {
        self.base.source_id()
    }

    /// The configured BTC parent classifier, shared with `capture_in_txn`.
    pub fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        self.base.parent_classifier()
    }

    async fn refresh_core_header_cache(&self, client: &mut Client) -> Result<()> {
        self.base.refresh_core_header_cache(client).await
    }
}

/// Per-height capture outcome. The poller maps `LineagePending` to
/// [`crate::poller::HeightProgress::Hold`] and everything else to `Advance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElastosHeightOutcome {
    /// A verified BTC-parent event was written.
    AuxpowWritten,
    /// No `auxpow` field, or a pre-activation dummy block; no event.
    NonAuxpowSkipped,
    /// The parent header fails its own difficulty (the common merge-mined
    /// case): Elastos captures only parents that are blocks, never `near`.
    NearSkipped,
    /// The parent header fails the child AuxPoW target; never written.
    ChildTargetSkipped,
    /// The parent is another chain's header (the shared lineage gate); no
    /// event written.
    NonBitcoinParent,
    /// The parent's lineage waits on a Bitcoin epoch the Core header cache
    /// has not reached; no event written, and the height is retried.
    LineagePending,
    /// The block was inconsistent / the proof malformed; skipped without a write.
    MalformedSkipped,
}

/// A child block whose AuxPoW commitment verified and whose parent meets the
/// child target: the identity the child-side record uses. The Elastos endpoint
/// may be untrusted, and hash self-consistency alone would let a fabricated
/// response displace real events, so only a block backed by real work
/// committing to it is ever recorded as the chain's block.
#[derive(Debug, Clone, Copy)]
struct ProvenBlock {
    hash: BlockHash,
    parent_hash: BlockHash,
}

/// The pure (no-DB) verdict of evaluating a fetched block through every gate.
/// Keeps the gate logic unit-testable over committed fixtures.
enum ElastosEvaluation {
    /// Terminal skip, no event. `proven` is set when the block can still be
    /// recorded as the chain's block at its height, see [`ProvenBlock`].
    Skip {
        outcome: ElastosHeightOutcome,
        proven: Option<ProvenBlock>,
    },
    /// A verified proof whose parent is a block: capture it through the
    /// lineage gate.
    Write {
        parsed: Box<ParsedAuxpowBlock>,
        recon: Box<ReconstructedBlock>,
    },
}

/// Drive one Elastos height: fetch, evaluate every gate, then apply the DB effect.
///
/// Every processed height also records which block the child chain carries
/// there (see `docs/data-model.md`, "Child Displacement"): a captured block is
/// recorded inside its capture transaction, and a block that yields no event is
/// recorded in a transaction of its own only when it is proven (its AuxPoW
/// commitment verified and its parent meets the child target), because the
/// endpoint may be untrusted.
/// The whole height, from the RPC fetch through the last write, runs under a
/// session-level lock on `(source, height)`, so what was observed is what is
/// recorded even when a poller and a backfill overlap on the height.
pub async fn process_elastos_height(
    client: &mut Client,
    rpc: &impl ElastosRpc,
    context: &ElastosCaptureContext,
    height: i32,
) -> Result<ElastosHeightOutcome> {
    let source_id = context.source_id();
    lock_child_chain_height_session(client, source_id, height).await?;
    let result = process_locked_height(client, rpc, context, height).await;
    finish_child_chain_height_operation(client, source_id, height, result).await
}

/// One height's fetch, gates and writes, under the session-level height lock
/// [`process_elastos_height`] holds around it.
async fn process_locked_height(
    client: &mut Client,
    rpc: &impl ElastosRpc,
    context: &ElastosCaptureContext,
    height: i32,
) -> Result<ElastosHeightOutcome> {
    let block = rpc
        .get_block_by_height(height)
        .await
        .with_context(|| format!("fetch Elastos block at height {height}"))?;
    apply_elastos_evaluation(client, context, height, &block).await
}

async fn apply_elastos_evaluation(
    client: &mut Client,
    context: &ElastosCaptureContext,
    height: i32,
    rpc_block: &ElastosBlock,
) -> Result<ElastosHeightOutcome> {
    let (outcome, proven, core_cache_generation) = match evaluate_elastos_block(height, rpc_block) {
        ElastosEvaluation::Skip { outcome, proven } => (outcome, proven, None),
        ElastosEvaluation::Write { parsed, recon } => {
            let block = ProvenBlock {
                hash: recon.block_hash,
                parent_hash: parsed.parent_header.hash(),
            };
            let (outcome, core_cache_generation) =
                write_capture(client, context, rpc_block, &parsed, &recon)
                    .await
                    .with_context(|| format!("Elastos capture at height {height}"))?;
            (outcome, Some(block), core_cache_generation)
        }
    };

    // A captured block is recorded inside its capture transaction. Every other
    // outcome records the block here, but only when it is proven: a block whose
    // commitment did not verify, or that carries no AuxPoW, or a response for
    // another height, says nothing trustworthy about what the chain carries.
    if outcome != ElastosHeightOutcome::AuxpowWritten
        && let Some(block) = proven
    {
        // A near or child-target verdict is settled; a non-Bitcoin one carries
        // the Core-cache generation it was decided under, so a verdict-changing
        // refresh reopens it; a pending one is retried.
        let head_outcome = match outcome {
            ElastosHeightOutcome::LineagePending => ChildChainHeadOutcome::Held,
            _ => ChildChainHeadOutcome::Recorded,
        };
        record_child_chain_block_in_own_transaction(
            client,
            context.source_id(),
            height,
            ChildChainHeadRecord {
                block_hash: block.hash.as_ref(),
                parent: CurrentBlockParent::Known(block.parent_hash.as_ref()),
                outcome: head_outcome,
                evidence: EvidenceMarker::None,
                observed_at: now_epoch_seconds()?,
                core_cache_generation,
            },
        )
        .await?;
    }
    Ok(outcome)
}

/// Run every offline gate over a fetched block. No DB, no RPC: pure, so the gate
/// order and verdicts are unit-testable over committed fixtures.
///
/// Order (each fails closed): requested-height match -> reconstruct + hash guard
/// -> auxpow presence -> parse -> PARENT-side dummy filter -> commitment verify ->
/// BTC parent target -> child target. The parent's Bitcoin lineage is decided
/// at the write, by the shared capture seam. A verdict carries the block's
/// proven identity only once its commitment verified and its parent met the
/// child target.
fn evaluate_elastos_block(requested_height: i32, block: &ElastosBlock) -> ElastosEvaluation {
    // Untrusted-endpoint guard: the RPC must answer for the height we asked for, or
    // a stale/misrouted response would write or revoke the WRONG child height while
    // the poller advances past this one.
    if block.height != requested_height {
        warn!(
            requested = requested_height,
            returned = block.height,
            "Elastos RPC returned a different height than requested; skipping"
        );
        return ElastosEvaluation::Skip {
            outcome: ElastosHeightOutcome::MalformedSkipped,
            proven: None,
        };
    }

    let recon = match block.reconstruct() {
        Ok(recon) => recon,
        Err(err) => {
            warn!(height = block.height, error = %err, "Elastos reconstruction/hash guard failed; skipping");
            return ElastosEvaluation::Skip {
                outcome: ElastosHeightOutcome::MalformedSkipped,
                proven: None,
            };
        }
    };
    let skip = |outcome, proven| ElastosEvaluation::Skip { outcome, proven };

    let Some(auxpow_blob) = recon.auxpow.as_deref() else {
        return skip(ElastosHeightOutcome::NonAuxpowSkipped, None);
    };

    let parsed = match parse_elastos_auxpow(recon.prefix_header.clone(), auxpow_blob) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(height = block.height, error = %err, "Elastos auxpow parse failed; skipping");
            return skip(ElastosHeightOutcome::MalformedSkipped, None);
        }
    };

    if let Some(outcome) = auxpow_gate_skip(block.height, &parsed, &recon) {
        // Only a block whose commitment verified and whose parent meets the
        // child target is proven. `NearSkipped` is decided before the child
        // target check, so it is proven only when that check also passes; the
        // dummy, commitment and child-target failures never are.
        let proven = (outcome == ElastosHeightOutcome::NearSkipped
            && validates_target(parsed.parent_header.hash(), recon.prefix_header.bits()))
        .then(|| ProvenBlock {
            hash: recon.block_hash,
            parent_hash: parsed.parent_header.hash(),
        });
        return skip(outcome, proven);
    }

    ElastosEvaluation::Write {
        parsed: Box::new(parsed),
        recon: Box::new(recon),
    }
}

fn auxpow_gate_skip(
    block_height: i32,
    parsed: &ParsedAuxpowBlock,
    recon: &ReconstructedBlock,
) -> Option<ElastosHeightOutcome> {
    // Dummy-block filter on the PARENT first: pre-activation dummies carry a
    // fabricated parent (bits 0 / 0x7FFFFFFF, empty coinbase outputs) that would
    // otherwise fail commitment verification.
    let parent_bits = parsed.parent_header.bits().to_consensus();
    if parent_bits == 0 || parent_bits == 0x7FFF_FFFF || parsed.parent_coinbase_outputs.is_empty() {
        return Some(ElastosHeightOutcome::NonAuxpowSkipped);
    }

    // The trust boundary: the full CAuxPow commitment to this child block hash.
    if let Err(err) = verify_auxpow_commitment(parsed, recon.block_hash, ELASTOS_AUXPOW_CHAIN_ID) {
        warn!(height = block_height, error = %err, "Elastos AuxPoW commitment verification failed; skipping");
        return Some(ElastosHeightOutcome::MalformedSkipped);
    }

    // BTC parent PoW: Elastos captures only BTC-difficulty-valid parents (never
    // `near`); the common merge-mined parent only meets Elastos's easier target.
    if !validates_target(parsed.parent_header.hash(), parsed.parent_header.bits()) {
        return Some(ElastosHeightOutcome::NearSkipped);
    }

    // Child AuxPoW-target PoW: the merged-mining validity the own node gets for
    // free; re-enforced for the untrusted-endpoint path.
    if !validates_target(parsed.parent_header.hash(), recon.prefix_header.bits()) {
        return Some(ElastosHeightOutcome::ChildTargetSkipped);
    }

    None
}

/// The in-transaction half of a capture: the Elastos upsert (the shared event
/// row only, Elastos has no sidecar), then the child-side record that this
/// block is the chain's block at its height. The capture transaction took the
/// per-height lock first, so the record is ordered before every parent lock;
/// the whole closure may run again under the retry loop and every step is
/// idempotent.
async fn upsert_and_record_block(
    txn: &Transaction<'_>,
    source_id: i64,
    payload: &MergeMiningEventPayload,
    observed_at: i64,
) -> Result<EventWriteOutcome> {
    let outcome = upsert_merge_mining_event_with_attributions(txn, source_id, payload).await?;
    let child_height = payload
        .child_height
        .context("Elastos event payload carries no child height")?;
    let child_block_hash = payload
        .child_block_hash
        .as_deref()
        .context("Elastos event payload carries no child block hash")?;
    record_child_chain_block(
        txn,
        source_id,
        child_height,
        ChildChainHeadRecord {
            block_hash: child_block_hash,
            parent: CurrentBlockParent::Known(payload.btc_parent_header_hash.as_slice()),
            outcome: ChildChainHeadOutcome::captured(payload.classification_provisional),
            evidence: EvidenceMarker::None,
            observed_at,
            core_cache_generation: None,
        },
    )
    .await?;
    Ok(outcome)
}

/// Capture a verified block. Builds the evidence directly (child = the Elastos
/// block, parent = the BTC block from the CAuxPow) and runs it through the
/// shared capture seam, whose lineage gate decides whether the parent is
/// Bitcoin's. A refused parent also returns the Core-cache generation its
/// verdict was decided under, for the block's child head.
async fn write_capture(
    client: &mut Client,
    context: &ElastosCaptureContext,
    block: &ElastosBlock,
    parsed: &ParsedAuxpowBlock,
    recon: &ReconstructedBlock,
) -> Result<(ElastosHeightOutcome, Option<i64>)> {
    let parent_attribution = resolve_parent_pool_attribution_from_coinbase(
        &parsed.parent_coinbase_script,
        &parsed.parent_coinbase_output_addresses,
        &context.resolver,
        context.base.pool_ids_by_slug(),
    );
    let mut attributions = parent_attribution.into_iter().collect::<Vec<_>>();
    attributions.extend(resolve_elastos_identity_attributions(
        block,
        &context.child_identities,
    ));
    let pool_attributions = ResolvedPoolAttributions { attributions };

    let evidence = NormalizedEventEvidence {
        child_height: Some(recon.height),
        child_block_hash: Some(recon.block_hash.to_byte_array().to_vec()),
        child_header_bytes: None,
        child_block_time: Some(i64::from(recon.time)),
        child_nbits: None,
        btc_parent_header: parsed.parent_header.header,
        // The own-node child-target check, GATED true to reach this path.
        pow_validates_child_target: Some(validates_target(
            parsed.parent_header.hash(),
            recon.prefix_header.bits(),
        )),
        btc_parent_coinbase_txid: Some(parsed.parent_coinbase_txid.to_byte_array().to_vec()),
        btc_parent_coinbase_script: Some(parsed.parent_coinbase_script.clone()),
        btc_parent_coinbase_outputs: Some(serialize(&parsed.parent_coinbase_outputs)),
        btc_parent_coinbase_outputs_text: None,
        btc_parent_coinbase_tx_bytes: None,
        // Child coinbase absent (the Elastos Go-RPC tx is not coerced into TxOut).
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: Some(parsed.auxpow_bytes.clone()),
    };

    let now = now_epoch_seconds()?;
    let mut payload = build_event_payload_from_evidence(
        evidence,
        pool_attributions,
        ClassificationProof::default(),
        now,
    )?;

    let captured = context
        .base
        .capture(
            client,
            &mut payload,
            "Elastos",
            async |txn, source_id, payload| {
                upsert_and_record_block(txn, source_id, payload, now).await
            },
        )
        .await?;
    Ok(match captured {
        CaptureOutcome::Written(_) => (ElastosHeightOutcome::AuxpowWritten, None),
        CaptureOutcome::NotBitcoin {
            core_cache_generation,
            ..
        } => (
            ElastosHeightOutcome::NonBitcoinParent,
            Some(core_cache_generation),
        ),
        CaptureOutcome::LineagePending(_) => (ElastosHeightOutcome::LineagePending, None),
    })
}

use crate::chains::elastos::rpc::ElastosRpcClient;
use crate::chains::spec::{ChainId, by_id};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress};

/// Elastos live capture chain. Monotonic like the Namecoin family: a pending
/// parent lineage holds the cursor and every other outcome advances.
pub(crate) struct ElastosChainPoller {
    state: ChainPollerState,
    rpc: ElastosRpcClient,
    context: ElastosCaptureContext,
}

impl ElastosChainPoller {
    /// Bundle the owned DB client, RPC client, and per-run context into a poller the
    /// generic `Poller` can drive height by height.
    pub(crate) fn new(
        client: Client,
        rpc: ElastosRpcClient,
        context: ElastosCaptureContext,
    ) -> Self {
        Self {
            state: ChainPollerState::new(by_id(ChainId::Elastos), context.source_id(), client),
            rpc,
            context,
        }
    }
}

impl ChainPoller for ElastosChainPoller {
    fn poller_state(&self) -> &ChainPollerState {
        &self.state
    }

    fn client_mut(&mut self) -> &mut Client {
        &mut self.state.client
    }

    async fn chain_tip(&self) -> Result<i32> {
        self.rpc.get_current_height().await
    }
    fn chain_rpc_metrics(&self) -> Option<mmm_rpc::RpcMetrics> {
        Some(self.rpc.metrics())
    }
    fn parent_classifier(&self) -> Option<&ConfiguredParentClassifier> {
        Some(self.context.parent_classifier())
    }
    fn non_bitcoin_parents(&self) -> usize {
        self.context.base.non_bitcoin_parents()
    }

    async fn refresh_core_cache(&mut self) -> Result<()> {
        self.context
            .refresh_core_header_cache(&mut self.state.client)
            .await?;
        Ok(())
    }

    /// Process one height, then translate the outcome to poll progress: a
    /// parent whose lineage is pending holds the cursor until a later tick's
    /// cache refresh decides it; every other outcome advances.
    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            process_elastos_height(&mut self.state.client, &self.rpc, &self.context, height)
                .await?;
        Ok(match outcome {
            ElastosHeightOutcome::LineagePending => HeightProgress::Hold,
            _ => HeightProgress::Advance,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(fixture: &str) -> ElastosBlock {
        serde_json::from_str(fixture).expect("deserialize Elastos fixture")
    }

    #[test]
    fn evaluates_known_stale_block_as_write() {
        // ELA 360062: a verified stale (BTC parent 572,333) passes recon +
        // commitment + both targets and goes to the lineage-gated write.
        let b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-360062.json"
        )));
        assert!(matches!(
            evaluate_elastos_block(b.height, &b),
            ElastosEvaluation::Write { .. }
        ));
    }

    #[test]
    fn evaluates_dummy_block_as_nonauxpow_skip() {
        // ELA 100000: a pre-activation dummy (parent bits 0, empty outputs) is
        // filtered before commitment verification.
        let b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-100000.json"
        )));
        assert!(matches!(
            evaluate_elastos_block(b.height, &b),
            ElastosEvaluation::Skip {
                outcome: ElastosHeightOutcome::NonAuxpowSkipped,
                ..
            }
        ));
    }

    #[test]
    fn real_post_activation_blocks_clear_commitment() {
        // Real blocks must clear recon + parse + commitment: the verifier
        // generalizes beyond ELA 360062, so the outcome is never Malformed or
        // NonAuxpow. The final outcome (Write / Near / ChildTarget) is
        // parent-dependent, so it is not asserted here.
        for fixture in [
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/elastos/ela-1500000.json"
            )),
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/elastos/ela-2000000.json"
            )),
        ] {
            let b = block(fixture);
            let eval = evaluate_elastos_block(b.height, &b);
            assert!(
                !matches!(
                    eval,
                    ElastosEvaluation::Skip {
                        outcome: ElastosHeightOutcome::MalformedSkipped
                            | ElastosHeightOutcome::NonAuxpowSkipped,
                        ..
                    }
                ),
                "a real Elastos block must clear recon + parse + commitment verification"
            );
        }
    }

    #[test]
    fn hash_mismatch_is_malformed() {
        let mut b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-360062.json"
        )));
        b.hash = "00".repeat(32);
        assert!(matches!(
            evaluate_elastos_block(b.height, &b),
            ElastosEvaluation::Skip {
                outcome: ElastosHeightOutcome::MalformedSkipped,
                ..
            }
        ));
    }

    #[test]
    fn height_mismatch_is_malformed() {
        // A misrouted RPC response for a different height is rejected before any
        // write/revoke, so the producer never acts on the wrong child height.
        let b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-360062.json"
        )));
        assert!(matches!(
            evaluate_elastos_block(b.height + 1, &b),
            ElastosEvaluation::Skip {
                outcome: ElastosHeightOutcome::MalformedSkipped,
                ..
            }
        ));
    }
}
