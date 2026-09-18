//! Hathor capture: the per-height state machine for live polling and bounded
//! backfills.
//!
//! Hathor is the RSK-shaped producer (it reconstructs an 80-byte BTC parent
//! header and hand-builds a [`NormalizedEventEvidence`]), driven over the
//! third-party public REST API. Because the API is untrusted the slice
//! self-verifies the BTC evidence: RFC 0006 reconstruction identity + the
//! "Hath" marker (in [`crate::chains::hathor::auxpow`]) + the block's own
//! Hathor target + the BTC parent PoW + the Core-cache nBits contamination
//! verdict. Hathor DAG membership/height stays RPC-asserted.
//!
//! Hathor blocks can be VOIDED (DAG reorg) or replaced at a height. A replaced
//! block's event is still valid Bitcoin-side evidence, so the state machine
//! never revokes it for that reason: every processed height records which
//! block the child chain carries there (see `docs/data-model.md`, "Child
//! Displacement"), and the earlier event is marked displaced. The endpoint
//! may be untrusted, so a block is recorded only under the rule in
//! [`may_record_current_block`]; `docs/capture.md` states it in prose.
//! Revocation keeps its one meaning, a non-BTC parent or a classifier
//! conflict, applied to the block the verdict was reached on.

use anyhow::{Context, Result};
use bitcoin::Transaction;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash as _;
use tokio_postgres::{Client, GenericClient};
use tracing::{debug, error, warn};

use crate::chains::hathor::auxpow::HathorReconstruction;
use crate::chains::hathor::convert::{block_hash_internal, derive_output_addresses};
use crate::chains::hathor::identity::upsert_hathor_reward_pool_identities;
use crate::chains::hathor::reconstruct::{
    DeclaredWork, HathorParentReconstruction, HathorReconstructedParent, reconstruct_or_skip,
};
use crate::chains::hathor::reward::{HATHOR_REWARD_ADDRESS_NAMESPACE, parse_hathor_reward_outputs};
use crate::chains::hathor::rpc::{HathorBlockMeta, HathorRpc, HathorTransaction};
use crate::chains::nbits_horizon::{HorizonGate, cached_horizon_gate};
use crate::chains::{
    ensure_offline_valid_not_classifier_conflict, is_offline_valid_classifier_conflict,
    revoke_active_block,
};
use crate::producer_runtime::ProducerContext;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::capture::{
    ClassificationProof, HATHOR_PROOF_FORMAT_RFC0006, HATHOR_REVOKE_NBITS_CONFLICT,
    HATHOR_REVOKE_NON_BTC, HathorEvidencePayload, NormalizedEventEvidence,
    ResolvedPoolAttributions, build_event_payload_from_evidence, now_epoch_seconds,
    resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::nbits_table::{NbitsLookup, NbitsTable, NbitsVerdict};
use mmm_capture::pool_resolver::PoolResolver;
use mmm_capture::source_registry::HATHOR_SOURCE_CODE;
use mmm_read_model::capture_in_txn;
use mmm_store::{
    ChildChainHeadOutcome, CurrentBlockParent, finish_child_chain_height_operation,
    hathor_sidecar_graph_heads_at_height, load_child_chain_head, load_pool_identities_by_namespace,
    lock_child_chain_height_session, record_child_chain_block,
    record_child_chain_block_in_own_transaction, write_hathor_capture_in_txn,
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

/// What a run's captures say about the live child chain, which decides
/// whether a height records which block the chain carries there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainObservation {
    /// Live polling or a bounded backfill over the REST API. `fork_window`
    /// is the deepest fork the run reconciles by displacement: the
    /// configured rescan depth, bounded at
    /// [`HATHOR_MAX_DISPLACEMENT_FORK_DEPTH`] on construction. A batch's
    /// range is not a bound on fork depth.
    Live { fork_window: i32 },
    /// An archive replay: a snapshot of the chain as it was, not an
    /// observation of it as it is. Events are written, but no height records
    /// which block the chain carries, so an old snapshot never restores a
    /// block the live chain has since replaced.
    ArchiveReplay,
}

/// Per-source capture state shared across every height in a poll/backfill run:
/// the pool resolver, the bootstrapped [`ProducerContext`] (source id +
/// slug->pool-id map + classifier), and the namespace-scoped reward-address
/// identity lookup. Built once so the per-height path does no I/O for these.
#[derive(Debug)]
pub struct HathorCaptureContext {
    resolver: PoolResolver,
    base: ProducerContext,
    /// Hathor reward-registry identities keyed under
    /// [`HATHOR_REWARD_ADDRESS_NAMESPACE`], resolved once for child-reward
    /// attribution.
    reward_identities: PoolIdentityLookup,
    observation: ChainObservation,
}

impl HathorCaptureContext {
    /// Bootstrap the source row, seed the Hathor reward-registry pools and
    /// identities, then load those identities by namespace. The classifier is
    /// threaded through so the same capture path serves both an enabled
    /// (Core-backed) and a disabled run.
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

    /// The `merge_mining_event.source_id` every Hathor write is tagged with.
    pub fn source_id(&self) -> i64 {
        self.base.source_id()
    }

    /// The configured BTC parent classifier, threaded into every capture and
    /// revoke so the read-model reconcile runs under the same placement policy.
    pub fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        self.base.parent_classifier()
    }

    pub(super) async fn refresh_core_header_cache(&self, client: &mut Client) -> Result<()> {
        self.base.refresh_core_header_cache(client).await
    }
}

