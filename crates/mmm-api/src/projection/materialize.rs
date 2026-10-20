//! Shared parent-projection pipeline for tree and block projections.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use anyhow::Result;

use super::shared::{
    BlockRow, ChildChainEvidence, DisplayMinerBasis, EventRow, PoolObject, ProofState,
    SourceOnlyRow, SourceRecord, SourceSummary, TreeCompetition, admit_observed_bitcoin_source,
    display_hash, ensure_unknown_btc_target, group_events, group_sources, resolve_display_miner,
    unknown_pool,
};
use crate::normalize::ParentKind;
use mmm_capture::source_registry::BITCOIN_SOURCE_CODE;

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
    pub(super) display_miner_pool: PoolObject,
    pub(super) display_miner_basis: &'static str,
    pub(super) source_summary: SourceSummary,
    pub(super) child_chain_evidence: Vec<ChildChainEvidence>,
    pub(super) proof_state: ProofState,
    pub(super) evidence: bool,
    pub(super) protected: bool,
    pub(super) competition: Option<TreeCompetition>,
}

/// Project block-backed parents into `ParentProjection`s, enforcing that active events for an
/// `unknown` block validate the Bitcoin target.
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

/// Project `Unknown` parents that have AuxPoW events but no `block` row (`btc_orphan_class = None`).
pub(super) fn project_direct_events(
    events: &[EventRow],
    block_hashes: &HashSet<Vec<u8>>,
) -> Result<Vec<ParentProjection>> {
    let mut grouped: BTreeMap<Vec<u8>, Vec<&EventRow>> = BTreeMap::new();
    for event in events {
        if block_hashes.contains(&event.parent_hash) || event.kind != ParentKind::Unknown {
            continue;
        }
        grouped
            .entry(event.parent_hash.clone())
            .or_default()
            .push(event);
    }

    let mut projections = Vec::new();
    for (hash, events) in grouped {
        let kind = ParentKind::Unknown;
        ensure_unknown_btc_target(
            kind,
            events.iter().map(|event| event.pow_validates_btc_target),
            "active direct unknown event fails Bitcoin target",
        )?;
        let first = events
            .iter()
            .min_by_key(|event| event.id)
            .expect("grouped from at least one event");
        let source_summary =
            source_summary_from_sources(events.iter().map(|event| &event.source), true);
        projections.push(ParentProjection {
            hash_bytes: hash.clone(),
            hash: display_hash(&hash)?,
            prev_hash: display_hash(&first.prev_hash)?,
            height: None,
            kind,
            btc_orphan_class: None,
            header_time: first.header_time,
            bitcoin_miner_pool: unknown_pool(),
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
    let _ = admit_observed_bitcoin_source(
        &mut records,
        source_filter_allows(source_filter, BITCOIN_SOURCE_CODE)
            .then(|| sources.get(BITCOIN_SOURCE_CODE))
            .flatten()
            .cloned(),
        block.core_attested || block.live_observed,
        |existing, bitcoin| existing.id == bitcoin.id,
    );
    source_summary_from_sources(
        records.iter(),
        block.pow_validated || block.kind != ParentKind::Unknown,
    )
}

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
        if let Some(child_height) = event.child_height {
            entry.1 = Some(entry.1.map_or(child_height, |min| min.min(child_height)));
            entry.2 = Some(entry.2.map_or(child_height, |max| max.max(child_height)));
        }
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

pub(super) fn source_filter_allows(source_filter: &[String], code: &str) -> bool {
    source_filter.is_empty() || source_filter.iter().any(|source| source == code)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn event_without_child_height(id: i64) -> EventRow {
        EventRow {
            id,
            source: SourceRecord {
                id: 1,
                code: "auxpow:i0coin".to_owned(),
                kind: "auxpow".to_owned(),
                chain: Some("i0coin".to_owned()),
            },
            child_height: None,
            parent_hash: vec![0; 32],
            prev_hash: vec![0; 32],
            header_time: 1_600_000_000,
            kind: ParentKind::Canonical,
            pow_validates_btc_target: true,
            child_miner_pool: unknown_pool(),
        }
    }

    #[test]
    fn child_chain_evidence_keeps_source_when_every_height_is_unknown() {
        let events = [event_without_child_height(1), event_without_child_height(2)];

        let evidence = child_chain_evidence_from_events(events.iter());

        assert_eq!(evidence.len(), 1);
        assert_eq!(evidence[0].source, "auxpow:i0coin");
        assert_eq!(evidence[0].child_chain.as_deref(), Some("i0coin"));
        assert_eq!(evidence[0].event_count, 2);
        assert_eq!(evidence[0].child_height_min, None);
        assert_eq!(evidence[0].child_height_max, None);
    }
}
