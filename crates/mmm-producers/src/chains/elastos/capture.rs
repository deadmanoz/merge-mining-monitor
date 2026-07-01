//! Elastos capture: the per-height state machine for live polling and bounded
//! backfills.
//!
//! Elastos is a Namecoin-family AuxPoW producer reachable over a CONFIGURABLE
//! endpoint (the self-hosted node by default, the public RPC as a
//! fallback). Because the endpoint may be untrusted, every written event is
//! self-verified before any DB write: the child header reconstruction + hash
//! guard ([`crate::chains::elastos::rpc::ElastosBlock::reconstruct`]), the full CAuxPow
//! commitment ([`verify_auxpow_commitment`]), the BTC parent-target gate, the
//! child AuxPoW-target gate, and the offline nBits contamination verdict.
//!
//! Unlike the own-node Namecoin-family producers, the Elastos child header is an
//! 84-byte header (the height is hashed in), so the child block hash is computed
//! and verified rather than taken as `ParsedHeader::hash()`. The producer builds a
//! [`NormalizedEventEvidence`] directly (the Hathor pattern): the child is the
//! Elastos block, the parent is the BTC block from the CAuxPow.
//!
//! Elastos is monotonic (`reorg_depth = 0`, DPoS finality). A replay/backfill can
//! still reprocess a height whose verdict flipped: a Valid->rejected flip revokes
//! the prior active row (reversible `ELASTOS_REVOKE_NON_BTC` or sticky
//! `ELASTOS_REVOKE_CLASSIFIER_CONFLICT`), and a rejected->Valid flip reactivates a
//! reversibly-revoked row via [`write_elastos_capture_in_txn`].

use anyhow::{Context, Result};
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use tokio_postgres::Client;
use tracing::warn;

use crate::chains::elastos::identity::{
    ELASTOS_MINERINFO_NAMESPACE, ELASTOS_REWARD_ADDRESS_NAMESPACE,
    resolve_elastos_identity_attributions, upsert_elastos_minerinfo_pool_identities,
    upsert_elastos_reward_address_pool_identities,
};
use crate::chains::elastos::rpc::{ElastosBlock, ElastosRpc, ReconstructedBlock};
use crate::chains::nbits_horizon::{
    HorizonOutcome, far_future_against_fresh_tip, resolve_horizon_nbits,
};
use crate::chains::{
    ensure_offline_valid_not_classifier_conflict, is_offline_valid_classifier_conflict,
};
use crate::producer_runtime::ProducerContext;
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::auxpow::{
    ELASTOS_AUXPOW_CHAIN_ID, ParsedAuxpowBlock, parse_bip34_height, parse_elastos_auxpow,
    validates_target, verify_auxpow_commitment,
};
use mmm_capture::capture::{
    ClassificationProof, ELASTOS_REVOKE_CLASSIFIER_CONFLICT, ELASTOS_REVOKE_NON_BTC,
    NormalizedEventEvidence, ResolvedPoolAttributions, build_event_payload_from_evidence,
    now_epoch_seconds, resolve_parent_pool_attribution_from_coinbase,
};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::nbits_table::{NbitsVerdict, classify_nbits};
use mmm_capture::pool_resolver::PoolResolver;
use mmm_capture::source_registry::ELASTOS_SOURCE_CODE;
use mmm_read_model::capture_in_txn;
use mmm_read_model::revoke_merge_mining_event;
use mmm_store::{
    active_event_ids_at_height, load_pool_identities_by_namespace, retag_revocation_reason,
    write_elastos_capture_in_txn,
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

    /// The configured BTC parent classifier, shared with `capture_in_txn` and the
    /// beyond-horizon nBits resolver (`chains::nbits_horizon`).
    pub fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        self.base.parent_classifier()
    }
}

