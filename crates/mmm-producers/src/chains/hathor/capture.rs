//! Hathor capture: the per-height state machine for live polling and bounded
//! backfills.
//!
//! Hathor is the RSK-shaped producer (it reconstructs an 80-byte BTC parent
//! header and hand-builds a [`NormalizedEventEvidence`]), driven over the
//! third-party public REST API. Because the API is untrusted the slice
//! self-verifies the BTC evidence: RFC 0006 reconstruction identity + the
//! "Hath" marker (in [`crate::chains::hathor::auxpow`]) + the BTC parent PoW + the
//! offline nBits contamination verdict. Hathor DAG membership/height stays
//! RPC-asserted.
//!
//! Hathor blocks can be VOIDED (DAG reorg) or replaced at a height, so the
//! state machine VALIDATES a replacement before mutating any prior capture, and
//! a same-height supersession is write-before-revoke with a durable
//! `poll_pending_reconcile` marker so a crash between the write and the revoke
//! self-heals.

use anyhow::{Context, Result};
use bitcoin::Transaction;
use bitcoin::consensus::{deserialize, serialize};
use bitcoin::hashes::Hash as _;
use tokio_postgres::Client;
use tracing::{error, warn};

use crate::chains::hathor::auxpow::HathorReconstruction;
use crate::chains::hathor::convert::{block_hash_internal, derive_output_addresses};
use crate::chains::hathor::identity::upsert_hathor_reward_pool_identities;
use crate::chains::hathor::reconstruct::{HathorReconstructedParent, reconstruct_or_skip};
use crate::chains::hathor::reward::{HATHOR_REWARD_ADDRESS_NAMESPACE, parse_hathor_reward_outputs};
use crate::chains::hathor::rpc::{HathorBlockMeta, HathorRpc, HathorTransaction};
use crate::chains::nbits_horizon::{
    HorizonOutcome, far_future_against_fresh_tip, resolve_horizon_nbits,
};
use crate::chains::{
    ensure_offline_valid_not_classifier_conflict, is_offline_valid_classifier_conflict,
};
use crate::producer_runtime::ProducerContext;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::auxpow::parse_bip34_height;
use mmm_capture::capture::{
    ClassificationProof, HATHOR_PROOF_FORMAT_RFC0006, HATHOR_REVOKE_NBITS_CONFLICT,
    HATHOR_REVOKE_NON_BTC, HATHOR_REVOKE_SUPERSEDED, HATHOR_REVOKE_VOIDED, HathorEvidencePayload,
    NormalizedEventEvidence, ResolvedPoolAttributions, build_event_payload_from_evidence,
    now_epoch_seconds, resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::nbits_table::{NbitsLookup, NbitsVerdict, classify_nbits, table};
use mmm_capture::pool_resolver::PoolResolver;
use mmm_capture::source_registry::HATHOR_SOURCE_CODE;
use mmm_read_model::capture_in_txn;
use mmm_read_model::revoke_merge_mining_event;
use mmm_store::{
    HathorEventRow, delete_pending_reconcile_at, hathor_events_at_height,
    load_pool_identities_by_namespace, upsert_pending_reconcile, write_hathor_capture_in_txn,
};

/// Pending-queue kind for a durable in-progress supersession marker.
pub(super) const PENDING_KIND_SUPERSEDE: &str = "supersede";
/// Pending-queue kind for a held rescan height awaiting retry.
pub(super) const PENDING_KIND_RECONCILE: &str = "reconcile";

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
}

