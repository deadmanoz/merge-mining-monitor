//! `child_block_observation` and `child_miner_identity` writes.

use std::collections::HashMap;

use anyhow::{Context, Result, bail, ensure};
use tokio_postgres::GenericClient;

use mmm_capture::capture::{ChildBlockObservation, ObservationKind};
use mmm_capture::pool_resolver::RSK_MINER_ADDRESS_NAMESPACE;

use crate::event::{Exactness, identity_conflict_clause};

/// Write an event's observation, before the event itself when it is new, and return its id.
pub(crate) async fn write_event_observation<C: GenericClient>(
    client: &C,
    source_id: i64,
    observation: &ChildBlockObservation,
) -> Result<i64> {
    write_observation(client, source_id, observation)
        .await?
        .context("event observation contradicts the stored child evidence")
}

/// Point an existing event at its observation.
pub(crate) async fn link_event_observation<C: GenericClient>(
    client: &C,
    event_id: i64,
    observation_id: i64,
) -> Result<()> {
    client
        .execute(
            "UPDATE merge_mining_event SET observation_id = $2 \
             WHERE id = $1 AND observation_id IS DISTINCT FROM $2",
            &[&event_id, &observation_id],
        )
        .await
        .context("link the event to its observation")?;
    Ok(())
}

/// Give the hashless observation of an event's child block the hash the event now
/// carries; when an exact observation of that block already exists, carry the
/// hashless one's identities and any child evidence the exact row lacks into it, point
/// the event at it and delete the hashless one. Displacement stays the exact row's: it
/// is decided per block hash.
pub(crate) async fn fold_hashless_observation<C: GenericClient>(
    client: &C,
    event_id: i64,
) -> Result<()> {
    let row = client
        .query_one(
            "SELECT \
                (SELECT o.id FROM child_block_observation o \
                  WHERE o.id = e.observation_id AND o.child_block_hash IS NULL), \
                (SELECT o.id FROM child_block_observation o \
                  WHERE o.source_id = e.source_id AND o.child_block_hash = e.child_block_hash) \
             FROM merge_mining_event e WHERE e.id = $1",
            &[&event_id],
        )
        .await
        .context("look up the event's hashless and exact observations")?;
    let hashless: Option<i64> = row.get(0);
    let exact: Option<i64> = row.get(1);
    match (hashless, exact) {
        (Some(hashless), None) => {
            client
                .execute(
                    "UPDATE child_block_observation o \
                     SET child_block_hash = e.child_block_hash, \
                         child_height = COALESCE(o.child_height, e.child_height) \
                     FROM merge_mining_event e WHERE e.id = $1 AND o.id = $2",
                    &[&event_id, &hashless],
                )
                .await
                .context("promote the event's hashless observation")?;
        }
        (Some(hashless), Some(exact)) => {
            let folded = client
                .execute(
                    &format!(
                        "UPDATE child_block_observation o SET {}, {}, \
                             child_difficulty_reported = \
                                 COALESCE(o.child_difficulty_reported, h.child_difficulty_reported), \
                             recorded_at = LEAST(o.recorded_at, h.recorded_at) \
                         FROM child_block_observation h \
                         WHERE o.id = $1 AND h.id = $2 AND {}",
                        fill_child_evidence_from("h"),
                        fill_near_coinbase_from("o", "h"),
                        child_evidence_agrees("h"),
                    ),
                    &[&exact, &hashless],
                )
                .await
                .context("fold the hashless observation into the exact one")?;
            ensure!(
                folded == 1,
                "the hashless observation contradicts the block's exact observation"
            );
            link_event_observation(client, event_id, exact).await?;
            client
                .execute(
                    "DELETE FROM child_block_observation WHERE id = $1",
                    &[&hashless],
                )
                .await
                .context("delete the folded hashless observation")?;
        }
        (None, _) => {}
    }
    Ok(())
}

/// Write a block that has no event (`not_merge_mined`, `non_bitcoin_parent`,
/// `unverified_parent`). A `not_merge_mined` or `unverified_parent` answer (an
/// endpoint's unauthenticated claim) never overwrites an evidence or near observation:
/// it leaves the proven parent in place and returns `None`. A `non_bitcoin_parent`
/// write follows the lineage gate's retraction of the block's events under its locks
/// and source-health bracket, so it overwrites any kind; a refusal there is a
/// contradiction and fails.
pub async fn upsert_child_only_observation<C: GenericClient>(
    client: &C,
    source_id: i64,
    observation: &ChildBlockObservation,
) -> Result<Option<i64>> {
    ensure!(
        observation.kind.is_child_only(),
        "an evidence or near observation is written through the capture path"
    );
    ensure!(
        observation.child_block_hash.is_some(),
        "a child-only observation requires the child block hash"
    );
    let written = write_observation(client, source_id, observation).await?;
    if written.is_none() && observation.kind == ObservationKind::NonBitcoinParent {
        bail!("child-only observation conflicts with an event-backed or contradictory observation");
    }
    Ok(written)
}

