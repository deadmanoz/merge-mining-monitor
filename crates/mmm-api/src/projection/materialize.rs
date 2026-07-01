//! The neutral parent-projection pipeline shared by tree and block projections.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;

use super::shared::{
    BlockRow, ChildChainEvidence, DisplayMinerBasis, EventRow, PoolObject, ProofState,
    SourceOnlyRow, SourceRecord, SourceSummary, TreeCompetition, display_hash,
    ensure_unknown_btc_target, group_events, group_sources, resolve_display_miner, unknown_pool,
};
use crate::normalize::ParentKind;
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;

/// The neutral per-parent projection: one Bitcoin parent (block-backed or
/// direct-event-only) before tree-vs-block-detail shaping. `evidence` and
/// `protected` are mutated post-construction by the tree builder
/// (tree::mod/anchor/compact) to drive canonical-reduction; both start false
/// here. `hash_bytes` is the wire/internal-order key for joins; `hash`/
/// `prev_hash` are explorer-hex (display_hash-reversed).
#[derive(Debug, Clone)]
pub(super) struct ParentProjection {
    pub(super) hash_bytes: Vec<u8>,
    pub(super) hash: String,
    pub(super) prev_hash: String,
    pub(super) height: Option<i32>,
    pub(super) kind: ParentKind,
    pub(super) btc_orphan_class: Option<String>,
    pub(super) header_time: i64,
    pub(super) bitcoin_miner_pool: PoolObject,
    /// Best-available display miner: the strict `bitcoin_miner_pool` when known,
    /// otherwise the chain-agnostic child-inferred fallback (see
    /// `resolve_display_miner`). Never overrides the strict fact.
    pub(super) display_miner_pool: PoolObject,
    pub(super) display_miner_basis: &'static str,
    pub(super) source_summary: SourceSummary,
    pub(super) child_chain_evidence: Vec<ChildChainEvidence>,
    pub(super) proof_state: ProofState,
    pub(super) evidence: bool,
    pub(super) protected: bool,
    pub(super) competition: Option<TreeCompetition>,
}

/// Project block-backed parents into neutral `ParentProjection`s, assembling
/// each per-parent source summary from its events and proofs.
/// Enforces the active-unknown invariant: a block-backed `unknown` whose any
/// active event fails the Bitcoin target is a hard projection-invariant error
/// (never silently served). Honors `source_filter` when admitting the Bitcoin
/// source into the summary.
pub(super) fn project_blocks(
    blocks: &[BlockRow],
    events: &[EventRow],
    proofs: &[SourceOnlyRow],
    sources: &HashMap<String, SourceRecord>,
    source_filter: &[String],
) -> Result<Vec<ParentProjection>> {
    let events_by_hash = group_events(events);
    let proofs_by_hash = group_sources(proofs);
    blocks
        .iter()
        .map(|block| {
            let block_events = events_by_hash.get(&block.hash).cloned().unwrap_or_default();
            ensure_unknown_btc_target(
                block.kind,
                block_events
                    .iter()
                    .map(|event| event.pow_validates_btc_target),
                "active block-backed unknown event fails Bitcoin target",
            )?;
            let block_proofs = proofs_by_hash.get(&block.hash).cloned().unwrap_or_default();
            let source_summary = source_summary_for_block(
                block,
                &block_events,
                &block_proofs,
                sources,
                source_filter,
            );
            let (display_miner_pool, display_miner_basis) = resolve_display_miner(
                &block.bitcoin_miner_pool,
                block_events.iter().map(|event| &event.child_miner_pool),
            );
            Ok(ParentProjection {
                hash_bytes: block.hash.clone(),
                hash: display_hash(&block.hash)?,
                prev_hash: display_hash(&block.prev_hash)?,
                height: block.height,
                kind: block.kind,
                btc_orphan_class: block.btc_orphan_class.clone(),
                header_time: block.header_time,
                bitcoin_miner_pool: block.bitcoin_miner_pool.clone(),
                display_miner_pool,
                display_miner_basis: display_miner_basis.as_str(),
                child_chain_evidence: child_chain_evidence_from_events(block_events.iter()),
                proof_state: ProofState {
                    has_live_observation: source_summary.live_observed,
                    has_auxpow_evidence: !block_events.is_empty() || !block_proofs.is_empty(),
                },
                source_summary,
                evidence: false,
                protected: false,
                competition: None,
            })
        })
        .collect()
}