impl HathorCaptureContext {
    /// Bootstrap the source row, seed the Hathor reward-registry pools and
    /// identities, then load those identities by namespace. The classifier is
    /// threaded through so the same capture path serves both an enabled
    /// (Core-backed) and a disabled run.
    pub async fn new_with_classifier(
        client: &Client,
        parent_classifier: ConfiguredParentClassifier,
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
        Ok(Self {
            resolver,
            base,
            reward_identities,
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
}

/// Per-height capture outcome. The poller maps the `*Hold` variants to
/// [`crate::poller::HeightProgress`]; the cursor-blocking `TableHorizonHold`
/// becomes `Abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HathorHeightOutcome {
    /// A verified BTC-parent event was written (or restored-and-refreshed).
    AuxpowWritten,
    /// A non-merge-mined (version != 3) block; no event.
    NonAuxpowSkipped,
    /// A voided (DAG-reorged) block; any active prior was revoked.
    VoidedSkipped,
    /// The proof was malformed / inconsistent and was skipped without a write.
    MalformedSkipped,
    /// A validated Hathor block whose parent is non-BTC (BCH contaminant or
    /// indeterminate); no event.
    NonBtcParentSkipped,
    /// The offline nBits verdict was Valid but an enabled classifier contradicted
    /// it (difficulty_epoch_ok = false); the write was blocked.
    ConflictSkipped,
    /// The block was definitively absent (best-effort hold).
    AbsentHold,
    /// A transient REST failure (best-effort hold).
    TransientHold,
    /// The parent BIP34 height is beyond the nBits-table horizon (cursor-blocking).
    TableHorizonHold,
}

/// Everything assembled offline from a reconstructed parent, before the write
/// decision. Built first so the `verdict` (Valid / non-BTC / above-horizon)
/// can route to write-vs-revoke without re-deriving the evidence. `bip34_height`
/// is carried so the above-horizon arm can resolve the epoch nBits from Core.
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
    NonAuxpow {
        current_hash: Vec<u8>,
    },
    Auxpow {
        current_hash: Vec<u8>,
        tx: HathorTransaction,
    },
}

/// Inner `Err` is a handled per-height outcome (absent, transient, malformed).
type HathorBlockLoad = std::result::Result<HathorBlockDecision, HathorHeightOutcome>;

/// Drive one Hathor height through the validate-before-mutate state machine.
pub async fn process_hathor_height(
    client: &mut Client,
    rpc: &impl HathorRpc,
    context: &HathorCaptureContext,
    height: i32,
) -> Result<HathorHeightOutcome> {
    let block = match load_hathor_block_decision(rpc, height).await? {
        Ok(block) => block,
        Err(outcome) => return Ok(outcome),
    };
    let prior = hathor_events_at_height(client, context.source_id(), height).await?;

    match block {
        HathorBlockDecision::Voided => {
            revoke_matching(client, context, &prior, |_| true, HATHOR_REVOKE_VOIDED).await?;
            Ok(HathorHeightOutcome::VoidedSkipped)
        }
        HathorBlockDecision::NonAuxpow { current_hash } => {
            revoke_superseded(client, context, &prior, &current_hash).await?;
            Ok(HathorHeightOutcome::NonAuxpowSkipped)
        }
        HathorBlockDecision::Auxpow { current_hash, tx } => {
            process_validated_hathor_auxpow(client, context, height, &prior, &current_hash, tx)
                .await
        }
    }
}

async fn load_hathor_block_decision(rpc: &impl HathorRpc, height: i32) -> Result<HathorBlockLoad> {
    let block = match fetch_hathor_block_meta(rpc, height).await? {
        Ok(block) => block,
        Err(outcome) => return Ok(Err(outcome)),
    };
    classify_hathor_block(rpc, height, block).await
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
    // A voided block is a definitive child-DAG signal: revoke any active prior
    // and write nothing. The block hash is not needed on this path.
    if block.is_voided {
        return Ok(Ok(HathorBlockDecision::Voided));
    }

    let current_hash = match hathor_block_hash_or_skip(height, &block.tx_id)? {
        Ok(hash) => hash,
        Err(outcome) => return Ok(Err(outcome)),
    };

    // A non-merge-mined (version != 3) block is a confirmed canonical block (no
    // /transaction needed): supersede any active prior with a different hash.
    if block.version != 3 {
        return Ok(Ok(HathorBlockDecision::NonAuxpow { current_hash }));
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

async fn process_validated_hathor_auxpow(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    prior: &[HathorEventRow],
    current_hash: &[u8],
    tx: HathorTransaction,
) -> Result<HathorHeightOutcome> {
    let Some(reconstruction) = reconstruct_or_skip(height, &tx)? else {
        return Ok(HathorHeightOutcome::MalformedSkipped);
    };
    let HathorReconstructedParent {
        raw,
        aux_pow,
        recon,
    } = reconstruction;

    // Reuse the prefix length reconstruct already computed; no second scan of raw.
    let funds_graph = &raw[..recon.funds_graph_len];
    let Some(built) = build_hathor_capture(context, &tx, height, &aux_pow, &recon, funds_graph)?
    else {
        return Ok(HathorHeightOutcome::MalformedSkipped);
    };

    apply_hathor_verdict(client, context, height, prior, current_hash, built).await
}

async fn apply_hathor_verdict(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    prior: &[HathorEventRow],
    current_hash: &[u8],
    built: BuiltCapture,
) -> Result<HathorHeightOutcome> {
    match built.verdict {
        NbitsVerdict::AboveTableHorizon => {
            // Beyond the embedded table: resolve the canonical epoch nBits from
            // Bitcoin Core instead of cursor-pinning. Core-disabled (offline
            // backfill / cache ingest) reports no synced tip, so this holds exactly
            // as before. A match writes, a mismatch / fabricated far-future height
            // revokes, an unanswerable Core holds.
            let bip34_height = built
                .bip34_height
                .expect("AboveTableHorizon requires a parsed BIP34 height");
            let actual_bits = built.evidence.btc_parent_header.bits;
            match resolve_horizon_nbits(context.parent_classifier(), bip34_height, actual_bits)
                .await
            {
                HorizonOutcome::Valid => {
                    write_valid_capture(client, context, height, prior, current_hash, built).await
                }
                HorizonOutcome::Contaminant | HorizonOutcome::FarFuture => {
                    revoke_current_and_superseded(
                        client,
                        context,
                        prior,
                        current_hash,
                        HATHOR_REVOKE_NON_BTC,
                    )
                    .await?;
                    Ok(HathorHeightOutcome::NonBtcParentSkipped)
                }
                HorizonOutcome::Hold => Ok(HathorHeightOutcome::TableHorizonHold),
            }
        }
        NbitsVerdict::Contaminant | NbitsVerdict::Indeterminate => {
            // A validated Hathor block with a non-BTC parent writes no event AND
            // revokes any active capture at this height, including a SAME-HASH row
            // whose verdict flipped after capture (e.g. an nBits-table
            // correction). The non-BTC reason is reversible, so a later re-Valid
            // recapture restores it.
            revoke_current_and_superseded(
                client,
                context,
                prior,
                current_hash,
                HATHOR_REVOKE_NON_BTC,
            )
            .await?;
            Ok(HathorHeightOutcome::NonBtcParentSkipped)
        }
        NbitsVerdict::Valid => {
            // Guard the in-table Valid path too: a fabricated far-future BIP34 height
            // inside a covered epoch (whose nBits happened to match) must be revoked,
            // not written, when a fresh synced Core tip proves it fabricated.
            if let Some(height_claim) = built.bip34_height
                && far_future_against_fresh_tip(context.parent_classifier(), height_claim).await
            {
                revoke_current_and_superseded(
                    client,
                    context,
                    prior,
                    current_hash,
                    HATHOR_REVOKE_NON_BTC,
                )
                .await?;
                Ok(HathorHeightOutcome::NonBtcParentSkipped)
            } else {
                write_valid_capture(client, context, height, prior, current_hash, built).await
            }
        }
    }
}

/// Write a Valid BTC-parent capture: a durable supersede marker, then the
/// event and sidecar (write-before-revoke), then revoke superseded priors and
/// clear the marker. A crash between write and revoke self-heals on the next
/// drain.
async fn write_valid_capture(
    client: &mut Client,
    context: &HathorCaptureContext,
    height: i32,
    prior: &[HathorEventRow],
    current_hash: &[u8],
    built: BuiltCapture,
) -> Result<HathorHeightOutcome> {
    let now = now_epoch_seconds()?;
    let superseded: Vec<i64> = prior
        .iter()
        .filter(|e| e.is_active && e.child_block_hash != current_hash)
        .map(|e| e.event_id)
        .collect();

    if !superseded.is_empty() {
        upsert_pending_reconcile(
            client,
            context.source_id(),
            height,
            PENDING_KIND_SUPERSEDE,
            Some(current_hash.to_vec()),
            Some(superseded.clone()),
            Some(HATHOR_REVOKE_SUPERSEDED),
        )
        .await?;
    }

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
            write_hathor_capture_in_txn(txn, source_id, payload, &sidecar).await
        },
    )
    .await;

    match write_result {
        Ok(_event_id) => {
            // Y now exists: revoke the superseded priors, then clear the marker.
            if !superseded.is_empty() {
                revoke_superseded(client, context, prior, current_hash).await?;
                delete_pending_reconcile_at(
                    client,
                    context.source_id(),
                    height,
                    PENDING_KIND_SUPERSEDE,
                )
                .await?;
            }
            Ok(HathorHeightOutcome::AuxpowWritten)
        }
        Err(err) if is_offline_valid_classifier_conflict(&err) => {
            warn!(
                height,
                "Hathor offline-Valid row conflicts with classifier; write blocked"
            );
            // The current block conflicts with the classifier, so revoke any
            // active capture at this height: the same-hash row stickily as a
            // conflict (never auto-restored), different-hash priors as
            // superseded. No replacement was written, so drop the supersede
            // marker too.
            revoke_current_and_superseded(
                client,
                context,
                prior,
                current_hash,
                HATHOR_REVOKE_NBITS_CONFLICT,
            )
            .await?;
            let _ = delete_pending_reconcile_at(
                client,
                context.source_id(),
                height,
                PENDING_KIND_SUPERSEDE,
            )
            .await;
            Ok(HathorHeightOutcome::ConflictSkipped)
        }
        Err(err) => Err(err).with_context(|| format!("Hathor capture at height {height}")),
    }
}

/// Revoke active prior events at a height whose hash differs from the current
/// canonical hash (a supersession).
async fn revoke_superseded(
    client: &mut Client,
    context: &HathorCaptureContext,
    prior: &[HathorEventRow],
    current_hash: &[u8],
) -> Result<()> {
    revoke_matching(
        client,
        context,
        prior,
        |hash| hash != current_hash,
        HATHOR_REVOKE_SUPERSEDED,
    )
    .await
}

/// Revoke prior events at a height when the current canonical block is REJECTED
/// (non-BTC verdict or classifier conflict): the same-hash block with
/// `current_reason`, and any different-hash prior as superseded, so a row whose
/// verdict flipped after capture stops feeding the read model.
async fn revoke_current_and_superseded(
    client: &mut Client,
    context: &HathorCaptureContext,
    prior: &[HathorEventRow],
    current_hash: &[u8],
    current_reason: &str,
) -> Result<()> {
    revoke_matching(
        client,
        context,
        prior,
        |hash| hash == current_hash,
        current_reason,
    )
    .await?;
    revoke_matching(
        client,
        context,
        prior,
        |hash| hash != current_hash,
        HATHOR_REVOKE_SUPERSEDED,
    )
    .await
}

/// Revoke active prior events matching `should_revoke`, reusing the public
/// revoke path (event mutation + parent reconcile in one transaction).
async fn revoke_matching<F>(
    client: &mut Client,
    context: &HathorCaptureContext,
    prior: &[HathorEventRow],
    should_revoke: F,
    reason: &str,
) -> Result<()>
where
    F: Fn(&[u8]) -> bool,
{
    for event in prior {
        if event.is_active && should_revoke(&event.child_block_hash) {
            revoke_merge_mining_event(client, event.event_id, reason, context.parent_classifier())
                .await
                .with_context(|| format!("revoke Hathor event {} ({reason})", event.event_id))?;
        }
    }
    Ok(())
}

/// Build the hand-assembled evidence + sidecar + pool ids + nBits verdict from a
/// reconstructed parent. Deserializes the reconstructed coinbase once.
///
/// Returns `Ok(None)` when the reconstructed coinbase is not a consensus-valid
/// transaction (trailing bytes, or no input). The parent passed its own PoW
/// target to get here, but the coinbase bytes are untrusted reconstruction
/// output: a malformed one is a per-block skip like every other format
/// violation, NOT an error - an `Err` here would fail the live tick and pin
/// the poller on that height forever (observed on archive height 1292779,
/// whose RFC-0006 coinbase carries trailing bytes).
fn build_hathor_capture(
    context: &HathorCaptureContext,
    tx: &HathorTransaction,
    hathor_height: i32,
    aux_pow: &[u8],
    recon: &HathorReconstruction,
    funds_graph: &[u8],
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
    let Some(coinbase_input) = coinbase.input.first() else {
        error!(
            height = hathor_height,
            "reconstructed BTC parent coinbase has no input; skipping"
        );
        return Ok(None);
    };
    let script_sig = coinbase_input.script_sig.as_bytes().to_vec();
    let bip34_height = parse_bip34_height(&script_sig);
    let output_addresses = derive_output_addresses(&coinbase);
    let coinbase_txid = coinbase.compute_txid();

    let nbits = recon.header.bits;
    let verdict = classify_nbits(bip34_height, nbits);
    let expected_btc_nbits = match table().expected_nbits(bip34_height.unwrap_or(-1)) {
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
        child_height: hathor_height,
        // The Hathor block hash IS the reconstructed BTC parent header hash.
        child_block_hash: block_hash_bytes.clone(),
        child_block_time: tx.timestamp,
        btc_parent_header: recon.header,
        // Hathor has no consensus-grade child nBits target in the REST payload;
        // leave the child-target verdict NULL like RSK.
        pow_validates_child_target: None,
        btc_parent_coinbase_txid: Some(coinbase_txid.to_byte_array().to_vec()),
        btc_parent_coinbase_script: Some(script_sig),
        btc_parent_coinbase_outputs: Some(serialize(&coinbase.output)),
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

use crate::chains::hathor::rpc::HathorRpcClient;
use crate::chains::spec::{ChainId, by_id};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress};

/// Hathor live capture chain. Maps the rich [`HathorHeightOutcome`] to the
/// driver's [`HeightProgress`]: the table-horizon hold is cursor-blocking
/// (`Abort`); best-effort holds enqueue a durable reconcile row and `Hold`.
pub(crate) struct HathorChainPoller {
    state: ChainPollerState,
    rpc: HathorRpcClient,
    context: HathorCaptureContext,
}

impl HathorChainPoller {
    /// Bundle the owned DB client, REST client, and capture context the poller
    /// driver borrows each tick.
    pub(crate) fn new(client: Client, rpc: HathorRpcClient, context: HathorCaptureContext) -> Self {
        Self {
            state: ChainPollerState::new(by_id(ChainId::Hathor), context.source_id(), client),
            rpc,
            context,
        }
    }
}

impl ChainPoller for HathorChainPoller {
    fn poller_state(&self) -> &ChainPollerState {
        &self.state
    }

    fn client_mut(&mut self) -> &mut Client {
        &mut self.state.client
    }

    async fn chain_tip(&self) -> Result<i32> {
        self.rpc.get_chain_tip().await
    }

    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            process_hathor_height(&mut self.state.client, &self.rpc, &self.context, height).await?;
        Ok(match outcome {
            HathorHeightOutcome::TableHorizonHold => HeightProgress::Abort,
            HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => {
                // Best-effort hold: enqueue a durable reconcile row so a replay
                // hold (dropped by the replay sub-range) is still retried via the
                // drain, regardless of which sub-range surfaced it. For a new-tip
                // height the cursor is gated by the new sub-range's break on Hold
                // and the row re-enqueues each tick until the height resolves, so
                // the queue's aging-out is what matters mainly for replay holds,
                // which the replay range otherwise drops without blocking.
                upsert_pending_reconcile(
                    &self.state.client,
                    self.context.source_id(),
                    height,
                    PENDING_KIND_RECONCILE,
                    None,
                    None,
                    Some("hathor_hold"),
                )
                .await?;
                HeightProgress::Hold
            }
            _ => HeightProgress::Advance,
        })
    }

    async fn drain_pending(&mut self) -> Result<()> {
        crate::chains::hathor::drain::drain_pending(
            &mut self.state.client,
            &self.rpc,
            &self.context,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bitcoin::BlockHash;

    use super::*;
    use crate::chains::hathor::auxpow::reconstruct_from_blobs;
    use mmm_capture::capture::ParentKind;

    /// A coinbase that fails consensus deserialization (trailing bytes) is a
    /// per-block skip, not an error: an `Err` would fail the live tick and pin
    /// the poller on that height forever (archive height 1292779 regression).
    #[test]
    fn malformed_reconstructed_coinbase_skips_instead_of_erroring() {
        let context = HathorCaptureContext {
            resolver: PoolResolver::from_default_snapshot().unwrap(),
            reward_identities: std::collections::HashMap::new(),
            base: crate::producer_runtime::ProducerContext::from_parts(
                std::collections::HashMap::new(),
                1,
                ConfiguredParentClassifier::Disabled,
            ),
        };
        let (tx, height, _) = fixture_tx(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hathor/1971823.json"
        )));
        let reconstructed = reconstruct_or_skip(height, &tx).unwrap().unwrap();
        let raw = reconstructed.raw;
        let aux_pow = reconstructed.aux_pow;
        let mut recon = reconstructed.recon;
        let funds_graph = &raw[..recon.funds_graph_len];

        let intact =
            build_hathor_capture(&context, &tx, height, &aux_pow, &recon, funds_graph).unwrap();
        assert!(intact.is_some(), "fixture coinbase must build");

        recon.full_coinbase.push(0x00);
        let corrupted =
            build_hathor_capture(&context, &tx, height, &aux_pow, &recon, funds_graph).unwrap();
        assert!(
            corrupted.is_none(),
            "trailing-byte coinbase must skip, not error"
        );
    }

    fn fixture_tx(json: &str) -> (HathorTransaction, i32, String) {
        let j: serde_json::Value = serde_json::from_str(json).unwrap();
        let tx = HathorTransaction {
            raw: j["raw_hex"].as_str().unwrap().to_owned(),
            aux_pow: Some(j["aux_pow_hex"].as_str().unwrap().to_owned()),
            hash: j["tx_id"].as_str().unwrap().to_owned(),
            timestamp: j["timestamp"].as_i64().unwrap_or(0),
        };
        (
            tx,
            j["hathor_height"].as_i64().unwrap() as i32,
            j["expected_nbits"].as_str().unwrap().to_owned(),
        )
    }

    /// The pure evidence inputs off a committed fixture, without a DB: BIP34
    /// height from the recovered coinbase, a Valid nBits verdict, the
    /// child_block_hash == parent header hash identity, and payout addresses for
    /// pool resolution.
    #[test]
    fn fixture_reconstructs_with_bip34_and_valid_verdict() {
        let (tx, _height, _) = fixture_tx(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hathor/1971823.json"
        )));
        let raw = hex::decode(&tx.raw).unwrap();
        let aux_pow = hex::decode(tx.aux_pow.as_ref().unwrap()).unwrap();
        let expected = BlockHash::from_str(&tx.hash).unwrap();
        let (_aux, recon) = reconstruct_from_blobs(&raw, &aux_pow, expected).unwrap();

