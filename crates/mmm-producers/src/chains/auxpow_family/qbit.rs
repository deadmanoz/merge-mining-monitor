//! The Qbit format adapter for the shared bitcoind-family runner.
//!
//! Qbit is the one family chain whose proof is not a classic `CAuxPow`: its
//! extended header has no `hashBlock` field, so `ParsedQbitAuxpow` is a
//! distinct type that cannot travel the `ParsedAuxpowBlock` write path. This
//! module owns everything that follows from that - splitting the exact
//! extended-header prefix out of a whole RPC block, authenticating native chain
//! placement (which the decoder deliberately does not do), and projecting the
//! verified proof into the chain-agnostic `NormalizedEventEvidence`. The shared
//! runner in `mod.rs` stays format-agnostic.

use anyhow::{Context, Result, ensure};
use bitcoin::BlockHash;
use bitcoin::consensus::serialize;
use bitcoin::hashes::Hash as _;
use tokio_postgres::Client;
use tracing::debug;

use super::{AuxpowCaptureContext, AuxpowFetch, write_event_in_txn};
use crate::chains::bitcoind_rpc::BitcoindRpc;
use crate::chains::spec::{ChainSpec, FamilySpec, FetchStrategy};
use mmm_capture::auxpow::{
    ParsedQbitAuxpow, ParsedQbitBlock, parse_qbit_extended_header, qbit_extended_header_prefix,
    validates_target,
};
use mmm_capture::capture::{
    ClassificationProof, NormalizedEventEvidence, ResolvedPoolAttributions,
    build_event_payload_from_evidence, now_epoch_seconds,
    resolve_parent_pool_attribution_from_coinbase,
};

/// Fetch and verify one Qbit height. Qbit's `getblock <hash> 0` returns the
/// complete block, so the exact extended-header prefix is split out first and
/// only that is handed to the decoder, which rejects trailing bytes; the block
/// body is never reinterpreted.
///
/// Native placement is authenticated HERE, because the decoder deliberately
/// does not: `child_height` is a caller claim to it and it pins no chain
/// identity. Both checks are hard errors, not capture errors: a node that
/// answers `getblock <hash>` with a block that does not hash to `hash`, or that
/// answers height 0 with a foreign genesis, is not a Qbit mainnet node and no
/// height from it can be trusted.
pub(super) async fn fetch_qbit_candidate(
    rpc: &impl BitcoindRpc,
    spec: &'static ChainSpec,
    family: &'static FamilySpec,
    block_hash: &BlockHash,
    height: i32,
    genesis_block_hash: &'static str,
) -> Result<AuxpowFetch> {
    let raw = rpc
        .get_block_raw(block_hash)
        .await
        .with_context(|| format!("get raw {} block {block_hash}", family.label))?;
    let claimed_height = u32::try_from(height)
        .with_context(|| format!("{} height {height} is not a native height", family.label))?;

    let parsed = match qbit_extended_header_prefix(&raw, claimed_height)
        .and_then(|prefix| parse_qbit_extended_header(prefix, claimed_height))
    {
        Ok(parsed) => parsed,
        Err(err) => return Ok(AuxpowFetch::Malformed(format!("{err:#}"))),
    };

    let child_header = match &parsed {
        ParsedQbitBlock::Direct(header) => header,
        ParsedQbitBlock::Auxpow(auxpow) => &auxpow.child_header,
    };
    ensure_qbit_native_placement(
        family.label,
        height,
        block_hash,
        child_header.hash(),
        genesis_block_hash,
    )?;

    match parsed {
        ParsedQbitBlock::Direct(_) => {
            debug!(
                chain = spec.slug,
                height,
                block_hash = %block_hash,
                "skipping directly mined block"
            );
            Ok(AuxpowFetch::NonAuxpow)
        }
        ParsedQbitBlock::Auxpow(parsed) => Ok(AuxpowFetch::Qbit(parsed)),
    }
}