/// Per-height capture outcome. The poller maps `TableHorizonHold` to
/// [`crate::poller::HeightProgress::Abort`] (cursor-blocking) and everything else
/// to `Advance` (Elastos is monotonic with no transient-hold path).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElastosHeightOutcome {
    /// A verified BTC-parent event was written (or reactivated).
    AuxpowWritten,
    /// No `auxpow` field, or a pre-activation dummy block; no event.
    NonAuxpowSkipped,
    /// The parent header fails BTC difficulty (the common merge-mined case):
    /// Elastos captures only BTC-difficulty-valid parents, never `near`.
    NearSkipped,
    /// The parent header fails the child AuxPoW target; never written.
    ChildTargetSkipped,
    /// The offline nBits verdict is non-BTC (BCH/BSV contaminant or
    /// indeterminate); any prior active row is revoked, no event written.
    NonBtcParentSkipped,
    /// The offline verdict was Valid but an enabled classifier contradicted it
    /// (`difficulty_epoch_ok = false`); the write was blocked.
    ClassifierConflictSkipped,
    /// The block was inconsistent / the proof malformed; skipped without a write.
    MalformedSkipped,
    /// The parent BIP34 height is beyond the nBits-table horizon (cursor-blocking).
    TableHorizonHold,
}

/// The pure (no-DB) verdict of evaluating a fetched block through every gate.
/// Keeps the gate logic unit-testable over committed fixtures.
enum ElastosEvaluation {
    /// Terminal skip, no DB mutation.
    Skip(ElastosHeightOutcome),
    /// The embedded nBits table cannot classify the parent (beyond its horizon).
    /// The async driver resolves the canonical epoch nBits from Bitcoin Core: a
    /// match writes, a mismatch / fabricated far-future height revokes, and an
    /// unanswerable Core holds the cursor. Carries the parsed parent + reconstructed
    /// block so the Core-Valid path can write without re-evaluating.
    TableHorizon {
        bip34_height: i32,
        parsed: Box<ParsedAuxpowBlock>,
        recon: Box<ReconstructedBlock>,
    },
    /// A non-BTC parent: revoke any prior active row at the height, then skip.
    RevokeNonBtc,
    /// A verified Valid BTC parent: write the event.
    Write {
        parsed: Box<ParsedAuxpowBlock>,
        recon: Box<ReconstructedBlock>,
    },
}

/// Drive one Elastos height: fetch, evaluate every gate, then apply the DB effect.
pub async fn process_elastos_height(
    client: &mut Client,
    rpc: &impl ElastosRpc,
    context: &ElastosCaptureContext,
    height: i32,
) -> Result<ElastosHeightOutcome> {
    let block = rpc
        .get_block_by_height(height)
        .await
        .with_context(|| format!("fetch Elastos block at height {height}"))?;

    match evaluate_elastos_block(height, &block) {
        ElastosEvaluation::Skip(outcome) => Ok(outcome),
        ElastosEvaluation::TableHorizon {
            bip34_height,
            parsed,
            recon,
        } => {
            handle_table_horizon_parent(
                client,
                context,
                height,
                bip34_height,
                &block,
                &parsed,
                &recon,
            )
            .await
        }
        ElastosEvaluation::RevokeNonBtc => {
            revoke_active_at_height(client, context, height, ELASTOS_REVOKE_NON_BTC).await?;
            Ok(ElastosHeightOutcome::NonBtcParentSkipped)
        }
        ElastosEvaluation::Write { parsed, recon } => {
            // Guard the in-table Valid path too: a fabricated far-future BIP34 height
            // inside a covered epoch (whose nBits happened to match) must be revoked,
            // not written. A fresh synced Core tip is the only authority that can
            // prove it; otherwise the offline Valid verdict stands.
            let bip34_height = parse_bip34_height(&parsed.parent_coinbase_script);
            if let Some(height_claim) = bip34_height
                && far_future_against_fresh_tip(context.parent_classifier(), height_claim).await
            {
                revoke_active_at_height(client, context, height, ELASTOS_REVOKE_NON_BTC).await?;
                Ok(ElastosHeightOutcome::NonBtcParentSkipped)
            } else {
                write_valid_capture(client, context, height, &block, &parsed, &recon).await
            }
        }
    }
}