/// Per-height capture outcome. The poller maps the `*Hold` variants to
/// [`crate::poller::HeightProgress`]; the cursor-blocking `TableHorizonHold`
/// becomes `Abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HathorHeightOutcome {
    /// A verified BTC-parent event was written (or restored-and-refreshed).
    AuxpowWritten,
    /// A non-merge-mined (version != 3) block; no event, nothing recorded.
    NonAuxpowSkipped,
    /// A voided (DAG-reorged) block; no event, nothing recorded.
    VoidedSkipped,
    /// A proven block whose parent misses BTC's target (the common merge-mined
    /// case); no event, but the block is recorded as the chain's block.
    NearSkipped,
    /// The proof was malformed / inconsistent and was skipped without a write.
    MalformedSkipped,
    /// A validated Hathor block whose parent is non-BTC (BCH contaminant or
    /// indeterminate); no event.
    NonBtcParentSkipped,
    /// The Core-cache nBits verdict was Valid but the classifier contradicted
    /// it (difficulty_epoch_ok = false); the write was blocked.
    ConflictSkipped,
    /// The block was definitively absent (best-effort hold).
    AbsentHold,
    /// A transient REST failure, or a response for another height than the
    /// one asked for (best-effort hold: retried, never counted as processed).
    TransientHold,
    /// The parent BIP34 height is beyond the Core-cache horizon (cursor-blocking).
    TableHorizonHold,
}

/// Everything assembled offline from a reconstructed parent, before the write
/// decision. Built first so the `verdict` (Valid / non-BTC / above-horizon)
/// can route to write-vs-revoke without re-deriving the evidence.
struct BuiltCapture {
    evidence: NormalizedEventEvidence,
    sidecar: HathorEvidencePayload,
    pool_attributions: ResolvedPoolAttributions,
    verdict: NbitsVerdict,
    bip34_height: Option<i32>,
}

/// Fetch-result decision before any DB mutation. This keeps voided DAG state and
/// non-AuxPoW canonical blocks out of the version-3 proof-validation path.
enum HathorBlockDecision {
    Voided,
    NonAuxpow,
    Auxpow {
        current_hash: Vec<u8>,
        tx: HathorTransaction,
    },
}

/// Inner `Err` is a handled per-height outcome (absent, transient, malformed).
type HathorBlockLoad = std::result::Result<HathorBlockDecision, HathorHeightOutcome>;

