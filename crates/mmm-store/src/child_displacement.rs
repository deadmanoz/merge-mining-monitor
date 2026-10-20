//! Child-side displacement: which block the child chain carries at a height.
//!
//! A child chain can replace the block it carries at a height. The event
//! captured for the replaced block is still valid Bitcoin-side evidence, so it
//! is never revoked for that reason; it is marked displaced instead, and every
//! Bitcoin-side aggregate keeps reading it. See `docs/data-model.md`, "Child
//! Displacement".

use anyhow::{Context, Result, ensure};
use tokio_postgres::types::ToSql;
use tokio_postgres::{Client, GenericClient, Transaction};

use mmm_capture::capture::ChildBlockObservation;

use crate::observation::upsert_child_only_observation;

/// Two-int advisory-lock class for serializing the per-height transition. The
/// two-int `pg_advisory_xact_lock(int4, int4)` space is disjoint from the
/// one-int space the per-block hash locks use, and the class keeps it apart
/// from the source-health class in the same space.
const CHILD_CHAIN_LOCK_CLASS: i32 = 0x4348; // 'CH' - child chain block at a height

/// Parent identity of the current child block, governing how hashless partial observations at the height are displaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurrentBlockParent<'a> {
    /// Verified proof naming this parent (internal byte order); matching hashless rows are current, others displaced.
    Known(&'a [u8]),
    /// Block carries no AuxPoW; all hashless rows at the height are displaced.
    NoAuxpow,
    /// Proof unverified; hashless rows are left untouched to avoid poisoning later promotion.
    Unknown,
}

/// Producer outcome stored on `child_chain_head` to decide whether a rescan can skip re-capturing the same block hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChildChainHeadOutcome {
    Captured,
    /// Proof verified and verdict settled, but yields no event: the parent failed the Bitcoin lineage gate.
    Recorded,
    NoAuxpow,
    /// Proof failed or verdict is provisional on the Bitcoin Core cache.
    Unverified,
    /// Cursor held at this block; re-captured on the next observation.
    Held,
}

impl ChildChainHeadOutcome {
    pub fn captured(classification_provisional: bool) -> Self {
        if classification_provisional {
            Self::Unverified
        } else {
            Self::Captured
        }
    }

    pub fn is_final(self) -> bool {
        matches!(self, Self::Captured | Self::Recorded | Self::NoAuxpow)
    }

    fn as_db_str(self) -> &'static str {
        match self {
            Self::Captured => "captured",
            Self::Recorded => "recorded",
            Self::NoAuxpow => "non_auxpow",
            Self::Unverified => "unverified",
            Self::Held => "held",
        }
    }

    fn from_db_str(value: &str) -> Result<Self> {
        Ok(match value {
            "captured" => Self::Captured,
            "recorded" => Self::Recorded,
            "non_auxpow" => Self::NoAuxpow,
            "unverified" => Self::Unverified,
            "held" => Self::Held,
            other => anyhow::bail!("unknown child_chain_head outcome {other:?}"),
        })
    }
}

/// Sidecar evidence fingerprint stored on `child_chain_head` so rescans detect out-of-band evidence changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceMarker {
    None,
    /// Digest of Hathor sidecar graph heads (first 45 bytes) at the height.
    HathorSidecars,
}

impl EvidenceMarker {
    async fn current<C: GenericClient>(
        self,
        client: &C,
        source_id: i64,
        child_height: i32,
    ) -> Result<Option<i64>> {
        match self {
            Self::None => Ok(None),
            Self::HathorSidecars => Ok(client
                .query_one(
                    "SELECT ('x' || left(md5(string_agg( \
                                h.id::text || ':' || \
                                md5(substr(h.funds_graph, h.funds_graph_split + 1, 45)), \
                                ',' ORDER BY h.id)), 16))::bit(64)::bigint \
                       FROM hathor_merge_mining_evidence h \
                       JOIN merge_mining_event e ON e.id = h.event_id \
                      WHERE e.source_id = $1 AND e.child_height = $2",
                    &[&source_id, &child_height],
                )
                .await
                .context("load the Hathor evidence marker")?
                .get(0)),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ChildChainHeadRecord<'a> {
    pub block_hash: &'a [u8],
    pub parent: CurrentBlockParent<'a>,
    pub outcome: ChildChainHeadOutcome,
    pub evidence: EvidenceMarker,
    pub observed_at: i64,
    /// Core-cache generation when decided in an earlier transaction (`None` uses the recording transaction's generation).
    pub core_cache_generation: Option<i64>,
}

/// Persisted `child_chain_head` row for a `(source_id, child_height)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildChainHead {
    pub block_hash: Vec<u8>,
    pub btc_parent_header_hash: Option<Vec<u8>>,
    pub outcome: ChildChainHeadOutcome,
    pub core_cache_changed: bool,
    pub evidence_changed: bool,
    pub observed_at: i64,
}

impl ChildChainHead {
    pub fn is_final(&self) -> bool {
        self.outcome.is_final() && !self.core_cache_changed
    }