/// Resolve an above-table-horizon parent against Bitcoin Core (see
/// [`resolve_horizon_nbits`]): a Core-confirmed nBits match writes the capture; a
/// mismatch or a fabricated far-future height revokes as non-BTC; an unanswerable
/// Core (disabled / IBD / unreachable / lagging / transient error) holds the cursor
/// exactly as the old stale-table behaviour did.
async fn handle_table_horizon_parent(
    client: &mut Client,
    context: &ElastosCaptureContext,
    height: i32,
    bip34_height: i32,
    block: &ElastosBlock,
    parsed: &ParsedAuxpowBlock,
    recon: &ReconstructedBlock,
) -> Result<ElastosHeightOutcome> {
    match resolve_horizon_nbits(
        context.parent_classifier(),
        bip34_height,
        parsed.parent_header.bits(),
    )
    .await
    {
        HorizonOutcome::Valid => {
            write_valid_capture(client, context, height, block, parsed, recon).await
        }
        HorizonOutcome::Contaminant | HorizonOutcome::FarFuture => {
            revoke_active_at_height(client, context, height, ELASTOS_REVOKE_NON_BTC).await?;
            Ok(ElastosHeightOutcome::NonBtcParentSkipped)
        }
        HorizonOutcome::Hold => Ok(ElastosHeightOutcome::TableHorizonHold),
    }
}

/// Test-only harness for injected active-row revocation cases after a real row
/// has already been written. It remains for synthetic `Contaminant` coverage and
/// the focused `FarFuture` revocation seam; the pure gate and async fail-closed
/// resolver paths live in `chains::nbits_horizon` unit tests.
#[cfg(any(test, feature = "db-integration"))]
pub async fn process_elastos_table_horizon_for_test(
    client: &mut Client,
    context: &ElastosCaptureContext,
    height: i32,
    bip34_height: i32,
    actual_bits: bitcoin::CompactTarget,
) -> Result<ElastosHeightOutcome> {
    match resolve_horizon_nbits(context.parent_classifier(), bip34_height, actual_bits).await {
        HorizonOutcome::Valid => Ok(ElastosHeightOutcome::AuxpowWritten),
        HorizonOutcome::Contaminant | HorizonOutcome::FarFuture => {
            revoke_active_at_height(client, context, height, ELASTOS_REVOKE_NON_BTC).await?;
            Ok(ElastosHeightOutcome::NonBtcParentSkipped)
        }
        HorizonOutcome::Hold => Ok(ElastosHeightOutcome::TableHorizonHold),
    }
}

/// Run every offline gate over a fetched block. No DB, no RPC: pure, so the gate
/// order and verdicts are unit-testable over committed fixtures.
///
/// Order (each fails closed): requested-height match -> reconstruct + hash guard
/// -> auxpow presence -> parse -> PARENT-side dummy filter -> commitment verify ->
/// BTC parent target -> child target -> nBits contamination verdict.
fn evaluate_elastos_block(requested_height: i32, block: &ElastosBlock) -> ElastosEvaluation {
    let Some((parsed, recon)) = (match reconstruct_and_parse_auxpow(requested_height, block) {
        Ok(parsed) => parsed,
        Err(outcome) => return ElastosEvaluation::Skip(outcome),
    }) else {
        return ElastosEvaluation::Skip(ElastosHeightOutcome::NonAuxpowSkipped);
    };

    if let Some(outcome) = auxpow_gate_skip(block.height, &parsed, &recon) {
        return ElastosEvaluation::Skip(outcome);
    }

    classify_elastos_parent_nbits(parsed, recon)
}