/// Bring the event's observation's identities and child pool in line with the event's
/// attribution rows (keeping the RSK miner identity the observation records), dropping
/// identities the event no longer carries. Run once per event after its last attribution
/// write (two statements: the identity insert and the update).
pub async fn sync_event_observation_attribution<C: GenericClient>(
    client: &C,
    event_id: i64,
) -> Result<()> {
    // Inserted first and resolved in the next statement; see `insert_child_identities`.
    client
        .execute(
            "INSERT INTO child_miner_identity (namespace, identifier) \
             SELECT DISTINCT namespace, matched_value FROM event_pool_attribution \
             WHERE event_id = $1 AND side = 'child_block' AND namespace NOT LIKE 'btc\\_%' \
             ON CONFLICT DO NOTHING",
            &[&event_id],
        )
        .await
        .context("insert the event's child miner identities")?;
    // The RSK miner identity is the observation's own, recorded with the block's facts.
    client
        .execute(
            "WITH n AS ( \
                 SELECT o.id, \
                        ARRAY( \
                            SELECT id FROM ( \
                                SELECT ci.id FROM child_miner_identity ci \
                                JOIN event_pool_attribution a \
                                  ON a.namespace = ci.namespace AND a.matched_value = ci.identifier \
                                WHERE a.event_id = $1 AND a.side = 'child_block' \
                                  AND a.namespace NOT LIKE 'btc\\_%' \
                                UNION \
                                SELECT ci.id FROM child_miner_identity ci \
                                WHERE ci.id = ANY (o.child_identity_ids) AND ci.namespace = $2 \
                            ) attributed ORDER BY id) AS child_identity_ids, \
                        (SELECT CASE WHEN count(DISTINCT a.pool_id) = 1 THEN min(a.pool_id) END \
                           FROM event_pool_attribution a \
                          WHERE a.event_id = $1 AND a.side = 'child_block' \
                            AND a.pool_id IS NOT NULL) AS child_pool_id \
                   FROM child_block_observation o \
                  WHERE o.id = (SELECT observation_id FROM merge_mining_event WHERE id = $1) \
             ) \
             UPDATE child_block_observation o SET \
                 child_identity_ids = n.child_identity_ids, \
                 child_pool_id = n.child_pool_id \
             FROM n \
             WHERE o.id = n.id \
               AND (o.child_identity_ids, o.child_pool_id) IS DISTINCT FROM \
                   (n.child_identity_ids, n.child_pool_id)",
            &[&event_id, &RSK_MINER_ADDRESS_NAMESPACE],
        )
        .await
        .context("sync the event observation's child attribution")?;
    Ok(())
}

/// The child evidence an observation keeps once known: a later write or fold fills only
/// what is still NULL.
const CHILD_EVIDENCE_COLUMNS: [&str; 9] = [
    "child_height",
    "child_block_time",
    "child_header_bytes",
    "child_nbits",
    "child_prev_block_hash",
    "child_coinbase_txid",
    "child_coinbase_script",
    "child_coinbase_outputs",
    "pow_validates_child_target",
];

/// The child evidence two records of one block must agree on where both hold it.
const CHECKED_CHILD_EVIDENCE: [&str; 6] = [
    "child_height",
    "child_block_time",
    "child_header_bytes",
    "child_nbits",
    "pow_validates_child_target",
    "child_prev_block_hash",
];