/// Pin the endpoint to Qbit mainnet once, at producer startup, before any
/// height is accepted. The per-height guard below only reaches its genesis
/// comparison when height 0 itself is processed, which the default poller
/// (seeded at `tip - reorg_depth`) and any backfill starting above zero never
/// do. Without this, a testnet or fork endpoint using Qbit's format and chain
/// ID would pass every per-height check and be persisted as `auxpow:qbit`.
/// A no-op for every other fetch strategy.
pub(super) async fn ensure_qbit_mainnet_endpoint(
    rpc: &impl BitcoindRpc,
    family: &'static FamilySpec,
) -> Result<()> {
    let FetchStrategy::QbitExtendedHeader { genesis_block_hash } = family.fetch else {
        return Ok(());
    };
    let actual = rpc
        .get_block_hash(0)
        .await
        .with_context(|| format!("get {} genesis hash to pin the endpoint", family.label))?;
    ensure_qbit_genesis(family.label, &actual, genesis_block_hash)
}

/// The pure comparison behind [`ensure_qbit_mainnet_endpoint`], split out so
/// the refusal is unit-testable without an RPC endpoint.
fn ensure_qbit_genesis(label: &str, actual: &BlockHash, genesis_block_hash: &str) -> Result<()> {
    ensure!(
        actual.to_string() == genesis_block_hash,
        "{label} endpoint height 0 is {actual} but mainnet genesis is {genesis_block_hash}; \
         refusing to capture from a non-mainnet node",
    );
    Ok(())
}

/// Authenticate that a decoded Qbit block really is the mainnet block at
/// `height`. The decoder treats `child_height` as a caller claim and pins no
/// chain identity, so both checks live here. Hard errors, not capture errors: a
/// node that answers `getblock <hash>` with a block that does not hash to
/// `hash`, or answers height 0 with a foreign genesis, is not a Qbit mainnet
/// node and no height from it can be trusted.
fn ensure_qbit_native_placement(
    label: &str,
    height: i32,
    block_hash: &BlockHash,
    child_block_hash: BlockHash,
    genesis_block_hash: &str,
) -> Result<()> {
    ensure!(
        child_block_hash == *block_hash,
        "{label} block at height {height} hashes to {child_block_hash} \
         but was returned for {block_hash}",
    );
    ensure!(
        height != 0 || child_block_hash.to_string() == genesis_block_hash,
        "{label} height 0 is {child_block_hash} but mainnet genesis is {genesis_block_hash}",
    );
    Ok(())
}

/// Write one Qbit event. The proof is projected straight into
/// `NormalizedEventEvidence`: the stored `aux_merkle_proof` is the exact
/// `auxpow_bytes` region `decode_qbit_auxpow_proof` re-accepts, and the child
/// height is the authenticated RPC height, not a coinbase claim. Child coinbase
/// fields stay NULL because the extended header carries no child coinbase, and
/// Qbit publishes no child payout registry, so attribution is BTC-parent-side
/// only.
pub(super) async fn write_qbit_event(
    client: &mut Client,
    context: &AuxpowCaptureContext,
    height: i32,
    parsed: &ParsedQbitAuxpow,
) -> Result<()> {
    let attributions = resolve_parent_pool_attribution_from_coinbase(
        &parsed.parent_coinbase_script,
        &parsed.parent_coinbase_output_addresses,
        &context.resolver,
        context.base.pool_ids_by_slug(),
    )
    .into_iter()
    .collect::<Vec<_>>();

    let now = now_epoch_seconds()?;
    let mut payload = build_event_payload_from_evidence(
        qbit_evidence(parsed, height),
        ResolvedPoolAttributions { attributions },
        ClassificationProof::default(),
        now,
    )?;
    write_event_in_txn(client, context, &mut payload).await
}