/// Drive one Hathor height through the validate-before-mutate state machine.
///
/// Every processed height also records which block the child chain carries
/// there (see `docs/data-model.md`, "Child Displacement"): a captured block
/// inside its capture transaction, any other reconstructed block in a
/// transaction of its own, both under the rule in
/// [`may_record_current_block`]. A response for another height than the one
/// asked for holds the height for a retry; the position itself stays the
/// endpoint's assertion, as it is for every captured event's child height.
/// The whole height, from the REST fetch through the last write, runs under a
/// session-level lock on `(source, height)`, so what was observed is what is
/// recorded even when a poller and a backfill overlap on the height.
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

/// Re-observe an already-processed height inside the trailing rescan window.
///
/// The `/block_at_height` metadata alone names the block, so when the chain
/// still carries the block the `child_chain_head` row recorded with a final
/// outcome, the `/transaction` fetch, the reconstruction and the write path
/// are skipped; displacement maintenance still runs under the height lock,
/// with the record the original capture made. Any other case runs the full
/// capture on the metadata already fetched. Voided, absent, misrouted and
/// non-merge-mined blocks behave exactly as in [`process_hathor_height`].
pub async fn rescan_hathor_height(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    height: i32,
) -> Result<HathorRescanOutcome> {
    let source_id = context.source_id();
    lock_child_chain_height_session(client, source_id, height).await?;
    let result = async {
        let block = match fetch_hathor_block_meta(rpc, height).await? {
            Ok(block) => block,
            Err(outcome) => return Ok(HathorRescanOutcome::Captured(outcome)),
        };
        if block.height == height
            && !block.is_voided
            && block.version == 3
            && let Ok(current_hash) = block_hash_internal(&block.tx_id)
            && let Some(head) = load_child_chain_head(&*client, source_id, height).await?
            && head.is_final()
            && head.block_hash == current_hash
        {
            record_child_chain_block_in_own_transaction(
                client,
                source_id,
                height,
                &current_hash,
                head.current_parent(),
                head.outcome,
                now_epoch_seconds()?,
            )
            .await?;
            return Ok(HathorRescanOutcome::Unchanged);
        }
        process_locked_block(client, rpc, context, height, block)
            .await
            .map(HathorRescanOutcome::Captured)
    }
    .await;
    finish_child_chain_height_operation(client, source_id, height, result).await
}

/// What a rescan of one height did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HathorRescanOutcome {
    /// The chain still carries the recorded block; only the block metadata
    /// was fetched and the displacement maintenance run.
    Unchanged,
    /// The height was captured again, with this outcome.
    Captured(HathorHeightOutcome),
}

/// One height's fetch, gates and writes, under the session-level height lock
/// [`process_hathor_height`] holds around it.
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

/// The gates and writes for block metadata already fetched at `height`.
async fn process_locked_block(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    height: i32,
    block: HathorBlockMeta,
) -> Result<HathorHeightOutcome> {
    let block = match classify_hathor_block(rpc, height, block).await? {
        Ok(block) => block,
        Err(outcome) => return Ok(outcome),
    };
    let (current_hash, tx) = match block {
        HathorBlockDecision::Voided => return Ok(HathorHeightOutcome::VoidedSkipped),
        HathorBlockDecision::NonAuxpow => return Ok(HathorHeightOutcome::NonAuxpowSkipped),
        HathorBlockDecision::Auxpow { current_hash, tx } => (current_hash, tx),
    };

    // Reconstruction is pure CPU and decides the common near case, so it runs
    // before the Core-cache lock and the nBits table load only a BTC-valid
    // parent needs.
    let (outcome, work) = match reconstruct_or_skip(height, &tx)? {
        HathorParentReconstruction::Malformed => (HathorHeightOutcome::MalformedSkipped, None),
        HathorParentReconstruction::Near(work) => (HathorHeightOutcome::NearSkipped, Some(work)),
        HathorParentReconstruction::BtcValid(parent) => {
            let work = parent.work;
            let outcome =
                classify_and_write_btc_valid(client, context, height, &current_hash, &tx, parent)
                    .await?;
            (outcome, Some(work))
        }
    };

    // A captured block was recorded inside its capture transaction; any
    // other reconstructed block is recorded here, in a transaction of its
    // own: a near, non-BTC, held or conflicting parent is still the parent of
    // the block the chain carries.
    if outcome != HathorHeightOutcome::AuxpowWritten
        && let Some(work) = work
        && may_record_current_block(&*client, context, height, work).await?
    {
        // A near parent is a settled proof-of-work comparison; a non-BTC or
        // conflicting verdict follows the Core cache and may change when the
        // height is observed again, so it is not final for a rescan.
        let head_outcome = match outcome {
            HathorHeightOutcome::NearSkipped => ChildChainHeadOutcome::Recorded,
            _ => ChildChainHeadOutcome::Unverified,
        };
        record_child_chain_block_in_own_transaction(
            client,
            context.source_id(),
            height,
            &current_hash,
            CurrentBlockParent::Known(&current_hash),
            head_outcome,
            now_epoch_seconds()?,
        )
        .await?;
    }
    Ok(outcome)
}

