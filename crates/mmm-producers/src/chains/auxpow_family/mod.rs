//! The shared capture, poll, and backfill implementation for bitcoind-family
//! AuxPoW chains (Namecoin, Syscoin, Fractal, Qbit, Terracoin).
//!
//! Everything chain-specific arrives through the `FamilySpec` data on the
//! chain's `ChainSpec` row: auth mode and endpoint config (resolved by
//! `chains::config`), the proof fetch strategy (`getblock 0` raw block, the
//! Fractal `getblockheader-auxpow` header blob with its exact-version gate, or
//! the Qbit extended-header prefix of a raw block), the malformed-proof
//! policy, the below-floor backfill warning, and the post-backfill repair
//! scope. A new bitcoind-family chain is a `CHAINS` row, not a module.
//!
//! Two proof formats reach the write path here. Classic CAuxPow chains go
//! through `build_event_payload`; Qbit's `ParsedQbitAuxpow` is a distinct type
//! with no `hashBlock` field, so it is projected straight into a
//! `NormalizedEventEvidence` instead of being forced into a synthetic
//! `ParsedAuxpowBlock` (which would need a `hash_block` value the Qbit wire
//! format never carries).

use anyhow::{Context, Result, ensure};
use bitcoin::BlockHash;
use tokio_postgres::Client;
use tracing::{debug, error, info, warn};

use crate::chains::bitcoind_rpc::{BitcoindRpc, BitcoindRpcClient};
use crate::chains::child_payout_registry::seed_child_payout_identities_for;
use crate::chains::spec::{
    CaptureFailurePolicy, ChainSpec, FamilySpec, FetchStrategy, RepairScope,
};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress, Poller, RescanOutcome};
use crate::producer_runtime::{ProducerContext, ProducerRuntime, run_post_backfill_repair};
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::auxpow::{
    ParsedAuxpowBlock, ParsedNamecoinBlock, ParsedQbitAuxpow, attach_child_block_coinbase,
    parse_auxpow_header_blob, parse_child_block_coinbase,
};
use mmm_capture::capture::{
    ClassificationProof, MergeMiningEventPayload, build_event_payload, now_epoch_seconds,
    resolve_event_pools_with_child_payout,
};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::capture_in_txn;
use mmm_store::{
    CAPTURE_ERROR_MALFORMED_AUXPOW_PROOF, ChildChainHeadOutcome, ChildChainHeadRecord,
    CurrentBlockParent, EvidenceMarker, clear_capture_error, finish_child_chain_height_operation,
    has_capture_error, load_child_chain_head, load_pool_identities_by_namespace,
    lock_child_chain_height_session, record_capture_error, record_child_chain_block,
    record_child_chain_block_in_own_transaction, reobserve_child_chain_block_in_own_transaction,
    upsert_merge_mining_event_with_attributions,
};
use qbit::{fetch_qbit_candidate, write_qbit_event};
#[cfg(any(test, feature = "db-integration"))]
pub use validation::ensure_mainnet_endpoint;
#[cfg(not(any(test, feature = "db-integration")))]
use validation::ensure_mainnet_endpoint;

mod backfill;
mod qbit;
mod validation;

pub(crate) use self::backfill::backfill;

/// Shared per-chain capture context for the bitcoind family: the immutable
/// chain spec plus everything resolved once at bootstrap so the per-height loop
/// does no repeat setup. Built per command, then borrowed across every height.
#[derive(Debug)]
pub struct AuxpowCaptureContext {
    spec: &'static ChainSpec,
    /// Pool snapshot resolver, loaded from the embedded default snapshot.
    resolver: PoolResolver,
    /// Reward-address identities for the family's child-payout namespace, empty
    /// when the family does not resolve child payouts.
    child_payout_identities: PoolIdentityLookup,
    /// Source id, classifier, and slug -> pool_id map shared by all producers.
    base: ProducerContext,
}

