//! The single definition of what a stale-vs-canonical competition IS.
//!
//! Three projections read competitions (tree decoration, block detail, and
//! the `/api/v1/competitions` endpoint), and every one of them needs the same
//! join, the same kind predicates, and the same overflow-guarded header-time
//! delta. Keeping those here means a change to the definition (an added
//! predicate, a different guard) lands in one place rather than being
//! copy-edited across three query strings.

use tokio_postgres::Row;

use super::{PoolObject, pool_from_columns};

/// `canonical.btc_header_time - stale.btc_header_time`, guarded so a
/// difference outside i32 yields SQL NULL rather than a wrapped number.
///
/// The operands are cast to `numeric` before subtracting. A BIGINT subtraction
/// would be evaluated *before* the BETWEEN guard could reject it, so two
/// timestamps near opposite i64 extrema raise `bigint out of range` and fail
/// the whole query rather than yielding NULL for that row. Nothing in the
/// schema bounds `btc_header_time`: the values are only u32-shaped because
/// every write path widens a rust-bitcoin `Header.time`. `numeric` is
/// unbounded, so the guard gets to do its job for any stored value.
///
/// Consumers must treat NULL as "unavailable", never as zero.
pub(crate) const COMPETITION_DELTA_SQL: &str = "CASE WHEN canonical.btc_header_time::numeric - stale.btc_header_time::numeric \
                BETWEEN -2147483648 AND 2147483647 \
          THEN (canonical.btc_header_time::numeric - stale.btc_header_time::numeric)::int \
          ELSE NULL END";

/// The stale-to-canonical join with both miner pools, terminated by the
/// predicates that make the pair a competition. Callers append their own
/// `AND <predicate>` and/or `ORDER BY`.
///
/// Both predicates are load-bearing, not decorative. `canonical_competitor_hash`
/// is only a foreign key to `block` with a not-self check, so the schema permits
/// it to point at a non-canonical row, or at a canonical row from a different
/// height. Only the classifier's same-height rule keeps that from happening, and
/// a competition across two heights is not a competition: its delta and pool
/// pair would describe two blocks that never raced, and `canonical_hash` would
/// not be recoverable from the reported `btc_height` the way
/// `/api/v1/competitions` documents.
pub(crate) const COMPETITION_FROM_SQL: &str = "FROM block stale \
     JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
     LEFT JOIN pool sp ON sp.id = stale.bitcoin_miner_pool_id \
     LEFT JOIN pool cp ON cp.id = canonical.bitcoin_miner_pool_id \
     WHERE stale.kind = 'stale' \
       AND canonical.kind = 'canonical' \
       AND canonical.btc_height = stale.btc_height";

/// Column list for the two hash-keyed competition queries (tree decoration and
/// block detail), whose projections are identical. Columns 0..=8 are what
/// [`CompetitionCore::from_row`] reads.
pub(crate) fn competition_core_select() -> String {
    format!(
        "SELECT stale.btc_header_hash, canonical.btc_header_hash, \
                {COMPETITION_DELTA_SQL} AS header_time_delta_s, \
                sp.id, sp.slug, sp.canonical_name, \
                cp.id, cp.slug, cp.canonical_name"
    )
}

/// One competition as the hash-keyed callers read it: both sides' hashes in
/// storage byte order, the guarded delta, and both miner pools.
#[derive(Debug, Clone)]
pub(crate) struct CompetitionCore {
    pub(crate) stale_hash: Vec<u8>,
    pub(crate) canonical_hash: Vec<u8>,
    pub(crate) header_time_delta_s: Option<i32>,
    pub(crate) stale_bitcoin_miner_pool: PoolObject,
    pub(crate) canonical_bitcoin_miner_pool: PoolObject,
}

impl CompetitionCore {
    /// Map a row selected by [`competition_core_select`]. Column order is the
    /// contract between the two functions; they change together.
    pub(crate) fn from_row(row: &Row) -> Self {
        Self {
            stale_hash: row.get(0),
            canonical_hash: row.get(1),
            header_time_delta_s: row.get(2),
            stale_bitcoin_miner_pool: pool_from_columns(row.get(3), row.get(4), row.get(5)),
            canonical_bitcoin_miner_pool: pool_from_columns(row.get(6), row.get(7), row.get(8)),
        }
    }
}