fn reconstruct_and_parse_auxpow(
    requested_height: i32,
    block: &ElastosBlock,
) -> std::result::Result<Option<(ParsedAuxpowBlock, ReconstructedBlock)>, ElastosHeightOutcome> {
    // Untrusted-endpoint guard: the RPC must answer for the height we asked for, or
    // a stale/misrouted response would write or revoke the WRONG child height while
    // the poller advances past this one.
    if block.height != requested_height {
        warn!(
            requested = requested_height,
            returned = block.height,
            "Elastos RPC returned a different height than requested; skipping"
        );
        return Err(ElastosHeightOutcome::MalformedSkipped);
    }

    let recon = match block.reconstruct() {
        Ok(recon) => recon,
        Err(err) => {
            warn!(height = block.height, error = %err, "Elastos reconstruction/hash guard failed; skipping");
            return Err(ElastosHeightOutcome::MalformedSkipped);
        }
    };

    let Some(auxpow_blob) = recon.auxpow.as_deref() else {
        return Ok(None);
    };

    let parsed = match parse_elastos_auxpow(recon.prefix_header.clone(), auxpow_blob) {
        Ok(parsed) => parsed,
        Err(err) => {
            warn!(height = block.height, error = %err, "Elastos auxpow parse failed; skipping");
            return Err(ElastosHeightOutcome::MalformedSkipped);
        }
    };

    Ok(Some((parsed, recon)))
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

fn classify_elastos_parent_nbits(
    parsed: ParsedAuxpowBlock,
    recon: ReconstructedBlock,
) -> ElastosEvaluation {
    // Offline BCH/BSV contamination verdict against the embedded table. Beyond the
    // table horizon the async driver resolves the canonical epoch nBits from Core
    // (no operator regen needed), so this no longer warns here.
    let bip34_height = parse_bip34_height(&parsed.parent_coinbase_script);
    match classify_nbits(bip34_height, parsed.parent_header.bits()) {
        NbitsVerdict::AboveTableHorizon => {
            let bip34_height =
                bip34_height.expect("AboveTableHorizon requires a parsed BIP34 height");
            ElastosEvaluation::TableHorizon {
                bip34_height,
                parsed: Box::new(parsed),
                recon: Box::new(recon),
            }
        }
        NbitsVerdict::Contaminant | NbitsVerdict::Indeterminate => ElastosEvaluation::RevokeNonBtc,
        NbitsVerdict::Valid => ElastosEvaluation::Write {
            parsed: Box::new(parsed),
            recon: Box::new(recon),
        },
    }
}

/// Write a Valid BTC-parent capture. Builds the evidence directly (child = the
/// Elastos block, parent = the BTC block from the CAuxPow) and runs the shared
/// `capture_in_txn` with the pre-upsert classifier-conflict guard.
async fn write_valid_capture(
    client: &mut Client,
    context: &ElastosCaptureContext,
    height: i32,
    block: &ElastosBlock,
    parsed: &ParsedAuxpowBlock,
    recon: &ReconstructedBlock,
) -> Result<ElastosHeightOutcome> {
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
        child_height: recon.height,
        child_block_hash: recon.block_hash.to_byte_array().to_vec(),
        child_block_time: i64::from(recon.time),
        btc_parent_header: parsed.parent_header.header,
        // The own-node child-target check, GATED true to reach this path.
        pow_validates_child_target: Some(validates_target(
            parsed.parent_header.hash(),
            recon.prefix_header.bits(),
        )),
        btc_parent_coinbase_txid: Some(parsed.parent_coinbase_txid.to_byte_array().to_vec()),
        btc_parent_coinbase_script: Some(parsed.parent_coinbase_script.clone()),
        btc_parent_coinbase_outputs: Some(serialize(&parsed.parent_coinbase_outputs)),
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

    let write_result = capture_in_txn(
        client,
        context.source_id(),
        context.parent_classifier(),
        &mut payload,
        "Elastos",
        async |txn, source_id, payload| {
            // Pre-upsert classifier-conflict guard: only Valid rows reach here, so
            // a preclassified difficulty_epoch_ok == Some(false) is a contaminant
            // the offline table missed. Abort rather than store the contradiction.
            ensure_offline_valid_not_classifier_conflict(payload)?;
            write_elastos_capture_in_txn(txn, source_id, payload).await
        },
    )
    .await;

    match write_result {
        Ok(_event_id) => Ok(ElastosHeightOutcome::AuxpowWritten),
        Err(err) if is_offline_valid_classifier_conflict(&err) => {
            warn!(
                height,
                "Elastos offline-Valid row conflicts with classifier; write blocked"
            );
            // Sticky reason: a later Valid recapture does NOT auto-restore it.
            revoke_active_at_height(client, context, height, ELASTOS_REVOKE_CLASSIFIER_CONFLICT)
                .await?;
            // A row already auto-revoked as REVERSIBLE non-BTC at this height must
            // be retagged sticky too, or a later Valid recapture would clear the
            // reversible reason and restore a classifier-rejected block.
            retag_revocation_reason(
                client,
                context.source_id(),
                height,
                ELASTOS_REVOKE_NON_BTC,
                ELASTOS_REVOKE_CLASSIFIER_CONFLICT,
            )
            .await?;
            Ok(ElastosHeightOutcome::ClassifierConflictSkipped)
        }
        Err(err) => Err(err).with_context(|| format!("Elastos capture at height {height}")),
    }
}

/// Revoke every active Elastos event at a height (a replay verdict-flip to
/// rejected), reusing the public revoke path (event mutation + parent reconcile in
/// one transaction). `ELASTOS_REVOKE_NON_BTC` is reversible (auto-restored on a
/// later Valid recapture); `ELASTOS_REVOKE_CLASSIFIER_CONFLICT` is sticky.
async fn revoke_active_at_height(
    client: &mut Client,
    context: &ElastosCaptureContext,
    height: i32,
    reason: &str,
) -> Result<()> {
    let event_ids = active_event_ids_at_height(client, context.source_id(), height).await?;
    for event_id in event_ids {
        revoke_merge_mining_event(client, event_id, reason, context.parent_classifier())
            .await
            .with_context(|| format!("revoke Elastos event {event_id} ({reason})"))?;
    }
    Ok(())
}

use crate::chains::elastos::rpc::ElastosRpcClient;
use crate::chains::spec::{ChainId, by_id};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress};