impl AuxpowCaptureContext {
    /// Bootstrap the context once per command: resolves the source id, loads the
    /// pool snapshot, and (only for child-payout families) loads the reward
    /// identities for the family namespace. The classifier is moved in from the
    /// runtime so the live override stays decided at one place.
    pub async fn new_with_classifier(
        client: &Client,
        spec: &'static ChainSpec,
        parent_classifier: ConfiguredParentClassifier,
    ) -> Result<Self> {
        let resolver = PoolResolver::from_default_snapshot()?;
        let mut base =
            ProducerContext::bootstrap_with(client, spec.source_code, &resolver, parent_classifier)
                .await?;
        // Seed this chain's child-payout address registry before loading it, so a
        // fresh database resolves the reviewed mappings on the first capture (the
        // Hathor/Elastos bootstrap pattern).
        let child_payout_identities = match family_of(spec).child_payout {
            Some(params) => {
                seed_child_payout_identities_for(
                    client,
                    params.namespace,
                    base.pool_ids_by_slug_mut(),
                )
                .await?;
                load_pool_identities_by_namespace(client, &[params.namespace]).await?
            }
            None => PoolIdentityLookup::new(),
        };
        Ok(Self {
            spec,
            resolver,
            child_payout_identities,
            base,
        })
    }

    /// The `source` row id for this chain, the FK every written event carries.
    fn source_id(&self) -> i64 {
        self.base.source_id()
    }

    /// The configured Core-backed BTC parent classifier used by `capture_in_txn`
    /// to place the parent.
    fn parent_classifier(&self) -> &ConfiguredParentClassifier {
        self.base.parent_classifier()
    }

    fn family(&self) -> &'static FamilySpec {
        family_of(self.spec)
    }
}

/// Extract the `FamilySpec` from a chain spec, panicking if the row is not a
/// bitcoind-family chain. Callers reach this module only via the registry, which
/// dispatches family chains here, so a missing `family` is a CHAINS-table bug.
pub(super) fn family_of(spec: &'static ChainSpec) -> &'static FamilySpec {
    spec.family
        .as_ref()
        .expect("auxpow_family requires a bitcoind-family ChainSpec row")
}

/// Per-height capture verdict. Drives the backfill summary counters; only
/// `AuxpowWritten` produces a `merge_mining_event`. Skips are normal, not
/// errors: most heights are non-AuxPoW, and a malformed block is never written
/// and never demotes prior evidence. Which malformed variant a chain produces
/// is `FamilySpec::failure_policy`, not a property of the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightOutcome {
    /// An AuxPoW event was upserted for this height.
    AuxpowWritten,
    /// The block carries no merge-mining proof (or fails the version gate);
    /// nothing written.
    NonAuxpowSkipped,
    /// `CaptureFailurePolicy::SkipMalformedProof`: the block claimed AuxPoW but failed
    /// to parse; logged and skipped without writing.
    MalformedSkipped,
    /// `CaptureFailurePolicy::HoldMalformedProof`: same failure, but a `capture_error`
    /// row was persisted for the height first and the interval must not be
    /// reported complete. The live cursor holds here; a bounded backfill fails
    /// the run.
    MalformedHeld,
}

/// What one height's proof fetch produced, before any write. The two parsed
/// variants are genuinely different wire formats, not a shared type with a
/// discriminant: Qbit's proof has no `hashBlock` and cannot be represented as a
/// `ParsedAuxpowBlock` without inventing one.
pub(super) enum AuxpowFetch {
    /// A classic CAuxPow proof (Namecoin, Syscoin, Fractal).
    Classic(Box<ParsedAuxpowBlock>),
    /// A verified Qbit extended-header proof.
    Qbit(Box<ParsedQbitAuxpow>),
    NonAuxpow,
    /// The block claimed a proof that failed to decode; the string is the full
    /// error chain, stored as `capture_error.detail` under the hold policy.
    Malformed(String),
}

/// Read the child header version (first 4 bytes, little-endian) from a
/// `[child header][CAuxPow]` blob.
fn child_header_version(blob: &[u8]) -> Result<i32> {
    ensure!(
        blob.len() >= 4,
        "header blob is shorter than the 4-byte version field"
    );
    Ok(i32::from_le_bytes(blob[0..4].try_into().unwrap()))
}

