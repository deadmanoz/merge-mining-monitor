//! Shared row types, loaders, mappers, and cross-endpoint DTOs for the
//! projection submodules.

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

/// Split an orphan-class filter into the concrete `block.btc_orphan_class` db
/// values and whether `pending` (the SQL `NULL` transient) is included. Every
/// orphan-class-filtered query binds these two params and splices the STATIC
/// fragment
/// `b.btc_orphan_class = ANY($values::text[]) OR ($include_pending::boolean AND b.btc_orphan_class IS NULL)`,
/// so no dynamic SQL string-building is needed. An empty `values` array matches
/// nothing via `= ANY`, so a pending-only filter selects exactly the NULL rows.
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

/// Wire-DTO: a Bitcoin miner pool reference. `known=false` with null id/slug
/// and name "Unknown" is the unresolved sentinel (see unknown_pool). Serde
/// field names are the locked contract (pinned by tree.json and block-*.json).
/// PartialEq/Eq back tree-builder dedup.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PoolObject {
    pub id: Option<i64>,
    pub slug: Option<String>,
    pub name: String,
    pub known: bool,
}

/// Wire-DTO: per-parent source-attestation summary (sorted source codes,
/// distinct-source count, distinct auxpow child-chain count, live-observed
/// flag, pow-validates-Bitcoin-target flag). Built by
/// materialize::source_summary_from_sources. Serde field names are the locked
/// contract (pinned by tree.json + block-*.json).
#[derive(Debug, Clone, Serialize)]
pub struct SourceSummary {
    pub sources: Vec<String>,
    pub distinct_sources: usize,
    pub auxpow_chain_count: usize,
    pub live_observed: bool,
    pub pow_validates_btc_target: bool,
}

/// Wire-DTO: coarse evidence presence flags for a parent (live observation and
/// AuxPoW evidence). Serde field names pinned by tree.json + block-*.json.
#[derive(Debug, Clone, Serialize)]
pub struct ProofState {
    pub has_live_observation: bool,
    pub has_auxpow_evidence: bool,
}

/// Wire-DTO: per-(source, child_chain) evidence rollup for a parent
/// (event_count, min/max child height). Built by
/// materialize::child_chain_evidence_from_events; array order is deterministic
/// (BTreeMap-grouped). Serde field names pinned by tree.json + block-*.json.
#[derive(Debug, Clone, Serialize)]
pub struct ChildChainEvidence {
    pub source: String,
    pub child_chain: Option<String>,
    pub event_count: usize,
    pub child_height_min: Option<i32>,
    pub child_height_max: Option<i32>,
}

/// Wire-DTO: the stale-vs-canonical competition attached to a stale node
/// (heights, both hashes, both pool objects, and the timing deltas). Built by
/// tree::build and block::loaders. Serde field names pinned by tree.json +
/// block-*.json; hashes are explorer-hex (display_hash).
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

/// Internal `source`-table row (id, code, kind, chain). NOT serialized; the
/// projection derives wire shapes (SourceSummary, ChildChainEvidence) from
/// sets of these. `kind` drives auxpow/live-chaintip classification in the
/// summary fold.
#[derive(Debug, Clone)]
pub(super) struct SourceRecord {
    pub(super) id: i64,
    pub(super) code: String,
    pub(super) kind: String,
    pub(super) chain: Option<String>,
}

/// Internal merge_mining_event projection row (id, source, child_height,
/// parent/prev hash bytes in wire order, header_time, derived ParentKind,
/// pow-validates-Bitcoin-target, resolved child miner pool). NOT serialized.
/// parent_hash/prev_hash are stored wire/internal byte order; never reverse
/// here. `child_miner_pool` is the store-maintained per-event
/// merge_mining_event.child_miner_pool_id (the namespace-agnostic "one distinct
/// known child pool else NULL" collapse), used by the display-miner fallback.
#[derive(Debug, Clone)]
pub(super) struct EventRow {
    pub(super) id: i64,
    pub(super) source: SourceRecord,
    pub(super) child_height: i32,
    pub(super) parent_hash: Vec<u8>,
    pub(super) prev_hash: Vec<u8>,
    pub(super) header_time: i64,
    pub(super) kind: ParentKind,
    pub(super) pow_validates_btc_target: bool,
    pub(super) child_miner_pool: PoolObject,
}