async fn fetch_hathor_block_meta(
    rpc: &impl HathorRpc,
    height: i32,
) -> Result<std::result::Result<HathorBlockMeta, HathorHeightOutcome>> {
    // Fetch the current block. A transient failure holds without mutating; a
    // definitive absence (Ok(None)) is also a no-mutation hold.
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
    block: HathorBlockMeta,
) -> Result<HathorBlockLoad> {
    // Untrusted-endpoint guard: the response must answer for the height asked
    // for, or a stale or misrouted response would write, revoke or record at
    // the WRONG child height. A hold, not a skip: the height is not processed,
    // so the poller queues a durable retry and a backfill does not count it.
    if block.height != height {
        warn!(
            requested = height,
            returned = block.height,
            "Hathor /block_at_height answered for a different height; holding"
        );
        return Ok(Err(HathorHeightOutcome::TransientHold));
    }

    // A voided block is a definitive child-DAG signal that this block is not
    // the chain's block at the height, but it names no replacement: nothing is
    // written, and nothing is recorded until the replacement is observed. The
    // block hash is not needed on this path.
    if block.is_voided {
        return Ok(Ok(HathorBlockDecision::Voided));
    }

    let current_hash = match hathor_block_hash_or_skip(height, &block.tx_id)? {
        Ok(hash) => hash,
        Err(outcome) => return Ok(Err(outcome)),
    };

    // A non-merge-mined (version != 3) block carries no proof this producer
    // verifies (no /transaction needed): no event, and not recorded as the
    // chain's block either, since the endpoint may be untrusted.
    if block.version != 3 {
        return Ok(Ok(HathorBlockDecision::NonAuxpow));
    }

    let tx = match fetch_validated_hathor_transaction(rpc, height, &block.tx_id).await? {
        Ok(tx) => tx,
        Err(outcome) => return Ok(Err(outcome)),
    };
    Ok(Ok(HathorBlockDecision::Auxpow { current_hash, tx }))
}