/// Whether a `getblockheader-false-true` blob is a merge-mined block under the
/// family's exact-version gate (Fractal: `0x20240100`; the `0x20260100`
/// Cadence class sets the generic AuxPoW bit but is NOT merge-mined).
fn is_merge_mined(blob: &[u8], exact_version: i32) -> Result<bool> {
    Ok(child_header_version(blob)? == exact_version)
}

/// Capture a single height through the family's fetch strategy. Outcomes and
/// write semantics are identical to the historical per-chain runners; the
/// chain label in capture contexts comes from the spec.
///
/// Every processed height also records which block the child chain carries
/// there (see `docs/data-model.md`, "Child Displacement"): a captured AuxPoW
/// block is recorded inside its capture transaction, and a block that yields
/// no event (non-AuxPoW, or a malformed proof) is recorded in a transaction of
/// its own. A rescanned height whose block changed therefore marks the earlier
/// event displaced instead of leaving two current blocks. Poll and backfill
/// share this path, so a backfill over a reorged range repairs it the same way.
///
/// The whole height, from the `getblockhash` observation through the last
/// write, runs under a session-level lock on `(source, height)`, so what a
/// producer observed is what it records: a live poller and a bounded backfill
/// overlapping on one height observe and write one after the other, and the
/// later observation describes the chain.
pub async fn process_auxpow_height(
    client: &mut Client,
    rpc: &impl BitcoindRpc,
    context: &AuxpowCaptureContext,
    height: i32,
) -> Result<HeightOutcome> {
    let source_id = context.source_id();
    lock_child_chain_height_session(client, source_id, height).await?;
    let result = async {
        let block_hash = observe_block_hash(rpc, context, height).await?;
        process_locked_height(client, rpc, context, height, block_hash).await
    }
    .await;
    let result = validation::retain_failed_height(client, context, height, result).await;
    finish_child_chain_height_operation(client, source_id, height, result).await
}

/// The chain's block hash at `height`: the one observation every processed
/// height makes, whether it goes on to capture or not.
async fn observe_block_hash(
    rpc: &impl BitcoindRpc,
    context: &AuxpowCaptureContext,
    height: i32,
) -> Result<BlockHash> {
    let label = context.family().label;
    rpc.get_block_hash(height)
        .await
        .with_context(|| format!("get {label} block hash at height {height}"))
}

/// Re-observe an already-processed height inside the trailing rescan window.
///
/// One `getblockhash` decides whether anything changed: when the chain still
/// carries the block the `child_chain_head` row recorded and that record is
/// final (see [`ChildChainHeadOutcome::is_final`]), the height is not
/// captured again. Displacement maintenance still runs under the height
/// lock, with the parent the original capture recorded, because a write that
/// bypasses observation (a historical publication import for a live chain)
/// can leave an undisplaced sibling at the height that only the next
/// observation repairs. A different hash, no row, or a non-final record runs
/// the full capture through [`process_auxpow_height`]'s locked path.
///
/// From the production host every remote call costs about 340 ms, so this is
/// the difference between a rescan window of 20 heights costing one round
/// trip each and costing three plus the capture.
pub async fn rescan_auxpow_height(
    client: &mut Client,
    rpc: &impl BitcoindRpc,
    context: &AuxpowCaptureContext,
    height: i32,
) -> Result<RescanOutcome<HeightOutcome>> {
    let source_id = context.source_id();
    lock_child_chain_height_session(client, source_id, height).await?;
    let result = async {
        let block_hash = observe_block_hash(rpc, context, height).await?;
        let head = load_child_chain_head(&*client, source_id, height, EvidenceMarker::None).await?;
        // An open capture error says the last processing of the height did
        // not succeed, whatever the head row records (a capture that recorded
        // the error and stopped before replacing the head leaves a final head
        // behind): only a successful reprocessing may clear it, so the height
        // takes the full capture.
        let error_open = context.family().failure_policy.retains_errors()
            && has_capture_error(client, source_id, height).await?;
        match head {
            Some(head)
                if !error_open
                    && head.is_final()
                    && head.block_hash.as_slice() == block_hash.as_ref() as &[u8] =>
            {
                reobserve_child_chain_block_in_own_transaction(
                    client,
                    source_id,
                    height,
                    block_hash.as_ref(),
                    head.current_parent(),
                    now_epoch_seconds()?,
                )
                .await?;
                Ok(RescanOutcome::Unchanged)
            }
            _ => process_locked_height(client, rpc, context, height, block_hash)
                .await
                .map(RescanOutcome::Captured),
        }
    }
    .await;
    let result = validation::retain_failed_height(client, context, height, result).await;
    finish_child_chain_height_operation(client, source_id, height, result).await
}