/// Internal `block`-table projection row (hash/prev_hash wire bytes, height,
/// kind, btc_orphan_class, header_time, resolved pool, plus the
/// live_observed/core_attested/pow_validated evidence flags). NOT serialized.
/// The three evidence flags gate Bitcoin-source admission and the
/// pow-validates summary flag.
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

/// Internal (parent_hash, source) pair for proof joins where only the attesting
/// source matters (no per-event fields). Loaded by
/// load_active_proofs_for_hashes. NOT serialized.
#[derive(Debug, Clone)]
pub(super) struct SourceOnlyRow {
    pub(super) parent_hash: Vec<u8>,
    pub(super) source: SourceRecord,
}

/// Load the full `source` registry keyed by code. Every projection that needs
/// to admit the synthetic Bitcoin source or classify auxpow/live-chaintip
/// kinds calls this once per request.
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

/// Map the canonical event-query column layout (id, source quad, child_height,
/// parent/prev hash, header_time, kind, pow_validates, child miner pool trio)
/// into EventRow, resolving the kind string via parent_kind_from_db. The
/// positional column order is the contract shared with the event SELECTs.
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

/// Map the canonical block-query column layout into BlockRow (hash, prev_hash,
/// height, kind, evidence flags, pool columns, btc_orphan_class). The
/// positional column order is the contract every block SELECT in tree/* must
/// match.
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

/// Load non-revoked attestation proofs (source-only) for a set of parent
/// hashes, honoring an empty-means-all source filter. revoked_at IS NULL is
/// the active-evidence predicate; revoked rows never project.
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

/// Build a PoolObject from (id, slug, name) columns: a NULL id yields the
/// unknown_pool sentinel (known=false); a present id yields known=true with
/// name defaulting to "Unknown" if NULL. The single mapping from pool join
/// columns to the wire DTO.
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

/// The unresolved-pool sentinel PoolObject (null id/slug, name "Unknown",
/// known=false). The shared implementation of the wire-locked unknown-pool object;
/// pool_from_columns and direct-event projections both route through it.
pub(super) fn unknown_pool() -> PoolObject {
    PoolObject {
        id: None,
        slug: None,
        name: "Unknown".to_owned(),
        known: false,
    }
}

/// How a parent's `display_miner_pool` was resolved. The wire value is the
/// snake_case `as_str`; it is intentionally chain-agnostic (`child_inferred` never
/// names a specific child chain, so the contract does not grow an arm per
/// chain). The strict `bitcoin_miner_pool` is never mutated by this layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DisplayMinerBasis {
    /// Strict Bitcoin coinbase attribution (`bitcoin_miner_pool` is known).
    BitcoinCoinbase,
    /// Inferred from a single distinct known child-chain miner pool across the
    /// parent's active merge-mining events (the merge-miner of the child block
    /// is the miner of the Bitcoin parent).
    ChildInferred,
    /// No strict coinbase pool and no unambiguous child inference (zero or
    /// conflicting known child pools).
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

/// Resolve the best-available display miner for a Bitcoin parent without
/// touching the strict `bitcoin_miner_pool` fact. The strict Bitcoin-coinbase
/// pool always wins when known. Otherwise fall back to the single distinct
/// known pool across the parent's active child events' `child_miner_pool`
/// (the store's namespace-agnostic per-event collapse), which is the
/// merge-mining inference; zero or more than one distinct known pool collapses
/// to unknown (mirroring `refresh_event_child_miner_pool_id`, no majority
/// vote). Pure: a revoked event simply never reaches `child_pools`, so the
/// label drops on revoke with no extra machinery. The single derivation shared
/// by the tree and block-detail projections (rule of two), so a node label and
/// its drawer can never disagree.
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
            // A second distinct known child pool: ambiguous, collapse to unknown.
            Some(_) => return (unknown_pool(), DisplayMinerBasis::Unknown),
        }
    }
    match chosen {
        Some(pool) => (pool.clone(), DisplayMinerBasis::ChildInferred),
        None => (unknown_pool(), DisplayMinerBasis::Unknown),
    }
}

/// Group events by parent_hash (cloning into owned EventRows) for per-parent
/// assembly in project_blocks. HashMap-keyed; callers that need deterministic
/// order sort downstream.
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

