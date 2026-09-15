//! The shared capture, poll, and backfill implementation for bitcoind-family
//! AuxPoW chains (Namecoin, Syscoin, Fractal, Qbit).
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

use crate::chains::backfill::{
    BackfillConfig, BackfillHeightEffect, BackfillSummary, run_delayed_backfill_range,
};
use crate::chains::bitcoind_rpc::{BitcoindRpc, BitcoindRpcClient};
use crate::chains::child_payout_registry::seed_child_payout_identities_for;
use crate::chains::spec::{ChainSpec, FamilySpec, FetchStrategy, MalformedPolicy, RepairScope};
use crate::poller::{ChainPoller, ChainPollerState, HeightProgress, Poller};
use crate::producer_runtime::{ProducerContext, ProducerRuntime, run_post_backfill_repair};
use mmm_bitcoin_core::ConfiguredParentClassifier;
use mmm_capture::auxpow::{
    ParsedAuxpowBlock, ParsedNamecoinBlock, ParsedQbitAuxpow, attach_child_block_coinbase,
    parse_auxpow_header_blob, parse_child_block_coinbase, parse_namecoin_block,
};
use mmm_capture::capture::{
    ClassificationProof, MergeMiningEventPayload, build_event_payload, now_epoch_seconds,
    resolve_event_pools_with_child_payout,
};
use mmm_capture::child_payout::PoolIdentityLookup;
use mmm_capture::pool_resolver::PoolResolver;
use mmm_read_model::capture_in_txn;
use mmm_store::{
    CAPTURE_ERROR_MALFORMED_AUXPOW_PROOF, clear_capture_error, finish_child_chain_height_operation,
    load_pool_identities_by_namespace, lock_child_chain_height_session, record_capture_error,
    record_child_chain_block, record_child_chain_block_in_own_transaction,
    upsert_merge_mining_event_with_attributions,
};
use qbit::{ensure_qbit_mainnet_endpoint, fetch_qbit_candidate, write_qbit_event};

mod qbit;

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
/// is `FamilySpec::malformed_policy`, not a property of the failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeightOutcome {
    /// An AuxPoW event was upserted for this height.
    AuxpowWritten,
    /// The block carries no merge-mining proof (or fails the version gate);
    /// nothing written.
    NonAuxpowSkipped,
    /// `MalformedPolicy::SkipAndContinue`: the block claimed AuxPoW but failed
    /// to parse; logged and skipped without writing.
    MalformedSkipped,
    /// `MalformedPolicy::HoldInterval`: same failure, but a `capture_error`
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
    let result = process_locked_height(client, rpc, context, height).await;
    finish_child_chain_height_operation(client, source_id, height, result).await
}