/// A predicate that observation `o` and row `other` do not contradict each other's child
/// evidence.
fn child_evidence_agrees(other: &str) -> String {
    CHECKED_CHILD_EVIDENCE
        .iter()
        .map(|column| {
            format!(
                "(o.{column} IS NULL OR {other}.{column} IS NULL OR o.{column} = {other}.{column})"
            )
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// `SET` items filling observation `o`'s missing child evidence from row `from`, and
/// merging both rows' identities.
fn fill_child_evidence_from(from: &str) -> String {
    CHILD_EVIDENCE_COLUMNS
        .iter()
        .map(|column| format!("{column} = COALESCE(o.{column}, {from}.{column})"))
        .chain(std::iter::once(format!(
            "child_identity_ids = ARRAY(SELECT DISTINCT id FROM \
             unnest(o.child_identity_ids || {from}.child_identity_ids) AS merged(id) ORDER BY id)"
        )))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `SET` items for a near parent's coinbase: kept and completed from row `from` while
/// both rows name the same near parent (`kind_row` holds the resulting kind), and
/// taken from the row that decides the kind otherwise.
fn fill_near_coinbase_from(kind_row: &str, from: &str) -> String {
    ["btc_parent_coinbase_txid", "btc_parent_coinbase_script", "btc_parent_coinbase_outputs"]
        .iter()
        .map(|column| {
            format!(
                "{column} = CASE WHEN {kind_row}.observation_kind = 'near' \
                     AND {from}.observation_kind = 'near' \
                     AND o.btc_parent_header_hash IS NOT DISTINCT FROM {from}.btc_parent_header_hash \
                 THEN COALESCE(o.{column}, {from}.{column}) ELSE {kind_row}.{column} END"
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A rewrite of the same near parent resolves its pool afresh but keeps the stored one when it
/// resolves none, as the coinbase it comes from is kept; any other write takes the new value.
const NEAR_PARENT_POOL_SET: &str = "btc_parent_pool_id = CASE \
    WHEN EXCLUDED.observation_kind = 'near' AND o.observation_kind = 'near' \
     AND o.btc_parent_header_hash IS NOT DISTINCT FROM EXCLUDED.btc_parent_header_hash \
    THEN COALESCE(EXCLUDED.btc_parent_pool_id, o.btc_parent_pool_id) \
    ELSE EXCLUDED.btc_parent_pool_id END";

async fn write_observation<C: GenericClient>(
    client: &C,
    source_id: i64,
    observation: &ChildBlockObservation,
) -> Result<Option<i64>> {
    let identity_ids = child_identity_ids(client, observation).await?;
    let conflict = identity_conflict_clause(if observation.child_block_hash.is_some() {
        Exactness::Exact
    } else {
        Exactness::Partial
    });
    // An unauthenticated child-only answer never overwrites a proven parent.
    let kind_guard = if matches!(
        observation.kind,
        ObservationKind::NotMergeMined | ObservationKind::UnverifiedParent
    ) {
        "AND o.observation_kind NOT IN ('evidence', 'near')"
    } else {
        ""
    };
    // A child block commits to one parent, so a stored parent is never contradicted; only
    // evidence and near rows store one (`chk_cbo_parent_by_kind`).
    // A rewrite that resolves no child pool keeps the stored one, which reclassify-pools may
    // have resolved after the writer loaded its registry; an event's attributions do the same.
    // Known child pools that disagree make it Unknown.
    let near = observation.near_parent.as_ref();
    let child_nbits = observation.child_nbits.map(i64::from);
    let sql = format!(
        "INSERT INTO child_block_observation AS o ( \
            source_id, child_height, child_block_hash, observation_kind, \
            child_block_time, child_header_bytes, child_nbits, child_prev_block_hash, \
            child_coinbase_txid, child_coinbase_script, child_coinbase_outputs, \
            pow_validates_child_target, \
            btc_parent_header_hash, btc_parent_header_bytes, btc_parent_header_time, \
            btc_parent_coinbase_txid, btc_parent_coinbase_script, btc_parent_coinbase_outputs, \
            child_identity_ids, child_pool_id, btc_parent_pool_id, recorded_at \
         ) VALUES ( \
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
            $13, $14, $15, $16, $17, $18, $19, $20, $21, $22 \
         ) \
         {conflict} \
            observation_kind = EXCLUDED.observation_kind, \
            {fill}, \
            btc_parent_header_hash = EXCLUDED.btc_parent_header_hash, \
            btc_parent_header_bytes = EXCLUDED.btc_parent_header_bytes, \
            btc_parent_header_time = EXCLUDED.btc_parent_header_time, \
            {near_coinbase}, \
            child_pool_id = CASE WHEN $23 THEN NULL \
                ELSE COALESCE(EXCLUDED.child_pool_id, o.child_pool_id) END, \
            {near_parent_pool} \
         WHERE (o.btc_parent_header_hash IS NULL OR EXCLUDED.btc_parent_header_hash IS NULL \
                OR o.btc_parent_header_hash = EXCLUDED.btc_parent_header_hash) \
           AND {agrees} \
           {kind_guard} \
         RETURNING id",
        agrees = child_evidence_agrees("EXCLUDED"),
        fill = fill_child_evidence_from("EXCLUDED"),
        near_coinbase = fill_near_coinbase_from("EXCLUDED", "EXCLUDED"),
        near_parent_pool = NEAR_PARENT_POOL_SET,
    );
    let row = client
        .query_opt(
            &sql,
            &[
                &source_id,
                &observation.child_height,
                &observation.child_block_hash,
                &observation.kind.as_db_str(),
                &observation.child_block_time,
                &observation.child_header_bytes,
                &child_nbits,
                &observation.child_prev_block_hash,
                &observation.child_coinbase_txid,
                &observation.child_coinbase_script,
                &observation.child_coinbase_outputs,
                &observation.pow_validates_child_target,
                &observation.btc_parent_header_hash,
                &near.map(|parent| parent.header_bytes.as_slice()),
                &near.map(|parent| parent.header_time),
                &near.and_then(|parent| parent.coinbase_txid.as_deref()),
                &near.and_then(|parent| parent.coinbase_script.as_deref()),
                &near.and_then(|parent| parent.coinbase_outputs.as_deref()),
                &identity_ids,
                &observation.child_pool_id,
                &observation.btc_parent_pool_id,
                &observation.recorded_at,
                &observation.child_pools_conflict,
            ],
        )
        .await
        .context("upsert child_block_observation")?;
    Ok(row.map(|row| row.get(0)))
}

/// Resolve the observation's identifiers to sorted `child_miner_identity` ids, inserting new ones.
async fn child_identity_ids<C: GenericClient>(
    client: &C,
    observation: &ChildBlockObservation,
) -> Result<Vec<i64>> {
    if observation.child_identities.is_empty() {
        return Ok(Vec::new());
    }
    let namespaces = observation
        .child_identities
        .iter()
        .map(|identity| identity.namespace.as_str())
        .collect::<Vec<_>>();
    let identifiers = observation
        .child_identities
        .iter()
        .map(|identity| identity.identifier.as_str())
        .collect::<Vec<_>>();
    insert_child_identities(client, &namespaces, &identifiers).await?;
    let row = client
        .query_one(
            "SELECT COALESCE(array_agg(ci.id ORDER BY ci.id), '{}') \
             FROM child_miner_identity ci \
             JOIN unnest($1::text[], $2::text[]) AS input(namespace, identifier) \
               USING (namespace, identifier)",
            &[&namespaces, &identifiers],
        )
        .await
        .context("resolve child miner identities")?;
    Ok(row.get(0))
}

/// Insert identifiers not yet in `child_miner_identity`. Callers resolve ids in a later
/// statement: when this insert skips a row another transaction committed meanwhile, its
/// own statement snapshot cannot see that row, but the next statement's can.
pub(crate) async fn insert_child_identities<C: GenericClient>(
    client: &C,
    namespaces: &[&str],
    identifiers: &[&str],
) -> Result<()> {
    client
        .execute(
            "INSERT INTO child_miner_identity (namespace, identifier) \
             SELECT * FROM unnest($1::text[], $2::text[]) \
             ON CONFLICT DO NOTHING",
            &[&namespaces, &identifiers],
        )
        .await
        .context("insert child miner identities")?;
    Ok(())
}

/// An observation no event backs, as `reclassify-pools` re-resolves its pools.
#[derive(Debug)]
pub struct ObservationPoolRow {
    pub id: i64,
    pub source_code: String,
    pub near: bool,
    pub child_identity_ids: Vec<i64>,
    pub child_coinbase_script: Option<Vec<u8>>,
    pub child_coinbase_outputs: Option<Vec<u8>>,
    pub btc_parent_coinbase_script: Option<Vec<u8>>,
    pub btc_parent_coinbase_outputs: Option<Vec<u8>>,
    pub child_pool_id: Option<i64>,
    pub btc_parent_pool_id: Option<i64>,
}

/// A keyset page of observations no event backs (nears and child-only rows) whose pools a
/// pass could change: all of them under `overwrite`, otherwise those missing a pool they
/// carry something to resolve it from (child outputs only from `payout_sources`).
pub async fn load_observation_pool_batch<C: GenericClient>(
    client: &C,
    cursor: Option<i64>,
    source_id: Option<i64>,
    overwrite: bool,
    payout_sources: &[&str],
    limit: i64,
) -> Result<Vec<ObservationPoolRow>> {
    let rows = client
        .query(
            "SELECT o.id, s.code, o.observation_kind = 'near', o.child_identity_ids, \
                    o.child_coinbase_script, o.child_coinbase_outputs, \
                    o.btc_parent_coinbase_script, o.btc_parent_coinbase_outputs, \
                    o.child_pool_id, o.btc_parent_pool_id \
             FROM child_block_observation o \
             JOIN source s ON s.id = o.source_id \
             WHERE o.observation_kind <> 'evidence' \
               AND ($1::bigint IS NULL OR o.id > $1) \
               AND ($2::bigint IS NULL OR o.source_id = $2) \
               AND ($3 \
                    OR (o.child_pool_id IS NULL \
                        AND (cardinality(o.child_identity_ids) > 0 \
                             OR o.child_coinbase_script IS NOT NULL \
                             OR (o.child_coinbase_outputs IS NOT NULL AND s.code = ANY($4)))) \
                    OR (o.observation_kind = 'near' AND o.btc_parent_pool_id IS NULL \
                        AND (o.btc_parent_coinbase_script IS NOT NULL \
                             OR o.btc_parent_coinbase_outputs IS NOT NULL))) \
             ORDER BY o.id LIMIT $5",
            &[&cursor, &source_id, &overwrite, &payout_sources, &limit],
        )
        .await
        .context("load an observation pool batch")?;
    Ok(rows
        .iter()
        .map(|row| ObservationPoolRow {
            id: row.get(0),
            source_code: row.get(1),
            near: row.get(2),
            child_identity_ids: row.get(3),
            child_coinbase_script: row.get(4),
            child_coinbase_outputs: row.get(5),
            btc_parent_coinbase_script: row.get(6),
            btc_parent_coinbase_outputs: row.get(7),
            child_pool_id: row.get(8),
            btc_parent_pool_id: row.get(9),
        })
        .collect())
}

/// Each child identity's current registry pool, for identities the registry maps.
pub async fn load_child_identity_pools<C: GenericClient>(client: &C) -> Result<HashMap<i64, i64>> {
    let rows = client
        .query(
            "SELECT ci.id, pi.pool_id FROM child_miner_identity ci \
             JOIN pool_identity pi ON pi.namespace = ci.namespace AND pi.identifier = ci.identifier \
             WHERE pi.pool_id IS NOT NULL",
            &[],
        )
        .await
        .context("load child identity pools")?;
    Ok(rows.iter().map(|row| (row.get(0), row.get(1))).collect())
}

/// Set each loaded row's re-resolved `(child pool, parent pool)` in one statement, and return
/// how many were set. A row a capture changed since it was loaded keeps what the capture wrote.
pub async fn update_observation_pools<C: GenericClient>(
    client: &C,
    updates: &[(&ObservationPoolRow, Option<i64>, Option<i64>)],
) -> Result<u64> {
    if updates.is_empty() {
        return Ok(0);
    }
    let ids = updates.iter().map(|update| update.0.id).collect::<Vec<_>>();
    let nears = updates
        .iter()
        .map(|update| update.0.near)
        .collect::<Vec<_>>();
    let loaded_child_pools = updates
        .iter()
        .map(|update| update.0.child_pool_id)
        .collect::<Vec<_>>();
    let loaded_parent_pools = updates
        .iter()
        .map(|update| update.0.btc_parent_pool_id)
        .collect::<Vec<_>>();
    let child_pools = updates.iter().map(|update| update.1).collect::<Vec<_>>();
    let parent_pools = updates.iter().map(|update| update.2).collect::<Vec<_>>();
    client
        .execute(
            "UPDATE child_block_observation o \
                SET child_pool_id = u.child_pool_id, btc_parent_pool_id = u.btc_parent_pool_id \
               FROM unnest($1::bigint[], $2::boolean[], $3::bigint[], $4::bigint[], \
                           $5::bigint[], $6::bigint[]) \
                    AS u(id, near, loaded_child_pool_id, loaded_parent_pool_id, \
                         child_pool_id, btc_parent_pool_id) \
              WHERE o.id = u.id \
                AND o.observation_kind <> 'evidence' \
                AND (o.observation_kind = 'near') = u.near \
                AND o.child_pool_id IS NOT DISTINCT FROM u.loaded_child_pool_id \
                AND o.btc_parent_pool_id IS NOT DISTINCT FROM u.loaded_parent_pool_id",
            &[
                &ids,
                &nears,
                &loaded_child_pools,
                &loaded_parent_pools,
                &child_pools,
                &parent_pools,
            ],
        )
        .await
        .context("update re-resolved observation pools")
}