/// One height's capture and writes for the block the caller observed at it,
/// under the session-level height lock [`process_auxpow_height`] and
/// [`rescan_auxpow_height`] hold around it.
async fn process_locked_height(
    client: &mut Client,
    rpc: &impl BitcoindRpc,
    context: &AuxpowCaptureContext,
    height: i32,
    block_hash: BlockHash,
) -> Result<HeightOutcome> {
    let family = context.family();

    let outcome = match fetch_auxpow_candidate(rpc, context, &block_hash, height).await? {
        AuxpowFetch::Classic(mut parsed) => {
            attach_child_payout_if_needed(rpc, context, &block_hash, parsed.as_mut()).await?;
            write_classic_event(client, context, height, &parsed).await?;
            HeightOutcome::AuxpowWritten
        }
        AuxpowFetch::Qbit(parsed) => {
            write_qbit_event(client, context, height, &parsed).await?;
            HeightOutcome::AuxpowWritten
        }
        AuxpowFetch::NonAuxpow => HeightOutcome::NonAuxpowSkipped,
        AuxpowFetch::Malformed(detail) => {
            record_malformed_height(client, context, height, &block_hash, detail).await?
        }
    };

    // A block the node confirms carries no AuxPoW cannot be any hashless AuxPoW
    // observation, so it displaces them; a proof that failed to parse leaves
    // the block's parent unknown, so hashless rows are left alone.
    let eventless_record = match outcome {
        HeightOutcome::AuxpowWritten => None,
        HeightOutcome::NonAuxpowSkipped => Some((
            CurrentBlockParent::NoAuxpow,
            ChildChainHeadOutcome::NoAuxpow,
        )),
        HeightOutcome::MalformedSkipped => Some((
            CurrentBlockParent::Unknown,
            ChildChainHeadOutcome::Unverified,
        )),
        HeightOutcome::MalformedHeld => {
            Some((CurrentBlockParent::Unknown, ChildChainHeadOutcome::Held))
        }
    };
    if let Some((current_parent, head_outcome)) = eventless_record {
        record_child_chain_block_in_own_transaction(
            client,
            context.source_id(),
            height,
            ChildChainHeadRecord {
                block_hash: block_hash.as_ref(),
                parent: current_parent,
                outcome: head_outcome,
                evidence: EvidenceMarker::None,
                observed_at: now_epoch_seconds()?,
            },
        )
        .await?;
    }
    if matches!(
        outcome,
        HeightOutcome::MalformedSkipped | HeightOutcome::MalformedHeld
    ) {
        return Ok(outcome);
    }

    // This exact height was reprocessed successfully, which is the ONLY thing
    // that clears its durable capture error. A cursor advance elsewhere never
    // does. Only hold-policy chains can have written one.
    if family.failure_policy.retains_errors()
        && clear_capture_error(client, context.source_id(), height).await?
    {
        info!(
            chain = context.spec.slug,
            height, "capture error resolved; height reprocessed successfully"
        );
    }
    Ok(outcome)
}

