//! Shared row types, loaders, mappers, and cross-endpoint DTOs (`fixtures/api/tree.json`, `block-*.json`).

use std::collections::HashMap;

use std::str::FromStr;

use anyhow::{Context, Result, bail};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use serde::Serialize;
use serde_json::json;
use tokio_postgres::Client;

use crate::normalize::{Classification, ParentKind};

mod backbone;
pub(crate) use backbone::{
    BackboneWindowCoverage, ensure_backbone_window_coverage,
    load_backbone_window_coverage_for_windows, load_max_complete_canonical_height,
};

mod competition;
pub(crate) use competition::{
    COMPETITION_DELTA_SQL, COMPETITION_FROM_SQL, CompetitionCore, competition_core_select,
};

mod protocol_commitment;
pub(crate) use protocol_commitment::{
    PROTOCOL_COMMITMENT_JOIN_SQL, PROTOCOL_COMMITMENTS_READY_SQL,
};

/// Split an orphan-class filter into `(db_values, include_pending)` for parameterized
/// `b.btc_orphan_class = ANY($values::text[]) OR ($include_pending::boolean AND b.btc_orphan_class IS NULL)` predicates.
pub(super) fn classification_filter_params(
    classification: &[Classification],
) -> (Vec<String>, bool) {
    let mut values = Vec::new();
    let mut include_pending = false;
    for class in classification {
        match class.as_db_str() {
            Some(db) => values.push(db.to_owned()),
            None => include_pending = true,
        }
    }
    (values, include_pending)
}

pub(super) fn split_height_windows(windows: &[(i32, i32)]) -> (Vec<i32>, Vec<i32>) {
    let from_heights = windows
        .iter()
        .map(|(from_height, _)| *from_height)
        .collect::<Vec<_>>();
    let to_heights = windows
        .iter()
        .map(|(_, to_height)| *to_height)
        .collect::<Vec<_>>();
    (from_heights, to_heights)
}