/// Elastos live capture chain. Monotonic like the Namecoin family, but maps the
/// rich [`ElastosHeightOutcome`] so the nBits-table horizon is cursor-blocking
/// (`Abort`); every other outcome advances.
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

    /// Process one height, then translate the outcome to poll progress: only the
    /// nBits-table horizon hold is cursor-blocking (`Abort`); every other outcome
    /// (write, revoke, any skip) advances. Elastos is monotonic, so there is no
    /// transient-hold/retry path.
    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            process_elastos_height(&mut self.state.client, &self.rpc, &self.context, height)
                .await?;
        Ok(match outcome {
            ElastosHeightOutcome::TableHorizonHold => HeightProgress::Abort,
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
        // ELA 360062: a verified stale (BTC parent 572,333, within the committed
        // nBits table) passes recon + commitment + both targets + a Valid verdict.
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
            ElastosEvaluation::Skip(ElastosHeightOutcome::NonAuxpowSkipped)
        ));
    }

    #[test]
    fn real_post_activation_blocks_clear_commitment() {
        // Real blocks must clear recon + parse + commitment: the verifier
        // generalizes beyond ELA 360062, so the outcome is never Malformed or
        // NonAuxpow. The final outcome (Write / RevokeNonBtc / Near / ChildTarget /
        // TableHorizon) is parent- and table-dependent, so it is not asserted here.
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
                    ElastosEvaluation::Skip(
                        ElastosHeightOutcome::MalformedSkipped
                            | ElastosHeightOutcome::NonAuxpowSkipped
                    )
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
            ElastosEvaluation::Skip(ElastosHeightOutcome::MalformedSkipped)
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
            ElastosEvaluation::Skip(ElastosHeightOutcome::MalformedSkipped)
        ));
    }

    #[test]
    fn stalled_2232276_parent_is_covered_non_btc_in_table() {
        // ELA 2,232,276 was the original far-future horizon regression. Its parent
        // nBits is in-table and classified directly as non-BTC (the far-future
        // height/tolerance gate now lives in `chains::nbits_horizon`).
        let b = block(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/elastos/ela-2232276.json"
        )));
        assert!(matches!(
            evaluate_elastos_block(b.height, &b),
            ElastosEvaluation::RevokeNonBtc
        ));
    }
}