/// Project a verified Qbit proof into the chain-agnostic evidence type. Pure,
/// so the persisted-field contract is testable from the mainnet control
/// fixtures without a node or a database.
fn qbit_evidence(parsed: &ParsedQbitAuxpow, height: i32) -> NormalizedEventEvidence {
    NormalizedEventEvidence {
        // The authenticated RPC height, not a coinbase claim: the Qbit
        // extended header carries no child coinbase to read one from.
        child_height: Some(height),
        child_block_hash: Some(parsed.child_header.hash().to_byte_array().to_vec()),
        child_header_bytes: Some(parsed.child_header.consensus_bytes()),
        child_block_time: Some(i64::from(parsed.child_header.time())),
        child_nbits: Some(parsed.child_header.bits().to_consensus()),
        btc_parent_header: parsed.parent_header.header,
        // Qbit's effective target is always the child's own pure-header nBits;
        // the decoder gates on it, so this re-derivation is recorded as
        // evidence rather than assumed.
        pow_validates_child_target: Some(validates_target(
            parsed.parent_header.hash(),
            parsed.child_header.bits(),
        )),
        btc_parent_coinbase_txid: Some(parsed.parent_coinbase_txid.to_byte_array().to_vec()),
        btc_parent_coinbase_script: Some(parsed.parent_coinbase_script.clone()),
        btc_parent_coinbase_outputs: Some(serialize(&parsed.parent_coinbase_outputs)),
        btc_parent_coinbase_outputs_text: None,
        btc_parent_coinbase_tx_bytes: None,
        // No child coinbase on the wire, and Qbit publishes no child payout
        // registry: these stay NULL rather than carrying a placeholder.
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: Some(parsed.auxpow_bytes.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::family_of;
    use super::*;
    use crate::chains::spec::{ChainId, FetchStrategy, by_id};
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct QbitControlsFile {
        controls: Vec<QbitControl>,
    }

    #[derive(Deserialize)]
    struct QbitControl {
        height: u32,
        header_hex: String,
        parent_hash: String,
        parent_self_pow: bool,
    }

    /// The mainnet control whose embedded parent is a full-difficulty Bitcoin
    /// block (Qbit 78,058 witnessing BTC 966,017).
    fn positive_control() -> QbitControl {
        serde_json::from_str::<QbitControlsFile>(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/qbit/qbit_controls.json"
        )))
        .expect("parse qbit controls fixture")
        .controls
        .into_iter()
        .find(|control| control.parent_self_pow)
        .expect("positive control present")
    }

    fn parsed_positive_control() -> (QbitControl, ParsedQbitAuxpow) {
        let control = positive_control();
        let raw = hex::decode(&control.header_hex).expect("decode control hex");
        match parse_qbit_extended_header(&raw, control.height).expect("parse control") {
            ParsedQbitBlock::Auxpow(parsed) => (control, *parsed),
            ParsedQbitBlock::Direct(_) => panic!("control must be a merged proof"),
        }
    }

    /// Every field a Qbit event persists, from the mainnet control. The child
    /// coinbase columns must stay NULL (the wire format has none) and the
    /// stored proof must be exactly the bytes `decode_qbit_auxpow_proof`
    /// re-accepts.
    #[test]
    fn qbit_evidence_projects_every_persisted_field_from_the_control() {
        use mmm_capture::auxpow::decode_qbit_auxpow_proof;
        use mmm_capture::capture::{ParentKind, ResolvedPoolAttributions};

        let (control, parsed) = parsed_positive_control();
        let evidence = qbit_evidence(&parsed, control.height as i32);

        // Child evidence: the authenticated RPC height and the real header.
        assert_eq!(evidence.child_height, Some(78_058));
        assert_eq!(
            evidence.child_block_hash.as_deref().map(hex::encode),
            Some(hex::encode(parsed.child_header.hash().to_byte_array()))
        );
        assert_eq!(
            evidence.child_header_bytes,
            Some(parsed.child_header.consensus_bytes())
        );
        assert_eq!(
            evidence.child_block_time,
            Some(i64::from(parsed.child_header.time()))
        );
        assert_eq!(
            evidence.child_nbits,
            Some(parsed.child_header.bits().to_consensus())
        );
        // Qbit's parent must meet the CHILD target; the proof would not have
        // parsed otherwise, and the evidence records the re-derivation.
        assert_eq!(evidence.pow_validates_child_target, Some(true));
        // No child coinbase exists on the wire: never a placeholder.
        assert_eq!(evidence.child_coinbase_txid, None);
        assert_eq!(evidence.child_coinbase_script, None);
        assert_eq!(evidence.child_coinbase_outputs, None);
        // The stored proof is exactly the region the Qbit decoder re-accepts.
        let stored = evidence.aux_merkle_proof.clone().expect("proof stored");
        assert_eq!(stored, parsed.auxpow_bytes);
        assert_eq!(
            decode_qbit_auxpow_proof(&stored)
                .expect("stored proof re-decodes")
                .parent_header_hash,
            parsed.parent_header.hash()
        );

        let payload = build_event_payload_from_evidence(
            evidence,
            ResolvedPoolAttributions::default(),
            ClassificationProof::default(),
            1_800_000_000,
        )
        .expect("build payload");
        assert_eq!(
            payload.btc_parent_header_hash,
            parsed.parent_header.hash().to_byte_array().to_vec()
        );
        // The positive control's parent is a full-difficulty Bitcoin block.
        assert!(payload.pow_validates_btc_target);
        assert_eq!(
            payload.btc_parent_header_time,
            i64::from(parsed.parent_header.time())
        );
        assert_eq!(
            payload.btc_parent_coinbase_txid,
            Some(parsed.parent_coinbase_txid.to_byte_array().to_vec())
        );
        assert_eq!(
            payload.btc_parent_coinbase_script,
            Some(parsed.parent_coinbase_script.clone())
        );
        // Without classifier proof a PoW-valid parent is Unknown (no placement).
        assert_eq!(payload.btc_parent_kind, ParentKind::Unknown);
        assert_eq!(
            hex::encode(&payload.btc_parent_header_hash),
            hex::encode(
                hex::decode(&control.parent_hash)
                    .expect("decode control parent hash")
                    .into_iter()
                    .rev()
                    .collect::<Vec<u8>>()
            )
        );
    }

    /// Native placement is the producer's job, so both guards must fire: a
    /// block that does not hash to the hash it was requested for, and a foreign
    /// genesis at height 0.
    #[test]
    fn native_placement_guard_rejects_wrong_hash_and_wrong_genesis() {
        let (control, parsed) = parsed_positive_control();
        let genesis = match family_of(by_id(ChainId::Qbit)).fetch {
            FetchStrategy::QbitExtendedHeader { genesis_block_hash } => genesis_block_hash,
            other => panic!("Qbit must use the extended-header fetch, got {other:?}"),
        };
        let child_hash = parsed.child_header.hash();
        let height = control.height as i32;

        // The happy path: the node returned the block it was asked for.
        ensure_qbit_native_placement("Qbit", height, &child_hash, child_hash, genesis)
            .expect("matching hash is accepted");

        // A node answering getblock <hash> with a different block.
        let foreign = parsed.parent_header.hash();
        let err = ensure_qbit_native_placement("Qbit", height, &foreign, child_hash, genesis)
            .expect_err("mismatched hash must be rejected");
        assert!(
            err.to_string().contains("but was returned for"),
            "unexpected error: {err}"
        );

        // A node whose height 0 is not Qbit mainnet genesis.
        let err = ensure_qbit_native_placement("Qbit", 0, &child_hash, child_hash, genesis)
            .expect_err("foreign genesis must be rejected");
        assert!(
            err.to_string().contains("mainnet genesis is"),
            "unexpected error: {err}"
        );

        // ... and the real genesis hash passes the height-0 gate.
        let genesis_hash: BlockHash = genesis.parse().expect("parse pinned genesis");
        ensure_qbit_native_placement("Qbit", 0, &genesis_hash, genesis_hash, genesis)
            .expect("pinned genesis is accepted at height 0");
    }

    /// The startup pin is what protects a poller seeded above height 0 and a
    /// backfill that never touches genesis: the per-height guard above cannot
    /// see those. An endpoint whose `getblockhash(0)` is not Qbit mainnet
    /// genesis must be refused outright, and the real genesis must pass.
    #[test]
    fn startup_pin_refuses_a_non_mainnet_endpoint() {
        let genesis = match family_of(by_id(ChainId::Qbit)).fetch {
            FetchStrategy::QbitExtendedHeader { genesis_block_hash } => genesis_block_hash,
            other => panic!("Qbit must use the extended-header fetch, got {other:?}"),
        };
        let genesis_hash: BlockHash = genesis.parse().expect("parse pinned genesis");
        ensure_qbit_genesis("Qbit", &genesis_hash, genesis).expect("mainnet genesis is accepted");

        // A fork or testnet sharing Qbit's format and chain ID but not its genesis.
        let (_, parsed) = parsed_positive_control();
        let foreign = parsed.child_header.hash();
        let err = ensure_qbit_genesis("Qbit", &foreign, genesis)
            .expect_err("a foreign genesis must be refused before any height is accepted");
        assert!(
            err.to_string()
                .contains("refusing to capture from a non-mainnet node"),
            "unexpected error: {err}"
        );
    }
}