/// Apply the family's malformed-proof policy to one height. Under
/// `HoldMalformedProof` the `capture_error` row is written BEFORE this returns, so a
/// crash between detection and return cannot lose the signal.
async fn record_malformed_height(
    client: &mut Client,
    context: &AuxpowCaptureContext,
    height: i32,
    block_hash: &BlockHash,
    detail: String,
) -> Result<HeightOutcome> {
    let spec = context.spec;
    match context.family().failure_policy {
        CaptureFailurePolicy::SkipMalformedProof => {
            error!(
                chain = spec.slug,
                height,
                block_hash = %block_hash,
                error = %detail,
                "malformed AuxPoW block skipped without writing an event"
            );
            Ok(HeightOutcome::MalformedSkipped)
        }
        CaptureFailurePolicy::HoldMalformedProof | CaptureFailurePolicy::HoldAnyHeightFailure => {
            let now = now_epoch_seconds()?;
            record_capture_error(
                client,
                context.source_id(),
                height,
                Some(block_hash.as_ref()),
                CAPTURE_ERROR_MALFORMED_AUXPOW_PROOF,
                Some(detail.as_str()),
                now,
            )
            .await?;
            error!(
                chain = spec.slug,
                height,
                block_hash = %block_hash,
                error = %detail,
                "malformed AuxPoW proof recorded as a capture error; interval held"
            );
            Ok(HeightOutcome::MalformedHeld)
        }
    }
}

/// Write one classic-CAuxPow event: pool resolution (including the optional
/// child-payout pass), payload assembly, and the Core-classified upsert.
async fn write_classic_event(
    client: &mut Client,
    context: &AuxpowCaptureContext,
    height: i32,
    parsed: &ParsedAuxpowBlock,
) -> Result<()> {
    let pool_ids = resolve_event_pools_with_child_payout(
        parsed,
        &context.resolver,
        context.base.pool_ids_by_slug(),
        context.family().child_payout,
        Some(&context.child_payout_identities),
    );
    let now = now_epoch_seconds()?;
    let mut payload = build_event_payload(
        parsed,
        Some(height),
        pool_ids,
        ClassificationProof::default(),
        now,
    )?;
    write_event_in_txn(client, context, &mut payload).await
}

/// The shared Core-classified upsert for both proof formats, followed by the
/// child-side record that this block is the chain's block at its height. The
/// capture transaction takes the per-height lock before any parent lock, so
/// the record is ordered correctly against a concurrent capture at the same
/// height; the upsert closure may run more than once under the retry loop and
/// the record is idempotent.
pub(super) async fn write_event_in_txn(
    client: &mut Client,
    context: &AuxpowCaptureContext,
    payload: &mut MergeMiningEventPayload,
) -> Result<()> {
    let observed_at = now_epoch_seconds()?;
    capture_in_txn(
        client,
        context.source_id(),
        context.parent_classifier(),
        payload,
        context.family().label,
        async |txn, source_id, payload| {
            let outcome =
                upsert_merge_mining_event_with_attributions(txn, source_id, payload).await?;
            let child_height = payload
                .child_height
                .context("bitcoind-family event payload carries no child height")?;
            let child_block_hash = payload
                .child_block_hash
                .as_deref()
                .context("bitcoind-family event payload carries no child block hash")?;
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
                },
            )
            .await?;
            Ok(outcome)
        },
    )
    .await?;
    Ok(())
}

async fn fetch_auxpow_candidate(
    rpc: &impl BitcoindRpc,
    context: &AuxpowCaptureContext,
    block_hash: &BlockHash,
    height: i32,
) -> Result<AuxpowFetch> {
    let spec = context.spec;
    let family = context.family();

    match family.fetch {
        FetchStrategy::RawBlock { .. } => {
            fetch_raw_block_candidate(rpc, spec, family, block_hash, height).await
        }
        FetchStrategy::HeaderBlob { exact_version } => {
            fetch_header_blob_candidate(rpc, spec, family, block_hash, height, exact_version).await
        }
        FetchStrategy::QbitExtendedHeader { genesis_block_hash } => {
            fetch_qbit_candidate(rpc, spec, family, block_hash, height, genesis_block_hash).await
        }
    }
}