        let coinbase: Transaction = deserialize(&recon.full_coinbase).unwrap();
        let bip34 = parse_bip34_height(coinbase.input[0].script_sig.as_bytes());
        assert_eq!(
            bip34,
            Some(710_969),
            "BIP34 parses to the BTC parent height"
        );
        assert_eq!(
            classify_nbits(bip34, recon.header.bits),
            NbitsVerdict::Valid
        );
        // The Hathor block hash IS the reconstructed BTC parent header hash.
        assert_eq!(recon.header.block_hash(), expected);
        // The recovered BTC coinbase yields payout addresses for pool resolution.
        assert!(!derive_output_addresses(&coinbase).is_empty());
    }

    #[test]
    fn parent_kind_unknown_without_classifier() {
        // A built payload without classifier proof is `unknown` (PoW-valid, no
        // chain placement), as for every other producer.
        let (tx, height, _) = fixture_tx(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hathor/2773476.json"
        )));
        let raw = hex::decode(&tx.raw).unwrap();
        let aux_pow = hex::decode(tx.aux_pow.as_ref().unwrap()).unwrap();
        let expected = BlockHash::from_str(&tx.hash).unwrap();
        let (_aux, recon) = reconstruct_from_blobs(&raw, &aux_pow, expected).unwrap();
        let funds_graph = &raw[..recon.funds_graph_len];
        let resolver = PoolResolver::from_default_snapshot().unwrap();
        let pool_attributions = ResolvedPoolAttributions::default();
        let coinbase: Transaction = deserialize(&recon.full_coinbase).unwrap();
        let _ = (&resolver, &funds_graph, height);

        let evidence = NormalizedEventEvidence {
            child_height: height,
            child_block_hash: recon.header.block_hash().to_byte_array().to_vec(),
            child_block_time: tx.timestamp,
            btc_parent_header: recon.header,
            pow_validates_child_target: None,
            btc_parent_coinbase_txid: Some(coinbase.compute_txid().to_byte_array().to_vec()),
            btc_parent_coinbase_script: Some(coinbase.input[0].script_sig.as_bytes().to_vec()),
            btc_parent_coinbase_outputs: Some(serialize(&coinbase.output)),
            child_coinbase_txid: None,
            child_coinbase_script: None,
            child_coinbase_outputs: None,
            aux_merkle_proof: None,
        };
        let payload = build_event_payload_from_evidence(
            evidence,
            pool_attributions,
            ClassificationProof::default(),
            1_800_000_000,
        )
        .unwrap();
        assert_eq!(payload.btc_parent_kind, ParentKind::Unknown);
        assert!(payload.pow_validates_btc_target);
        assert_eq!(payload.pow_validates_child_target, None);
        assert!(payload.pool_attributions.is_empty());
    }
}