/// Group source-only proof rows by parent_hash for per-parent assembly.
/// HashMap-keyed; the proof analogue of group_events.
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

/// Parse a `btc_parent_kind` / `block.kind` db enum string into ParentKind,
/// erroring on an unrecognized value (never silently coerced). API-local
/// twin of mmm-read-model's parser; the api does not depend on the writer
/// crate (constraint #1), and this maps four fixed db literals.
pub(super) fn parent_kind_from_db(raw: &str) -> Result<ParentKind> {
    match raw {
        "canonical" => Ok(ParentKind::Canonical),
        "near" => Ok(ParentKind::Near),
        "stale" => Ok(ParentKind::Stale),
        "unknown" => Ok(ParentKind::Unknown),
        _ => bail!("unknown parent kind {raw:?}"),
    }
}

/// Render 32 stored hash bytes (wire/internal order) as explorer-hex,
/// reversing via the BlockHash newtype's Display. The single forward-order
/// reversal point; never reverse bytes by hand. Errors if the bytea is not 32
/// bytes.
pub(super) fn display_hash(bytes: &[u8]) -> Result<String> {
    Ok(BlockHash::from_slice(bytes)
        .context("hash bytea is not 32 bytes")?
        .to_string())
}

/// Render a child block hash for display, honoring chain byte order. RSK child
/// block hashes are stored and served in forward (hex) byte order, so they are
/// hex-encoded as-is; every other chain's hash is internal/wire order and is
/// reversed by `display_hash` for explorer/RPC hex. The byte-order invariant is
/// locked, so this is the single place the RSK special-case lives.
pub(super) fn display_child_block_hash(source_code: &str, bytes: &[u8]) -> Result<String> {
    if source_code == mmm_capture::source_registry::RSK_SOURCE_CODE {
        Ok(hex::encode(bytes))
    } else {
        display_hash(bytes)
    }
}

/// Parse a normalized explorer-hex block hash back to stored wire/internal
/// bytes (the inverse of display_hash) for hash-keyed lookups and keyset
/// cursors. Uses the BlockHash newtype's to_byte_array, never a manual
/// reverse.
pub(super) fn stored_hash_from_display(hash: &str) -> Result<Vec<u8>> {
    Ok(BlockHash::from_str(hash)
        .context("parse normalized block hash")?
        .to_byte_array()
        .to_vec())
}

/// Construct the canonical 'API projection invariant failed' anyhow::Error
/// (wraps details as JSON). Used at every projection invariant bail!, so the
/// 500-path message format is uniform across tree/block/build/detail.
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

/// Strict-BIP34 height lookup over base evidence, API-local.
///
/// DECLARED DUPLICATION of mmm-read-model's `load_strict_bip34_height` SQL
/// shell (the pure parser and constants are shared via mmm-capture): the api
/// must not depend on the writer crate, and this read-only SELECT over
/// merge_mining_event is squarely projection-query territory. Preserves the
/// BIP34 activation-floor guard - a decoded height below activation is never
/// usable strict evidence.
pub(crate) async fn load_strict_bip34_height<C: tokio_postgres::GenericClient>(
    client: &C,
    hash: &[u8],
) -> anyhow::Result<Option<i32>> {
    use anyhow::Context as _;
    use mmm_capture::auxpow::parse_bip34_height;
    use mmm_capture::btc_orphan::{BIP34_HEIGHT, STRICT_BIP34_CHAINS};

    let strict_chains: &[&str] = STRICT_BIP34_CHAINS;
    let rows = client
        .query(
            "SELECT e.btc_parent_coinbase_script \
             FROM merge_mining_event e \
             JOIN source s ON s.id = e.source_id \
             WHERE e.btc_parent_header_hash = $1 \
               AND e.revoked_at IS NULL \
               AND e.btc_parent_kind <> 'near' \
               AND e.btc_parent_coinbase_script IS NOT NULL \
               AND s.chain = ANY($2) \
             ORDER BY e.id",
            &[&hash, &strict_chains],
        )
        .await
        .context("load strict BIP34 coinbase candidates")?;
    for row in rows {
        let script: Vec<u8> = row.get(0);
        if let Some(height) = parse_bip34_height(&script)
            && height >= BIP34_HEIGHT
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