async fn fetch_raw_block_candidate(
    rpc: &impl BitcoindRpc,
    spec: &'static ChainSpec,
    family: &'static FamilySpec,
    block_hash: &BlockHash,
    height: i32,
) -> Result<AuxpowFetch> {
    let raw = rpc
        .get_block_raw(block_hash)
        .await
        .with_context(|| format!("get raw {} block {block_hash}", family.label))?;
    match validation::parse_raw_candidate(&raw, family, block_hash, height) {
        Ok(ParsedNamecoinBlock::NonAuxpow(_)) => {
            debug!(
                chain = spec.slug,
                height,
                block_hash = %block_hash,
                "skipping non-AuxPoW block"
            );
            Ok(AuxpowFetch::NonAuxpow)
        }
        Ok(ParsedNamecoinBlock::Auxpow(parsed)) => parsed_candidate_or_malformed(Ok(parsed)),
        Err(err) => parsed_candidate_or_malformed(Err(err)),
    }
}

async fn fetch_header_blob_candidate(
    rpc: &impl BitcoindRpc,
    spec: &'static ChainSpec,
    family: &'static FamilySpec,
    block_hash: &BlockHash,
    height: i32,
    exact_version: i32,
) -> Result<AuxpowFetch> {
    let blob = rpc
        .get_header_with_auxpow(block_hash)
        .await
        .with_context(|| format!("get {} header+AuxPoW for {block_hash}", family.label))?;
    // Cadence Mining: only the exact merge-mined version carries a
    // CAuxPow. Skip every other class (including 0x20260100, which
    // sets the generic AuxPoW bit but is not merge-mined) WITHOUT
    // attempting to parse.
    if !is_merge_mined(&blob, exact_version)
        .with_context(|| format!("read {} header version at height {height}", family.label))?
    {
        debug!(
            chain = spec.slug,
            height,
            block_hash = %block_hash,
            "skipping non-merge-mined block"
        );
        return Ok(AuxpowFetch::NonAuxpow);
    }
    parsed_candidate_or_malformed(parse_auxpow_header_blob(&blob).map(Box::new))
}

/// Fold a classic-CAuxPow parse result into an [`AuxpowFetch`]. The malformed
/// arm carries the full error chain so the family's malformed policy can log it
/// and, for hold-policy chains, persist it as `capture_error.detail`.
fn parsed_candidate_or_malformed(parsed: Result<Box<ParsedAuxpowBlock>>) -> Result<AuxpowFetch> {
    match parsed {
        Ok(parsed) => Ok(AuxpowFetch::Classic(parsed)),
        Err(err) => Ok(AuxpowFetch::Malformed(format!("{err:#}"))),
    }
}

async fn attach_child_payout_if_needed(
    rpc: &impl BitcoindRpc,
    context: &AuxpowCaptureContext,
    block_hash: &BlockHash,
    parsed: &mut ParsedAuxpowBlock,
) -> Result<()> {
    let family = context.family();
    let label = family.label;
    if matches!(family.fetch, FetchStrategy::HeaderBlob { .. }) && family.child_payout.is_some() {
        let raw = rpc.get_block_raw(block_hash).await.with_context(|| {
            format!("get full {label} child block {block_hash} for reward identity")
        })?;
        let child = parse_child_block_coinbase(&raw)
            .with_context(|| format!("parse full {label} child block {block_hash} coinbase"))?;
        attach_child_block_coinbase(parsed, child).with_context(|| {
            format!("pair full {label} child block {block_hash} with AuxPoW proof")
        })?;
    }
    Ok(())
}

/// Live capture chain for the bitcoind family. Heights up to the tip always
/// exist, so `process_height` never returns `Retry`; it advances past every
/// captured or skipped height and returns `Hold` only for a malformed proof
/// under either hold policy (see `height_progress_for`).
struct AuxpowFamilyPoller {
    state: ChainPollerState,
    rpc: BitcoindRpcClient,
    context: AuxpowCaptureContext,
}