pub(super) fn admit_observed_bitcoin_source<T>(
    records: &mut Vec<T>,
    source: Option<T>,
    observed: bool,
    same_source: impl Fn(&T, &T) -> bool,
) -> bool {
    if observed
        && let Some(source) = source
        && !records.iter().any(|record| same_source(record, &source))
    {
        records.push(source);
        return true;
    }
    false
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolObject {
    pub id: Option<i64>,
    pub slug: Option<String>,
    pub name: String,
    pub known: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SourceSummary {
    pub sources: Vec<String>,
    pub distinct_sources: usize,
    pub auxpow_chain_count: usize,
    pub live_observed: bool,
    pub pow_validates_btc_target: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProofState {
    pub has_live_observation: bool,
    pub has_auxpow_evidence: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChildChainEvidence {
    pub source: String,
    pub child_chain: Option<String>,
    pub event_count: usize,
    pub child_height_min: Option<i32>,
    pub child_height_max: Option<i32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TreeCompetition {
    pub btc_height: i32,
    pub stale_hash: String,
    pub canonical_hash: String,
    pub stale_bitcoin_miner_pool: PoolObject,
    pub canonical_bitcoin_miner_pool: PoolObject,
    pub header_time_delta_s: Option<i32>,
    pub propagation_delta_s: Option<i32>,
}

#[derive(Debug, Clone)]
pub(super) struct SourceRecord {
    pub(super) id: i64,
    pub(super) code: String,
    pub(super) kind: String,
    pub(super) chain: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct EventRow {
    pub(super) id: i64,
    pub(super) source: SourceRecord,
    pub(super) child_height: Option<i32>,
    pub(super) parent_hash: Vec<u8>,
    pub(super) prev_hash: Vec<u8>,
    pub(super) header_time: i64,
    pub(super) kind: ParentKind,
    pub(super) pow_validates_btc_target: bool,
    pub(super) child_miner_pool: PoolObject,
}

#[derive(Debug, Clone)]
pub(super) struct BlockRow {
    pub(super) hash: Vec<u8>,
    pub(super) prev_hash: Vec<u8>,
    pub(super) height: Option<i32>,
    pub(super) kind: ParentKind,
    pub(super) btc_orphan_class: Option<String>,
    pub(super) header_time: i64,
    pub(super) bitcoin_miner_pool: PoolObject,
    pub(super) live_observed: bool,
    pub(super) core_attested: bool,
    pub(super) pow_validated: bool,
}

#[derive(Debug, Clone)]
pub(super) struct SourceOnlyRow {
    pub(super) parent_hash: Vec<u8>,
    pub(super) source: SourceRecord,
}

pub(super) async fn load_sources(client: &Client) -> Result<HashMap<String, SourceRecord>> {
    let rows = client
        .query("SELECT id, code, kind, chain FROM source", &[])
        .await
        .context("load source registry")?;
    Ok(rows
        .into_iter()
        .map(|row| {
            let source = SourceRecord {
                id: row.get(0),
                code: row.get(1),
                kind: row.get(2),
                chain: row.get(3),
            };
            (source.code.clone(), source)
        })
        .collect())
}

pub(super) fn map_event_rows(rows: Vec<tokio_postgres::Row>) -> Result<Vec<EventRow>> {
    rows.into_iter()
        .map(|row| {
            let source = SourceRecord {
                id: row.get(1),
                code: row.get(2),
                kind: row.get(3),
                chain: row.get(4),
            };
            let kind: String = row.get(9);
            Ok(EventRow {
                id: row.get(0),
                source,
                child_height: row.get(5),
                parent_hash: row.get(6),
                prev_hash: row.get(7),
                header_time: row.get(8),
                kind: parent_kind_from_db(&kind)?,
                pow_validates_btc_target: row.get(10),
                child_miner_pool: pool_from_columns(row.get(11), row.get(12), row.get(13)),
            })
        })
        .collect()
}

pub(super) fn rows_to_blocks(rows: Vec<tokio_postgres::Row>) -> Result<Vec<BlockRow>> {
    rows.into_iter()
        .map(|row| {
            let kind: String = row.get(3);
            Ok(BlockRow {
                hash: row.get(0),
                prev_hash: row.get(1),
                height: row.get(2),
                kind: parent_kind_from_db(&kind)?,
                btc_orphan_class: row.get(11),
                header_time: row.get(4),
                bitcoin_miner_pool: pool_from_columns(row.get(8), row.get(9), row.get(10)),
                live_observed: row.get(5),
                core_attested: row.get(6),
                pow_validated: row.get(7),
            })
        })
        .collect()
}

pub(super) async fn load_active_proofs_for_hashes(
    client: &Client,
    source_filter: &[String],
    block_hashes: &[Vec<u8>],
) -> Result<Vec<SourceOnlyRow>> {
    let all_sources = source_filter.is_empty();
    let rows = client
        .query(
            "SELECT ap.btc_header_hash, ap.source_id, s.code, s.kind, s.chain, s.instance \
             FROM attestation_proof ap \
             JOIN source s ON s.id = ap.source_id \
             WHERE ap.revoked_at IS NULL \
               AND ($1::boolean OR s.code = ANY($2::text[])) \
               AND ap.btc_header_hash = ANY($3::bytea[])",
            &[&all_sources, &source_filter, &block_hashes],
        )
        .await
        .context("load active attestation proofs")?;
    Ok(source_only_rows(rows))
}

fn source_only_rows(rows: Vec<tokio_postgres::Row>) -> Vec<SourceOnlyRow> {
    rows.into_iter()
        .map(|row| SourceOnlyRow {
            parent_hash: row.get(0),
            source: SourceRecord {
                id: row.get(1),
                code: row.get(2),
                kind: row.get(3),
                chain: row.get(4),
            },
        })
        .collect()
}

pub(super) fn pool_from_columns(
    id: Option<i64>,
    slug: Option<String>,
    name: Option<String>,
) -> PoolObject {
    id.map_or_else(unknown_pool, |id| PoolObject {
        id: Some(id),
        slug,
        name: name.unwrap_or_else(|| "Unknown".to_owned()),
        known: true,
    })
}

pub(super) fn unknown_pool() -> PoolObject {
    PoolObject {
        id: None,
        slug: None,
        name: "Unknown".to_owned(),
        known: false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DisplayMinerBasis {
    BitcoinCoinbase,
    ChildInferred,
    Unknown,
}

impl DisplayMinerBasis {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::BitcoinCoinbase => "bitcoin_coinbase",
            Self::ChildInferred => "child_inferred",
            Self::Unknown => "unknown",
        }
    }
}

/// Resolve display miner: `bitcoin_miner_pool` wins when known; otherwise infer from a single
/// distinct known child pool across `child_pools` (collapsing to unknown if zero or conflicting).
pub(super) fn resolve_display_miner<'a>(
    bitcoin_miner_pool: &PoolObject,
    child_pools: impl IntoIterator<Item = &'a PoolObject>,
) -> (PoolObject, DisplayMinerBasis) {
    if bitcoin_miner_pool.known {
        return (
            bitcoin_miner_pool.clone(),
            DisplayMinerBasis::BitcoinCoinbase,
        );
    }
    let mut chosen: Option<&PoolObject> = None;
    for pool in child_pools {
        if !pool.known {
            continue;
        }
        match chosen {
            None => chosen = Some(pool),
            Some(seen) if seen.id == pool.id => {}
            Some(_) => return (unknown_pool(), DisplayMinerBasis::Unknown),
        }
    }
    match chosen {
        Some(pool) => (pool.clone(), DisplayMinerBasis::ChildInferred),
        None => (unknown_pool(), DisplayMinerBasis::Unknown),
    }
}

pub(super) fn group_events(events: &[EventRow]) -> HashMap<Vec<u8>, Vec<EventRow>> {
    let mut grouped: HashMap<Vec<u8>, Vec<EventRow>> = HashMap::new();
    for event in events {
        grouped
            .entry(event.parent_hash.clone())
            .or_default()
            .push(event.clone());
    }
    grouped
}

pub(super) fn group_sources(rows: &[SourceOnlyRow]) -> HashMap<Vec<u8>, Vec<SourceOnlyRow>> {
    let mut grouped: HashMap<Vec<u8>, Vec<SourceOnlyRow>> = HashMap::new();
    for row in rows {
        grouped
            .entry(row.parent_hash.clone())
            .or_default()
            .push(row.clone());
    }
    grouped
}

pub(super) fn parent_kind_from_db(raw: &str) -> Result<ParentKind> {
    match raw {
        "canonical" => Ok(ParentKind::Canonical),
        "invalid_block" => Ok(ParentKind::InvalidBlock),
        "stale" => Ok(ParentKind::Stale),
        "unknown" => Ok(ParentKind::Unknown),
        _ => bail!("unknown parent kind {raw:?}"),
    }
}

pub(super) fn display_hash(bytes: &[u8]) -> Result<String> {
    Ok(BlockHash::from_slice(bytes)
        .context("hash bytea is not 32 bytes")?
        .to_string())
}

/// RSK child block hashes are stored and served in forward byte order; all other chains reverse via [`display_hash`].
pub(super) fn display_child_block_hash(source_code: &str, bytes: &[u8]) -> Result<String> {
    if source_code == mmm_capture::source_registry::RSK_SOURCE_CODE {
        Ok(hex::encode(bytes))
    } else {
        display_hash(bytes)
    }
}

pub(super) fn stored_hash_from_display(hash: &str) -> Result<Vec<u8>> {
    Ok(BlockHash::from_str(hash)
        .context("parse normalized block hash")?
        .to_byte_array()
        .to_vec())
}

pub(super) fn projection_invariant_error(details: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "API projection invariant failed: {}",
        json!({ "details": details })
    )
}

pub(super) fn ensure_unknown_btc_target(
    kind: ParentKind,
    pow_validates_btc_target: impl IntoIterator<Item = bool>,
    details: &str,
) -> Result<()> {
    if kind == ParentKind::Unknown && pow_validates_btc_target.into_iter().any(|valid| !valid) {
        bail!(projection_invariant_error(details));
    }
    Ok(())
}

/// Declared read-only copy of `mmm-read-model`'s `load_strict_bip34_height` SQL shell
/// (shared validator in `mmm_capture::btc_orphan`) so `mmm-api` never depends on `mmm-read-model`.
pub(crate) async fn load_strict_bip34_height<C: tokio_postgres::GenericClient>(
    client: &C,
    hash: &[u8],
) -> anyhow::Result<Option<i32>> {
    use anyhow::Context as _;
    use mmm_capture::btc_orphan::{STRICT_BIP34_CHAINS, strict_bip34_height_from_evidence};

    let strict_chains: &[&str] = STRICT_BIP34_CHAINS;
    let rows = client
        .query(
            "SELECT s.chain, e.btc_parent_coinbase_script, e.btc_parent_coinbase_tx_bytes \
             FROM merge_mining_event e \
             JOIN source s ON s.id = e.source_id \
             WHERE e.btc_parent_header_hash = $1 \
               AND e.revoked_at IS NULL \
               AND e.btc_parent_coinbase_script IS NOT NULL \
               AND s.chain = ANY($2) \
             ORDER BY e.id",
            &[&hash, &strict_chains],
        )
        .await
        .context("load strict BIP34 coinbase candidates")?;
    for row in rows {
        let chain: String = row.get(0);
        let script: Vec<u8> = row.get(1);
        let tx_bytes: Option<Vec<u8>> = row.get(2);
        if let Some(height) =
            strict_bip34_height_from_evidence(&chain, &script, tx_bytes.as_deref())
        {
            return Ok(Some(height));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod display_miner_tests {
    use super::{DisplayMinerBasis, PoolObject, resolve_display_miner, unknown_pool};

    fn pool(id: i64) -> PoolObject {
        PoolObject {
            id: Some(id),
            slug: Some(format!("p{id}")),
            name: format!("P{id}"),
            known: true,
        }
    }

    #[test]
    fn strict_coinbase_pool_always_wins_over_child_evidence() {
        let strict = pool(7);
        let (a, b) = (pool(8), pool(9));
        let (display, basis) = resolve_display_miner(&strict, [&a, &b]);
        assert_eq!(display.id, Some(7));
        assert_eq!(basis, DisplayMinerBasis::BitcoinCoinbase);
    }

    #[test]
    fn single_distinct_known_child_pool_is_inferred() {
        let strict = unknown_pool();
        let (a, b) = (pool(8), pool(8));
        let (display, basis) = resolve_display_miner(&strict, [&a, &b]);
        assert_eq!(display.id, Some(8));
        assert_eq!(basis, DisplayMinerBasis::ChildInferred);
    }

    #[test]
    fn conflicting_known_child_pools_collapse_to_unknown() {
        let strict = unknown_pool();
        let (a, b) = (pool(8), pool(9));
        let (display, basis) = resolve_display_miner(&strict, [&a, &b]);
        assert!(!display.known);
        assert_eq!(basis, DisplayMinerBasis::Unknown);
    }

    #[test]
    fn unknown_child_pools_are_ignored_and_stay_unknown() {
        let strict = unknown_pool();
        let (a, b) = (unknown_pool(), unknown_pool());
        let (display, basis) = resolve_display_miner(&strict, [&a, &b]);
        assert!(!display.known);
        assert_eq!(basis, DisplayMinerBasis::Unknown);
    }
}