/// One height's observation and writes, under the session-level height lock
/// [`process_auxpow_height`] holds around it.
async fn process_locked_height(
    client: &mut Client,
    rpc: &impl BitcoindRpc,
    context: &AuxpowCaptureContext,
    height: i32,
) -> Result<HeightOutcome> {
    let family = context.family();
    let label = family.label;

    let block_hash = rpc
        .get_block_hash(height)
        .await
        .with_context(|| format!("get {label} block hash at height {height}"))?;

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

    if outcome != HeightOutcome::AuxpowWritten {
        record_child_chain_block_in_own_transaction(
            client,
            context.source_id(),
            height,
            block_hash.as_ref(),
            now_epoch_seconds()?,
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
    if family.malformed_policy == MalformedPolicy::HoldInterval
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
/// `HoldInterval` the `capture_error` row is written BEFORE this returns, so a
/// crash between detection and return cannot lose the signal.
async fn record_malformed_height(
    client: &mut Client,
    context: &AuxpowCaptureContext,
    height: i32,
    block_hash: &BlockHash,
    detail: String,
) -> Result<HeightOutcome> {
    let spec = context.spec;
    match context.family().malformed_policy {
        MalformedPolicy::SkipAndContinue => {
            error!(
                chain = spec.slug,
                height,
                block_hash = %block_hash,
                error = %detail,
                "malformed AuxPoW block skipped without writing an event"
            );
            Ok(HeightOutcome::MalformedSkipped)
        }
        MalformedPolicy::HoldInterval => {
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
            record_child_chain_block(txn, source_id, child_height, child_block_hash, observed_at)
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
        FetchStrategy::RawBlock => {
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
    match parse_namecoin_block(&raw) {
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
/// under the `HoldInterval` policy (see `height_progress_for`).
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
    /// `MalformedPolicy::HoldInterval`, whose `capture_error` row is already
    /// persisted by the time this returns. `Hold` blocks the cursor in the new
    /// sub-range; in the best-effort replay sub-range the driver continues, and
    /// the persisted row is what keeps that gap visible.
    async fn process_height(&mut self, height: i32) -> Result<HeightProgress> {
        let outcome =
            process_auxpow_height(&mut self.state.client, &self.rpc, &self.context, height).await?;
        Ok(height_progress_for(outcome))
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
    ensure_qbit_mainnet_endpoint(&rpc, family_of(spec)).await?;
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

/// Registry-dispatched backfill entry point for bitcoind-family chains.
pub(crate) async fn backfill(rt: ProducerRuntime, config: BackfillConfig) -> Result<()> {
    let spec = config.spec;
    let rpc_config = crate::chains::config::bitcoind_rpc_config(spec)?;
    let rpc = BitcoindRpcClient::new(family_of(spec).label, rpc_config)?;
    run_auxpow_backfill(rt, rpc, config).await
}

/// Run a bounded backfill for a bitcoind-family chain: tip validation, the
/// spec-driven below-floor warning, classifier warning, the per-height capture
/// loop, and post-backfill repair under the spec's scope.
pub(crate) async fn run_auxpow_backfill(
    rt: ProducerRuntime,
    rpc: BitcoindRpcClient,
    config: BackfillConfig,
) -> Result<()> {
    let ProducerRuntime {
        pg_client: mut client,
        parent_classifier,
    } = rt;
    let spec = config.spec;
    let family = family_of(spec);
    ensure_qbit_mainnet_endpoint(&rpc, family).await?;

    let chain_tip = rpc
        .get_block_count()
        .await
        .with_context(|| format!("get {} tip before backfill", spec.display_name))?;
    config.validate_against_tip(chain_tip)?;

    if let Some(message) = family.floor_warning
        && config.start_height < spec.activation_floor
    {
        warn!(
            start_height = config.start_height,
            first_auxpow_height = spec.activation_floor,
            "{message}"
        );
    }

    let context =
        AuxpowCaptureContext::new_with_classifier(&client, spec, parent_classifier).await?;
    info!(
        chain = spec.slug,
        start_height = config.start_height,
        end_height = config.end_height,
        chain_tip,
        "starting bounded AuxPoW backfill"
    );

    let summary = run_delayed_backfill_range(&config, 0, async |height| {
        let outcome = process_auxpow_height(&mut client, &rpc, &context, height).await?;
        Ok(auxpow_backfill_effect(outcome))
    })
    .await?;

    if summary.malformed_held > 0 {
        warn!(
            chain = spec.slug,
            processed = summary.processed,
            auxpow_written = summary.auxpow_written,
            non_auxpow_skipped = summary.non_auxpow_skipped,
            malformed_skipped = summary.malformed_skipped,
            malformed_held = summary.malformed_held,
            "bounded AuxPoW backfill left unresolved capture errors"
        );
    } else {
        info!(
            chain = spec.slug,
            processed = summary.processed,
            auxpow_written = summary.auxpow_written,
            non_auxpow_skipped = summary.non_auxpow_skipped,
            malformed_skipped = summary.malformed_skipped,
            "completed bounded AuxPoW backfill"
        );
    }

    let repair_scope = match family.repair_scope {
        RepairScope::Global => None,
        RepairScope::SourceScoped => Some(spec.source_code),
    };
    run_post_backfill_repair(
        &mut client,
        context.parent_classifier(),
        repair_scope,
        config.start_height,
        config.end_height,
        &format!("{} backfill", spec.display_name),
    )
    .await?;

    // Everything captured in the range is written and reconciled; the run
    // itself is still not a success. Reporting completion over a hole is the
    // failure this policy exists to prevent.
    ensure_backfill_complete(spec, &config, &summary)
}

/// The bounded-backfill completion verdict. A range that left any held
/// malformed proof is incomplete and must exit non-zero, whatever else it
/// captured; the operator re-runs the range once the capture errors clear.
fn ensure_backfill_complete(
    spec: &ChainSpec,
    config: &BackfillConfig,
    summary: &BackfillSummary,
) -> Result<()> {
    ensure!(
        summary.malformed_held == 0,
        "{} backfill {}..{} is incomplete: {} height(s) hold an unresolved capture error; \
         resolve them and re-run the range",
        spec.display_name,
        config.start_height,
        config.end_height,
        summary.malformed_held,
    );
    Ok(())
}

fn auxpow_backfill_effect(outcome: HeightOutcome) -> BackfillHeightEffect {
    match outcome {
        HeightOutcome::AuxpowWritten => BackfillHeightEffect::AuxpowWritten,
        HeightOutcome::NonAuxpowSkipped => BackfillHeightEffect::NonAuxpowSkipped,
        HeightOutcome::MalformedSkipped => BackfillHeightEffect::MalformedSkipped,
        HeightOutcome::MalformedHeld => BackfillHeightEffect::MalformedHeld,
    }
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

    /// A bounded backfill over a range containing a held malformed proof must
    /// fail the run, naming the range, rather than logging completion.
    #[test]
    fn backfill_over_a_held_height_is_reported_incomplete() {
        let spec = by_id(ChainId::Qbit);
        let config = BackfillConfig::from_args(spec, ["78050", "78060"]).expect("parse range");

        let clean = BackfillSummary {
            processed: 2,
            auxpow_written: 1,
            non_auxpow_skipped: 1,
            ..BackfillSummary::default()
        };
        ensure_backfill_complete(spec, &config, &clean)
            .expect("a clean range completes successfully");

        let held = BackfillSummary {
            processed: 3,
            malformed_held: 1,
            ..clean
        };
        let err = ensure_backfill_complete(spec, &config, &held)
            .expect_err("a held height must fail the run");
        assert_eq!(
            err.to_string(),
            "Qbit backfill 78050..78060 is incomplete: 1 height(s) hold an unresolved \
             capture error; resolve them and re-run the range"
        );
    }

    /// The malformed-outcome to backfill-counter mapping keeps the two policies
    /// in separate columns, so a held height can never be tallied as a skip.
    #[test]
    fn backfill_effects_keep_held_and_skipped_separate() {
        assert_eq!(
            auxpow_backfill_effect(HeightOutcome::MalformedHeld),
            BackfillHeightEffect::MalformedHeld
        );
        assert_eq!(
            auxpow_backfill_effect(HeightOutcome::MalformedSkipped),
            BackfillHeightEffect::MalformedSkipped
        );
    }
}