impl AuxpowFamilyPoller {
    /// Bundle the owned Postgres client, RPC client, and bootstrapped capture
    /// context into the `ChainPoller` the generic `Poller` drives. The poller
    /// owns its connections for the lifetime of `run_forever`.
    fn new(
        spec: &'static ChainSpec,
        client: Client,
        rpc: BitcoindRpcClient,
        context: AuxpowCaptureContext,
    ) -> Self {
        Self {
            state: ChainPollerState::new(spec, context.source_id(), client),
            rpc,
            context,
        }
    }
}

impl ChainPoller for AuxpowFamilyPoller {
    fn poller_state(&self) -> &ChainPollerState {
        &self.state
    }

    fn client_mut(&mut self) -> &mut Client {
        &mut self.state.client
    }

    /// Current child-chain tip via `getblockcount`, the upper bound the poller
    /// advances the cursor toward (the cursor table, never `MAX(child_height)`).
    async fn chain_tip(&self) -> Result<i32> {
        self.rpc.get_block_count().await
    }
    fn chain_rpc_metrics(&self) -> Option<mmm_rpc::RpcMetrics> {
        Some(self.rpc.metrics())
    }
    fn parent_classifier(&self) -> Option<&ConfiguredParentClassifier> {
        Some(self.context.parent_classifier())
    }

    async fn refresh_core_cache(&mut self) -> Result<()> {
        self.context
            .base
            .refresh_core_header_cache(&mut self.state.client)
            .await?;
        Ok(())
    }

    /// Capture one height. Every height up to the tip exists in a bitcoind
    /// chain, so there is no Retry case here (unlike the header-pull divergent
    /// chains); the one non-advancing outcome is a held malformed proof under
    /// either hold policy, whose `capture_error` row is already
    /// persisted by the time this returns. `Hold` blocks the cursor in the new
    /// sub-range; in the best-effort replay sub-range the driver continues, and
    /// the persisted row is what keeps that gap visible.
    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            process_auxpow_height(&mut self.state.client, &self.rpc, &self.context, height).await?;
        Ok(height_progress_for(outcome))
    }

    async fn rescan_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            rescan_auxpow_height(&mut self.state.client, &self.rpc, &self.context, height).await?;
        Ok(match outcome {
            RescanOutcome::Unchanged => HeightProgress::Advance,
            RescanOutcome::Captured(outcome) => height_progress_for(outcome),
        })
    }
}

/// Whether the live cursor may move past a captured height. Only a held
/// malformed proof blocks it; a skip (non-AuxPoW, or malformed under the
/// skip-and-continue policy) advances exactly as it always has.
fn height_progress_for(outcome: HeightOutcome) -> HeightProgress {
    match outcome {
        HeightOutcome::MalformedHeld => HeightProgress::Hold,
        HeightOutcome::AuxpowWritten
        | HeightOutcome::NonAuxpowSkipped
        | HeightOutcome::MalformedSkipped => HeightProgress::Advance,
    }
}