fn hathor_block_hash_or_skip(
    height: i32,
    tx_id: &str,
) -> Result<std::result::Result<Vec<u8>, HathorHeightOutcome>> {
    // From here the block hash is required; a malformed tx_id from the untrusted
    // REST API is a skip, not a hard error that would abort the whole tick.
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
    // version == 3: VALIDATE the replacement before mutating anything.
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

/// Classify a reconstructed BTC-valid parent against the Core cache and
/// apply its verdict, under the shared cache lock.
async fn classify_and_write_btc_valid(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    current_hash: &[u8],
    tx: &HathorTransaction,
    parent: HathorReconstructedParent,
) -> Result<HathorHeightOutcome> {
    mmm_store::lock_bitcoin_core_header_cache_shared(client).await?;
    let result = async {
        let nbits_table = mmm_store::load_bitcoin_core_nbits_table(client).await?;
        let HathorReconstructedParent {
            raw,
            aux_pow,
            recon,
            work,
        } = parent;
        // Reuse the prefix length reconstruct already computed; no second scan of raw.
        let funds_graph = &raw[..recon.funds_graph_len];
        match build_hathor_capture(
            context,
            tx,
            height,
            &aux_pow,
            &recon,
            funds_graph,
            &nbits_table,
        )? {
            Some(built) => {
                apply_hathor_verdict(
                    client,
                    context,
                    height,
                    current_hash,
                    work,
                    built,
                    &nbits_table,
                )
                .await
            }
            // The block reconstructed; only its reconstructed coinbase is unusable.
            None => Ok(HathorHeightOutcome::MalformedSkipped),
        }
    }
    .await;
    mmm_store::finish_bitcoin_core_header_cache_shared_operation(client, result).await
}

/// Whether a block declaring `work` may be recorded as the chain's block at
/// the height. Never under an archive replay. Otherwise only when a block
/// captured at the height has a sidecar to hold it against, since nothing
/// else there could be displaced, and its weight clears every captured
/// block's floor: that block's weight less what Hathor's difficulty
/// adjustment could have moved across the fork between them, one step each
/// way when the two share a parent (the common one-deep reorg), otherwise a
/// step per block on each branch across the run's window. The weight a
/// response declares is the endpoint's claim, and the target it sets is met
/// by whatever work the response carries; this floor, a fixed fraction of a
/// captured block's work, is what a fabricated replacement cannot clear. A
/// captured block's own sidecar counts, so a first capture at a height
/// records. A merge-mined Hathor block's hash is its BTC parent header's
/// hash, so the recorded block names its own parent.
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
        debug!(
            height,
            "no captured Hathor block at the height to hold the declared weight against; \
             not recorded"
        );
        return Ok(false);
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

async fn apply_hathor_verdict(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    current_hash: &[u8],
    work: DeclaredWork,
    built: BuiltCapture,
    nbits_table: &NbitsTable,
) -> Result<HathorHeightOutcome> {
    match built.verdict {
        NbitsVerdict::AboveTableHorizon => {
            let bip34_height = built
                .bip34_height
                .expect("AboveTableHorizon requires a parsed BIP34 height");
            match cached_horizon_gate(
                context.parent_classifier(),
                nbits_table.horizon_height(),
                bip34_height,
            )
            .await
            {
                HorizonGate::FarFuture => {
                    revoke_active_block(
                        client,
                        &context.base,
                        height,
                        current_hash,
                        current_hash,
                        HATHOR_REVOKE_NON_BTC,
                    )
                    .await?;
                    Ok(HathorHeightOutcome::NonBtcParentSkipped)
                }
                HorizonGate::Hold | HorizonGate::WithinTip => {
                    Ok(HathorHeightOutcome::TableHorizonHold)
                }
            }
        }
        NbitsVerdict::Contaminant | NbitsVerdict::Indeterminate => {
            // A validated Hathor block with a non-BTC parent writes no event AND
            // revokes this block's own active capture, a row whose verdict
            // flipped after capture (e.g. a Core-cache correction). The non-BTC
            // reason is reversible, so a later re-Valid recapture restores it.
            revoke_active_block(
                client,
                &context.base,
                height,
                current_hash,
                current_hash,
                HATHOR_REVOKE_NON_BTC,
            )
            .await?;
            Ok(HathorHeightOutcome::NonBtcParentSkipped)
        }
        NbitsVerdict::Valid => {
            // A matching nBits value is not enough to write a claimed BIP34
            // height beyond the persisted Core horizon, even within the current
            // difficulty epoch. A fresh tip can demote a clearly fabricated
            // claim; all other unobserved claims hold for a cache refresh.
            if let Some(height_claim) = built.bip34_height {
                match cached_horizon_gate(
                    context.parent_classifier(),
                    nbits_table.horizon_height(),
                    height_claim,
                )
                .await
                {
                    HorizonGate::FarFuture => {
                        revoke_active_block(
                            client,
                            &context.base,
                            height,
                            current_hash,
                            current_hash,
                            HATHOR_REVOKE_NON_BTC,
                        )
                        .await?;
                        Ok(HathorHeightOutcome::NonBtcParentSkipped)
                    }
                    HorizonGate::Hold => Ok(HathorHeightOutcome::TableHorizonHold),
                    HorizonGate::WithinTip => {
                        write_valid_capture(client, context, height, current_hash, work, built)
                            .await
                    }
                }
            } else {
                write_valid_capture(client, context, height, current_hash, work, built).await
            }
        }
    }
}

/// Write a Valid BTC-parent capture: the event and sidecar, then the record
/// that this block is the chain's block there (under
/// [`may_record_current_block`]), all in the capture transaction under the
/// per-height lock it takes first. Nothing remains to complete after the
/// commit.
async fn write_valid_capture(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    current_hash: &[u8],
    work: DeclaredWork,
    built: BuiltCapture,
) -> Result<HathorHeightOutcome> {
    let now = now_epoch_seconds()?;
    let mut payload = build_event_payload_from_evidence(
        built.evidence,
        built.pool_attributions,
        ClassificationProof::default(),
        now,
    )?;
    let sidecar = built.sidecar;

    let write_result = capture_in_txn(
        client,
        context.source_id(),
        context.parent_classifier(),
        &mut payload,
        "Hathor",
        async |txn, source_id, payload| {
            // Capture-time pre-upsert guard: only Valid rows reach here, so a
            // preclassified difficulty_epoch_ok == Some(false) contradicts the
            // offline verdict; abort rather than store the contradiction.
            ensure_offline_valid_not_classifier_conflict(payload)?;
            let outcome = write_hathor_capture_in_txn(txn, source_id, payload, &sidecar).await?;
            if may_record_current_block(txn, context, height, work).await? {
                record_child_chain_block(
                    txn,
                    source_id,
                    height,
                    current_hash,
                    CurrentBlockParent::Known(current_hash),
                    ChildChainHeadOutcome::Captured,
                    now,
                )
                .await?;
            }
            Ok(outcome)
        },
    )
    .await;

    match write_result {
        Ok(_event_id) => Ok(HathorHeightOutcome::AuxpowWritten),
        Err(err) if is_offline_valid_classifier_conflict(&err) => {
            warn!(
                height,
                "Hathor offline-Valid row conflicts with classifier; write blocked"
            );
            // The current block conflicts with the classifier: revoke its own
            // active capture stickily (never auto-restored). No replacement was
            // written; the caller still records the block as the chain's.
            revoke_active_block(
                client,
                &context.base,
                height,
                current_hash,
                current_hash,
                HATHOR_REVOKE_NBITS_CONFLICT,
            )
            .await?;
            Ok(HathorHeightOutcome::ConflictSkipped)
        }
        Err(err) => Err(err).with_context(|| format!("Hathor capture at height {height}")),
    }
}

/// Build the hand-assembled evidence + sidecar + pool ids + nBits verdict from a
/// reconstructed parent. Deserializes the reconstructed coinbase once.
///
/// Returns `Ok(None)` when the reconstructed bytes do not deserialize as a
/// transaction with coinbase input structure (trailing bytes, a non-coinbase
/// prevout, or no input). The parent passed its own PoW target to get here, but
/// the coinbase bytes are untrusted reconstruction output: a malformed one is
/// a per-block skip like every other format violation, NOT an error - an `Err`
/// here would fail the live tick and pin the poller on that height forever
/// (observed on archive height 1292779, whose RFC-0006 coinbase carries
/// trailing bytes).
fn build_hathor_capture(
    context: &HathorCaptureContext,
    tx: &HathorTransaction,
    hathor_height: i32,
    aux_pow: &[u8],
    recon: &HathorReconstruction,
    funds_graph: &[u8],
    nbits_table: &NbitsTable,
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
    let bip34_height = parse_bip34_height(&script_sig);
    let output_addresses = derive_output_addresses(&coinbase);
    let coinbase_txid = coinbase.compute_txid();

    let nbits = recon.header.bits;
    let verdict = nbits_table.classify_nbits(bip34_height, nbits);
    let expected_btc_nbits = match nbits_table.expected_nbits(bip34_height.unwrap_or(-1)) {
        NbitsLookup::Found(bits) => i64::from(bits),
        _ => i64::from(nbits.to_consensus()),
    };

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
        // The Hathor block hash IS the reconstructed BTC parent header hash.
        child_block_hash: Some(block_hash_bytes.clone()),
        child_header_bytes: None,
        child_block_time: Some(tx.timestamp),
        child_nbits: None,
        btc_parent_header: recon.header,
        // Hathor has no consensus-grade child nBits target in the REST payload;
        // leave the child-target verdict NULL like RSK.
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
        expected_btc_nbits,
        proof_format: HATHOR_PROOF_FORMAT_RFC0006,
    };

    Ok(Some(BuiltCapture {
        evidence,
        sidecar,
        pool_attributions,
        verdict,
        bip34_height,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valid reconstructed coinbase bytes are retained, while malformed or
    /// non-coinbase transactions skip without pinning the live poller.
    #[test]
    fn reconstructed_coinbase_validation_preserves_valid_and_skips_invalid() {
        let context = HathorCaptureContext {
            resolver: PoolResolver::from_default_snapshot().unwrap(),
            reward_identities: std::collections::HashMap::new(),
            observation: ChainObservation::Live { fork_window: 20 },
            base: crate::producer_runtime::ProducerContext::from_parts(
                std::collections::HashMap::new(),
                1,
                ConfiguredParentClassifier::Disabled,
            ),
        };
        let (tx, height) = fixture_tx(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hathor/1971823.json"
        )));
        let HathorParentReconstruction::BtcValid(reconstructed) =
            reconstruct_or_skip(height, &tx).unwrap()
        else {
            panic!("the fixture must reconstruct as a BTC-valid parent");
        };
        let raw = reconstructed.raw;
        let aux_pow = reconstructed.aux_pow;
        let mut recon = reconstructed.recon;
        let funds_graph = &raw[..recon.funds_graph_len];
        let nbits_table = NbitsTable::from_bitcoin_core_headers(&[
            mmm_capture::nbits_table::BitcoinEpochHeader {
                height: 0,
                block_time: 1,
                bits: 0x1d00_ffff,
            },
        ])
        .expect("the minimal test Core header cache is valid");

        let intact = build_hathor_capture(
            &context,
            &tx,
            height,
            &aux_pow,
            &recon,
            funds_graph,
            &nbits_table,
        )
        .unwrap()
        .expect("fixture coinbase must build");
        assert_eq!(
            intact.evidence.btc_parent_coinbase_tx_bytes.as_deref(),
            Some(recon.full_coinbase.as_slice()),
            "validated reconstructed coinbase bytes must be retained"
        );

        let pristine_coinbase = recon.full_coinbase.clone();
        recon.full_coinbase.push(0x00);
        let corrupted = build_hathor_capture(
            &context,
            &tx,
            height,
            &aux_pow,
            &recon,
            funds_graph,
            &nbits_table,
        )
        .unwrap();
        assert!(
            corrupted.is_none(),
            "trailing-byte coinbase must skip, not error"
        );

        let mut non_coinbase: Transaction = deserialize(&pristine_coinbase).unwrap();
        non_coinbase.input[0].previous_output =
            bitcoin::OutPoint::new(bitcoin::Txid::from_byte_array([1; 32]), 0);
        assert!(!non_coinbase.is_coinbase());
        recon.full_coinbase = serialize(&non_coinbase);

        let built = build_hathor_capture(
            &context,
            &tx,
            height,
            &aux_pow,
            &recon,
            funds_graph,
            &nbits_table,
        )
        .unwrap();
        assert!(
            built.is_none(),
            "well-formed non-coinbase transaction must skip"
        );
    }

    fn fixture_tx(json: &str) -> (HathorTransaction, i32) {
        let j: serde_json::Value = serde_json::from_str(json).unwrap();
        (
            HathorTransaction {
                raw: j["raw_hex"].as_str().unwrap().to_owned(),
                aux_pow: Some(j["aux_pow_hex"].as_str().unwrap().to_owned()),
                hash: j["tx_id"].as_str().unwrap().to_owned(),
                timestamp: j["timestamp"].as_i64().unwrap_or(0),
            },
            j["hathor_height"].as_i64().unwrap() as i32,
        )
    }
}