/// Project parents that have AuxPoW events but NO read-model `block` row
/// (Near/Unknown only) into neutral projections, skipping any hash already
/// covered by a block row. `btc_orphan_class` is None by construction (no
/// block row means no Core-gated orphan verdict, i.e. pending). Same
/// active-unknown-fails-target invariant as project_blocks. Groups by
/// parent_hash via BTreeMap for deterministic ordering.
pub(super) fn project_direct_events(
    events: &[EventRow],
    block_hashes: &HashSet<Vec<u8>>,
) -> Result<Vec<ParentProjection>> {
    let mut grouped: BTreeMap<Vec<u8>, Vec<&EventRow>> = BTreeMap::new();
    for event in events {
        if block_hashes.contains(&event.parent_hash)
            || !matches!(event.kind, ParentKind::Near | ParentKind::Unknown)
        {
            continue;
        }
        grouped
            .entry(event.parent_hash.clone())
            .or_default()
            .push(event);
    }

    let mut projections = Vec::new();
    for (hash, events) in grouped {
        let kind = if events.iter().any(|event| event.kind == ParentKind::Unknown) {
            ParentKind::Unknown
        } else {
            ParentKind::Near
        };
        ensure_unknown_btc_target(
            kind,
            events.iter().map(|event| event.pow_validates_btc_target),
            "active direct unknown event fails Bitcoin target",
        )?;
        let first = events
            .iter()
            .min_by_key(|event| event.id)
            .expect("grouped from at least one event");
        let source_summary = source_summary_from_sources(
            events.iter().map(|event| &event.source),
            kind != ParentKind::Near,
        );
        projections.push(ParentProjection {
            hash_bytes: hash.clone(),
            hash: display_hash(&hash)?,
            prev_hash: display_hash(&first.prev_hash)?,
            height: None,
            kind,
            // Direct-event projections have no read-model `block` row, so no
            // Core-gated orphan class (pending by construction).
            btc_orphan_class: None,
            header_time: first.header_time,
            bitcoin_miner_pool: unknown_pool(),
            // Near/unknown direct-event parents are not validated Bitcoin blocks,
            // so we never infer a miner for them: display stays unknown.
            display_miner_pool: unknown_pool(),
            display_miner_basis: DisplayMinerBasis::Unknown.as_str(),
            child_chain_evidence: child_chain_evidence_from_events(events.iter().copied()),
            proof_state: ProofState {
                has_live_observation: false,
                has_auxpow_evidence: true,
            },
            source_summary,
            evidence: false,
            protected: false,
            competition: None,
        });
    }
    Ok(projections)
}

/// Build the `SourceSummary` for a block-backed parent: union the source
/// records across its events and proofs, then (subject to
/// source_filter) add the synthetic Bitcoin source iff the block is
/// core-attested or live-observed and not already present. pow_validates flag
/// is true unless the parent is an unfproven unknown.
pub(super) fn source_summary_for_block(
    block: &BlockRow,
    events: &[EventRow],
    proofs: &[SourceOnlyRow],
    sources: &HashMap<String, SourceRecord>,
    source_filter: &[String],
) -> SourceSummary {
    let mut records = events
        .iter()
        .map(|event| event.source.clone())
        .collect::<Vec<_>>();
    records.extend(proofs.iter().map(|proof| proof.source.clone()));
    if source_filter_allows(source_filter, BITCOIN_SOURCE_CODE)
        && let (true, Some(source)) = (
            block.core_attested || block.live_observed,
            sources.get(BITCOIN_SOURCE_CODE),
        )
        && !records.iter().any(|record| record.id == source.id)
    {
        records.push(source.clone());
    }
    source_summary_from_sources(
        records.iter(),
        block.pow_validated || block.kind != ParentKind::Unknown,
    )
}

/// Aggregate events into per-(source,child_chain) `ChildChainEvidence`:
/// event_count plus min/max child_height. Grouped via BTreeMap so the
/// serialized array order is deterministic (pinned by the tree/block JSON
/// fixtures).
pub(super) fn child_chain_evidence_from_events<'a>(
    events: impl IntoIterator<Item = &'a EventRow>,
) -> Vec<ChildChainEvidence> {
    let mut grouped =
        BTreeMap::<(String, Option<String>), (usize, Option<i32>, Option<i32>)>::new();
    for event in events {
        let entry = grouped
            .entry((event.source.code.clone(), event.source.chain.clone()))
            .or_insert((0, None, None));
        entry.0 += 1;
        entry.1 = Some(
            entry
                .1
                .map_or(event.child_height, |min| min.min(event.child_height)),
        );
        entry.2 = Some(
            entry
                .2
                .map_or(event.child_height, |max| max.max(event.child_height)),
        );
    }
    grouped
        .into_iter()
        .map(
            |((source, child_chain), (event_count, child_height_min, child_height_max))| {
                ChildChainEvidence {
                    source,
                    child_chain,
                    event_count,
                    child_height_min,
                    child_height_max,
                }
            },
        )
        .collect()
}

/// True iff `code` passes the source filter: an empty filter admits every
/// source, otherwise the code must be explicitly listed. Gates whether the
/// synthetic Bitcoin source is added to a block summary.
pub(super) fn source_filter_allows(source_filter: &[String], code: &str) -> bool {
    source_filter.is_empty() || source_filter.iter().any(|source| source == code)
}

/// Fold a set of source records into a `SourceSummary`: dedupe by source id,
/// sort codes, count distinct sources, count distinct auxpow child chains, and
/// detect a live-chaintip observation. `pow_validates_btc_target` is passed
/// through by the caller. Sorted/BTree-backed so the serialized summary is
/// deterministic (pinned by tree + block fixtures).
pub(super) fn source_summary_from_sources<'a>(
    sources: impl IntoIterator<Item = &'a SourceRecord>,
    pow_validates_btc_target: bool,
) -> SourceSummary {
    let mut by_id: BTreeMap<i64, &SourceRecord> = BTreeMap::new();
    for source in sources {
        by_id.entry(source.id).or_insert(source);
    }
    let mut codes = by_id
        .values()
        .map(|source| source.code.clone())
        .collect::<Vec<_>>();
    codes.sort();
    let auxpow_chains = by_id
        .values()
        .filter(|source| source.kind == "auxpow")
        .filter_map(|source| source.chain.as_deref())
        .collect::<BTreeSet<_>>();
    let live_observed = by_id.values().any(|source| source.kind == "live-chaintip");
    SourceSummary {
        sources: codes,
        distinct_sources: by_id.len(),
        auxpow_chain_count: auxpow_chains.len(),
        live_observed,
        pow_validates_btc_target,
    }
}