    pub fn current_parent(&self) -> CurrentBlockParent<'_> {
        match (&self.btc_parent_header_hash, self.outcome) {
            (Some(parent), _) => CurrentBlockParent::Known(parent.as_slice()),
            (None, ChildChainHeadOutcome::NoAuxpow) => CurrentBlockParent::NoAuxpow,
            (None, _) => CurrentBlockParent::Unknown,
        }
    }
}

pub async fn load_child_chain_head<C: GenericClient>(
    client: &C,
    source_id: i64,
    child_height: i32,
    evidence: EvidenceMarker,
) -> Result<Option<ChildChainHead>> {
    let row = client
        .query_opt(
            "SELECT h.block_hash, h.btc_parent_header_hash, h.outcome, \
                    h.core_cache_generation < s.core_cache_generation, \
                    h.evidence_marker, h.observed_at \
               FROM child_chain_head h, bitcoin_core_header_cache_state s \
              WHERE h.source_id = $1 AND h.child_height = $2 AND s.singleton",
            &[&source_id, &child_height],
        )
        .await
        .context("load the child chain head")?;
    let Some(row) = row else {
        return Ok(None);
    };
    let recorded_marker: Option<i64> = row.get(4);
    let evidence_changed =
        recorded_marker != evidence.current(client, source_id, child_height).await?;
    Ok(Some(ChildChainHead {
        block_hash: row.get(0),
        btc_parent_header_hash: row.get(1),
        outcome: ChildChainHeadOutcome::from_db_str(row.get::<_, &str>(2))?,
        core_cache_changed: row.get(3),
        evidence_changed,
        observed_at: row.get(5),
    }))
}