/// Registry-dispatched live-poll entry point for bitcoind-family chains.
pub(crate) async fn poll(spec: &'static ChainSpec, rt: ProducerRuntime) -> Result<()> {
    let rpc_config = crate::chains::config::bitcoind_rpc_config(spec)?;
    let rpc = BitcoindRpcClient::new(family_of(spec).label, rpc_config)?;
    ensure_mainnet_endpoint(&rpc, family_of(spec)).await?;
    let poller_config = crate::chains::config::poller_config(spec)?;
    let context =
        AuxpowCaptureContext::new_with_classifier(&rt.pg_client, spec, rt.parent_classifier)
            .await?;
    let poller = Poller::new(
        AuxpowFamilyPoller::new(spec, rt.pg_client, rpc, context),
        poller_config,
    )
    .await?;
    poller.run_forever().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chains::spec::{ChainId, by_id};

    // Relocated UNCHANGED from src/fractal_capture.rs (slice 4): these are the
    // version gate's behavioral guard. Fixture includes are re-anchored on
    // CARGO_MANIFEST_DIR per the consolidation relocation rule.

    #[test]
    fn version_gate_accepts_merge_mined_and_skips_cadence() {
        let exact_version = match family_of(by_id(ChainId::Fractal)).fetch {
            FetchStrategy::HeaderBlob { exact_version } => exact_version,
            other => panic!("Fractal must use a header-blob version gate, got {other:?}"),
        };

        // Positive: real FB 1,342,257 (child version 0x20240100, merge-mined).
        let positive = hex::decode(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/fractal/fb-1342257-getblockheader-auxpow.hex"
            ))
            .trim(),
        )
        .expect("decode positive fixture");
        // Negative: real FB 1,824,355 (child version 0x20260100 Cadence; it sets
        // the generic 0x100 AuxPoW bit but is NOT merge-mined).
        let negative = hex::decode(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/fractal/fb-1824355-getblockheader-auxpow.hex"
            ))
            .trim(),
        )
        .expect("decode negative fixture");

        assert_eq!(child_header_version(&positive).unwrap(), exact_version);
        assert!(is_merge_mined(&positive, exact_version).unwrap());

        // The Cadence class must NOT pass the exact-version gate, so the producer
        // skips it (NonAuxpowSkipped) without invoking parse_auxpow_header_blob.
        assert_ne!(child_header_version(&negative).unwrap(), exact_version);
        assert!(!is_merge_mined(&negative, exact_version).unwrap());
    }

    #[test]
    fn merge_mined_payload_uses_rpc_height_and_nulls_child_fields() {
        use mmm_capture::capture::{ParentKind, ResolvedPoolAttributions};

        let raw = hex::decode(
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../fixtures/fractal/fb-1342257-getblockheader-auxpow.hex"
            ))
            .trim(),
        )
        .expect("decode fixture");
        let parsed = parse_auxpow_header_blob(&raw).expect("parse blob");

        // Isolates the Fractal-specific payload assembly (child height, NULL
        // child fields, preserved proof), not pool resolution.
        let payload = build_event_payload(
            &parsed,
            Some(1_342_257),
            ResolvedPoolAttributions::default(),
            ClassificationProof::default(),
            1_800_000_000,
        )
        .expect("build payload");

        // The RPC height owns this field; the blob has no child coinbase height.
        assert_eq!(payload.child_height, Some(1_342_257));
        // No child coinbase in the getblockheader-auxpow proof -> child fields NULL.
        assert_eq!(payload.child_coinbase_txid, None);
        assert_eq!(payload.child_coinbase_script, None);
        assert!(payload.pool_attributions.is_empty());
        // The CAuxPow proof bytes are preserved.
        assert!(
            payload
                .aux_merkle_proof
                .as_ref()
                .is_some_and(|bytes| !bytes.is_empty())
        );
        // The BTC parent is the known stale at height 928455 (wire-order hash);
        // child-target validation populates for the 80-byte Fractal child header.
        assert_eq!(
            hex::encode(&payload.btc_parent_header_hash),
            "7fd4a366c064bd5ca394d172e8e5490e380267ad8e3900000000000000000000"
        );
        assert!(payload.pow_validates_btc_target);
        assert_eq!(payload.pow_validates_child_target, Some(true));
        // Without classifier proof, a PoW-valid parent is unknown (no placement).
        assert_eq!(payload.btc_parent_kind, ParentKind::Unknown);
    }

    /// Only a held malformed proof blocks the live cursor. The incumbent
    /// skip-and-continue outcome advances exactly as it always has.
    #[test]
    fn only_a_held_malformed_proof_blocks_the_cursor() {
        assert_eq!(
            height_progress_for(HeightOutcome::MalformedHeld),
            HeightProgress::Hold
        );
        for advancing in [
            HeightOutcome::AuxpowWritten,
            HeightOutcome::NonAuxpowSkipped,
            HeightOutcome::MalformedSkipped,
        ] {
            assert_eq!(height_progress_for(advancing), HeightProgress::Advance);
        }
    }
}