impl CurrentBlockParent<'_> {
    fn parent_hash(&self) -> Option<&[u8]> {
        match self {
            CurrentBlockParent::Known(parent) => Some(parent),
            CurrentBlockParent::NoAuxpow | CurrentBlockParent::Unknown => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ChildDisplacementOutcome {
    /// Events whose displacement was cleared because their block is the
    /// chain's block at the height again.
    pub restored: u64,
    /// Events newly marked displaced by the current block.
    pub displaced: u64,
}

/// The advisory-lock key for one `(source_id, child_height)`: the class (`$1`) in key one, a
/// hash of the source (`$2`) and `height` (a placeholder or column) in key two.
fn child_chain_lock_key(height: &str) -> String {
    format!("$1, hashint8(($2::bigint << 32) | {height}::bigint)")
}

/// Hold the per-height lock at session level across a producer's observation
/// of the child chain and the writes that follow, so that what a producer
/// observed is what it records: two producers overlapping on one height (a
/// live poller and a bounded backfill) observe and write one after the other,
/// and the later observation describes the chain. The transaction-level lock
/// the capture transaction takes on the same key is re-entrant for the
/// session that holds this one. Pair every call with
/// [`finish_child_chain_height_operation`].
pub async fn lock_child_chain_height_session(
    client: &Client,
    source_id: i64,
    child_height: i32,
) -> Result<()> {
    client
        .execute(
            &format!("SELECT pg_advisory_lock({})", child_chain_lock_key("$3")),
            &[
                &CHILD_CHAIN_LOCK_CLASS,
                &source_id,
                &i64::from(child_height),
            ],
        )
        .await
        .context("lock the child chain height for the session")?;
    Ok(())
}

/// Release the session-level height lock and fold the unlock into the
/// operation's result: the operation's error wins when both fail.
pub async fn finish_child_chain_height_operation<T>(
    client: &Client,
    source_id: i64,
    child_height: i32,
    result: Result<T>,
) -> Result<T> {
    let unlock = client
        .execute(
            &format!("SELECT pg_advisory_unlock({})", child_chain_lock_key("$3")),
            &[
                &CHILD_CHAIN_LOCK_CLASS,
                &source_id,
                &i64::from(child_height),
            ],
        )
        .await
        .context("unlock the child chain height for the session");
    match (result, unlock) {
        (Ok(value), Ok(_)) => Ok(value),
        (Ok(_), Err(err)) => Err(err),
        (Err(err), _) => Err(err),
    }
}

/// Take the transaction-scoped advisory lock that serializes every change to
/// which block the child chain carries at `(source_id, child_height)`.
///
/// Producers take it before upserting a captured block's event, so that two
/// captures of different blocks at one height contend here first rather than
/// each holding its own event row while waiting for the other; the lock is
/// re-entrant within the transaction, so the later [`record_child_chain_block`]
/// call takes it again without blocking. Released at commit or rollback.
pub async fn lock_child_chain_height(
    txn: &Transaction<'_>,
    source_id: i64,
    child_height: i32,
) -> Result<()> {
    txn.execute(
        &format!(
            "SELECT pg_advisory_xact_lock({})",
            child_chain_lock_key("$3")
        ),
        &[
            &CHILD_CHAIN_LOCK_CLASS,
            &source_id,
            &i64::from(child_height),
        ],
    )
    .await
    .context("lock the child chain height")?;
    Ok(())
}

/// Record `record.block_hash` as the child chain's current block at `(source_id, child_height)`.
///
/// Under [`lock_child_chain_height`] (which callers that also upsert an event must acquire
/// *before* the event upsert to avoid row-lock vs height-lock deadlocks), upserts
/// `child_chain_head`, clears displacement on the matching event and observation, and marks any
/// other undisplaced events and observations at the height as displaced at `observed_at`
/// (preserving first-displacement timestamps on already-displaced rows and handling hashless rows
/// per [`CurrentBlockParent`]).
pub async fn record_child_chain_block(
    txn: &Transaction<'_>,
    source_id: i64,
    child_height: i32,
    record: ChildChainHeadRecord<'_>,
) -> Result<ChildDisplacementOutcome> {
    let ChildChainHeadRecord {
        block_hash: current_block_hash,
        parent: current_parent,
        outcome,
        evidence,
        observed_at,
        core_cache_generation,
    } = record;
    ensure!(
        current_block_hash.len() == 32,
        "child block hash must be 32 bytes, got {}",
        current_block_hash.len()
    );
    lock_child_chain_height(txn, source_id, child_height).await?;
    let parent_hash = current_parent.parent_hash();
    let evidence_marker = evidence.current(txn, source_id, child_height).await?;
    txn.execute(
        "INSERT INTO child_chain_head \
             (source_id, child_height, block_hash, btc_parent_header_hash, outcome, \
              core_cache_generation, evidence_marker, observed_at) \
         SELECT $1, $2, $3, $4, $5, COALESCE($8, s.core_cache_generation), $7, $6 \
           FROM bitcoin_core_header_cache_state s WHERE s.singleton \
         ON CONFLICT (source_id, child_height) DO UPDATE SET \
             block_hash = EXCLUDED.block_hash, \
             btc_parent_header_hash = EXCLUDED.btc_parent_header_hash, \
             outcome = EXCLUDED.outcome, \
             core_cache_generation = EXCLUDED.core_cache_generation, \
             evidence_marker = EXCLUDED.evidence_marker, \
             observed_at = EXCLUDED.observed_at",
        &[
            &source_id,
            &child_height,
            &current_block_hash,
            &parent_hash,
            &outcome.as_db_str(),
            &observed_at,
            &evidence_marker,
            &core_cache_generation,
        ],
    )
    .await
    .context("record the child chain head")?;
    apply_child_displacement(
        txn,
        source_id,
        child_height,
        current_block_hash,
        current_parent,
        observed_at,
    )
    .await
}

/// Delete up to `limit` of a source's `child_chain_head` rows below `floor` (the lowest
/// height a rescan still reaches), oldest first. Observations keep the per-block history;
/// the head is rescan state only. A height whose lock another session holds (a rescan
/// between reading and re-observing its head) is left for a later prune.
pub async fn prune_child_chain_heads_below(
    client: &Client,
    source_id: i64,
    floor: i32,
    limit: i64,
) -> Result<u64> {
    client
        .execute(
            &format!(
                "DELETE FROM child_chain_head \
                  WHERE source_id = $2 AND child_height IN ( \
                      SELECT child_height FROM ( \
                          SELECT child_height FROM child_chain_head \
                           WHERE source_id = $2 AND child_height < $3 \
                           ORDER BY child_height LIMIT $4) candidates \
                       WHERE pg_try_advisory_xact_lock({}))",
                child_chain_lock_key("child_height")
            ),
            &[&CHILD_CHAIN_LOCK_CLASS, &source_id, &floor, &limit],
        )
        .await
        .context("prune child_chain_head below the rescan floor")
}

/// Refresh `observed_at` and re-apply displacement for an unchanged `child_chain_head` block
/// without advancing its recorded `core_cache_generation`.
pub async fn reobserve_child_chain_block_in_own_transaction(
    client: &mut Client,
    source_id: i64,
    child_height: i32,
    current_block_hash: &[u8],
    current_parent: CurrentBlockParent<'_>,
    observed_at: i64,
) -> Result<ChildDisplacementOutcome> {
    ensure!(
        current_block_hash.len() == 32,
        "child block hash must be 32 bytes, got {}",
        current_block_hash.len()
    );
    let txn = client
        .transaction()
        .await
        .context("begin child block re-observation")?;
    lock_child_chain_height(&txn, source_id, child_height).await?;
    let updated = txn
        .execute(
            "UPDATE child_chain_head SET observed_at = $4 \
              WHERE source_id = $1 AND child_height = $2 AND block_hash = $3",
            &[&source_id, &child_height, &current_block_hash, &observed_at],
        )
        .await
        .context("refresh the child chain head observation")?;
    ensure!(
        updated == 1,
        "the child chain head at height {child_height} no longer records the re-observed block"
    );
    let outcome = apply_child_displacement(
        &txn,
        source_id,
        child_height,
        current_block_hash,
        current_parent,
        observed_at,
    )
    .await?;
    txn.commit()
        .await
        .context("commit child block re-observation")?;
    Ok(outcome)
}

async fn apply_child_displacement(
    txn: &Transaction<'_>,
    source_id: i64,
    child_height: i32,
    current_block_hash: &[u8],
    current_parent: CurrentBlockParent<'_>,
    observed_at: i64,
) -> Result<ChildDisplacementOutcome> {
    let parent_hash = current_parent.parent_hash();
    let hashless_decidable = !matches!(current_parent, CurrentBlockParent::Unknown);
    let params: [&(dyn ToSql + Sync); 6] = [
        &source_id,
        &child_height,
        &current_block_hash,
        &parent_hash,
        &hashless_decidable,
        &observed_at,
    ];
    let rows = txn
        .query(&displacement_sql("merge_mining_event"), &params)
        .await
        .context("record the child chain's current block")?;
    // Observations carry the same displacement, including blocks without an event.
    txn.execute(&displacement_sql("child_block_observation"), &params)
        .await
        .context("record the child chain's current block on its observations")?;
    let restored = rows.iter().filter(|row| row.get::<_, bool>(0)).count() as u64;
    Ok(ChildDisplacementOutcome {
        restored,
        displaced: rows.len() as u64 - restored,
    })
}

/// Set displacement on `table`'s rows at `(source_id, child_height)` against the chain's
/// current block: the current block (or its hashless row) is restored, every other
/// undisplaced row is displaced, and already-displaced rows keep their first record.
fn displacement_sql(table: &str) -> String {
    format!(
        "WITH candidate AS ( \
             SELECT id, \
                    (COALESCE(child_block_hash = $3, FALSE) \
                     OR (child_block_hash IS NULL \
                         AND $4::bytea IS NOT NULL \
                         AND btc_parent_header_hash = $4::bytea)) AS is_current \
             FROM {table} \
             WHERE source_id = $1 AND child_height = $2 \
               AND NOT (child_block_hash IS NULL AND NOT $5::boolean) \
         ) \
         UPDATE {table} t \
         SET child_displaced_at = CASE WHEN c.is_current THEN NULL ELSE $6::bigint END, \
             child_displaced_by = CASE WHEN c.is_current THEN NULL ELSE $3::bytea END \
         FROM candidate c \
         WHERE t.id = c.id \
           AND ( \
             (c.is_current AND t.child_displaced_at IS NOT NULL) \
             OR (NOT c.is_current AND t.child_displaced_at IS NULL) \
           ) \
         RETURNING t.child_displaced_at IS NULL AS restored"
    )
}

/// [`record_child_chain_block`] in its own transaction for blocks that yield no event,
/// with the block's child-only observation when it has one.
pub async fn record_child_chain_block_in_own_transaction(
    client: &mut Client,
    source_id: i64,
    child_height: i32,
    record: ChildChainHeadRecord<'_>,
    observation: Option<&ChildBlockObservation>,
) -> Result<ChildDisplacementOutcome> {
    let txn = client
        .transaction()
        .await
        .context("begin child block record")?;
    let outcome = record_child_chain_block(&txn, source_id, child_height, record).await?;
    if let Some(observation) = observation {
        upsert_child_only_observation(&txn, source_id, observation).await?;
    }
    txn.commit().await.context("commit child block record")?;
    Ok(outcome)
}
