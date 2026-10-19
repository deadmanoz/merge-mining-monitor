//! `child_block_observation` and `child_miner_identity` writes.

use anyhow::{Context, Result, bail, ensure};
use tokio_postgres::GenericClient;

use mmm_capture::capture::{ChildBlockObservation, ObservationKind};
use mmm_capture::pool_resolver::RSK_MINER_ADDRESS_NAMESPACE;

use crate::event::{EventWriteDisposition, EventWriteOutcome, Exactness, identity_conflict_clause};

/// Write the observation for an event the caller just upserted and link it through
/// `observation_id`.
///
/// The event upsert owns identity resolution, and its disposition says what the
/// observation needs: a promoted event's hashless observation learns the hash too (or is
/// folded into an exact observation that already exists), and a hashless payload that an
/// exact event satisfied takes that event's identity. Otherwise the payload's identity
/// is the event's.
pub(crate) async fn upsert_event_observation<C: GenericClient>(
    client: &C,
    source_id: i64,
    outcome: &EventWriteOutcome,
    mut observation: ChildBlockObservation,
) -> Result<i64> {
    match outcome.disposition {
        EventWriteDisposition::Promoted => {
            fold_hashless_observation(client, outcome.event_id).await?;
        }
        EventWriteDisposition::SatisfiedByExistingExact => {
            let event = client
                .query_one(
                    "SELECT child_height, child_block_hash FROM merge_mining_event WHERE id = $1",
                    &[&outcome.event_id],
                )
                .await
                .context("load the exact event's child identity")?;
            observation.child_height = event.get::<_, Option<i32>>(0).or(observation.child_height);
            observation.child_block_hash = event.get(1);
        }
        EventWriteDisposition::Inserted | EventWriteDisposition::Updated => {}
    }
    let observation_id = write_observation(client, source_id, &observation)
        .await?
        .context("event observation contradicts the stored child evidence")?;
    client
        .execute(
            "UPDATE merge_mining_event SET observation_id = $2 \
             WHERE id = $1 AND observation_id IS DISTINCT FROM $2",
            &[&outcome.event_id, &observation_id],
        )
        .await
        .context("link the event to its observation")?;
    Ok(observation_id)
}

/// The observation holding an event's child block: its `observation_id`, or, for an event
/// written before migration 0033 (unlinked until the swap migration), the observation with
/// its child identity. Each lookup is an equality on one identity index.
async fn event_observation_id<C: GenericClient>(client: &C, event_id: i64) -> Result<Option<i64>> {
    Ok(client
        .query_one(
            "SELECT COALESCE( \
                 e.observation_id, \
                 (SELECT o.id FROM child_block_observation o \
                   WHERE o.source_id = e.source_id AND o.child_block_hash = e.child_block_hash), \
                 (SELECT o.id FROM child_block_observation o \
                   WHERE e.child_block_hash IS NULL AND o.source_id = e.source_id \
                     AND o.child_block_hash IS NULL AND o.child_height = e.child_height \
                     AND o.btc_parent_header_hash = e.btc_parent_header_hash)) \
             FROM merge_mining_event e WHERE e.id = $1",
            &[&event_id],
        )
        .await
        .context("find the event's observation")?
        .get(0))
}

/// Give the hashless observation of an event's child block the hash the event now
/// carries; when an exact observation of that block already exists, carry the
/// hashless one's identities and any child evidence the exact row lacks into it and
/// delete it. Displacement stays the exact row's: it is decided per block hash.
async fn fold_hashless_observation<C: GenericClient>(client: &C, event_id: i64) -> Result<()> {
    let row = client
        .query_one(
            "SELECT \
                (SELECT o.id FROM child_block_observation o \
                  WHERE o.source_id = e.source_id AND o.child_block_hash IS NULL \
                    AND o.child_height = e.child_height \
                    AND o.btc_parent_header_hash = e.btc_parent_header_hash), \
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
            client
                .execute(
                    &format!(
                        "UPDATE child_block_observation o SET {}, {}, \
                             child_difficulty_reported = \
                                 COALESCE(o.child_difficulty_reported, h.child_difficulty_reported), \
                             recorded_at = LEAST(o.recorded_at, h.recorded_at) \
                         FROM child_block_observation h WHERE o.id = $1 AND h.id = $2",
                        fill_child_evidence_from("h"),
                        fill_near_coinbase_from("o", "h"),
                    ),
                    &[&exact, &hashless],
                )
                .await
                .context("fold the hashless observation into the exact one")?;
            client
                .execute(
                    "UPDATE merge_mining_event SET observation_id = NULL WHERE observation_id = $1",
                    &[&hashless],
                )
                .await
                .context("unlink the hashless observation")?;
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
/// `unverified_parent`). It never overwrites the observation of a child block that
/// still has an event: a `not_merge_mined` or `unverified_parent` answer for such a
/// block (an endpoint's unauthenticated claim) leaves its proven evidence in place and
/// returns `None`. A `non_bitcoin_parent` write follows its event's retraction in the
/// same transaction, so a refusal there is a contradiction and fails.
pub async fn upsert_child_only_observation<C: GenericClient>(
    client: &C,
    source_id: i64,
    observation: &ChildBlockObservation,
) -> Result<Option<i64>> {
    ensure!(
        observation.kind.is_child_only(),
        "an evidence or near observation is written with its event"
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

/// Bring the event's observation's identities and pools in line with the event's
/// attribution rows (plus an RSK sidecar's miner), dropping identities the event no
/// longer carries. Run once per event after its last attribution write (three
/// statements: the observation lookup, the identity insert and the update).
pub async fn sync_event_observation_attribution<C: GenericClient>(
    client: &C,
    event_id: i64,
) -> Result<()> {
    match event_observation_id(client, event_id).await? {
        Some(observation_id) => {
            sync_observation_attribution(client, event_id, observation_id).await
        }
        None => Ok(()),
    }
}

/// [`sync_event_observation_attribution`] for a caller that already holds the observation id.
pub(crate) async fn sync_observation_attribution<C: GenericClient>(
    client: &C,
    event_id: i64,
    observation_id: i64,
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
    client
        .execute(
            "UPDATE child_block_observation o SET \
                 child_identity_ids = n.child_identity_ids, \
                 child_pool_id = n.child_pool_id, \
                 btc_parent_pool_id = CASE WHEN o.observation_kind = 'near' THEN n.btc_parent_pool_id END \
             FROM ( \
                 SELECT ARRAY( \
                     SELECT id FROM ( \
                         SELECT ci.id FROM child_miner_identity ci \
                         JOIN event_pool_attribution a \
                           ON a.namespace = ci.namespace AND a.matched_value = ci.identifier \
                         WHERE a.event_id = e.id AND a.side = 'child_block' \
                           AND a.namespace NOT LIKE 'btc\\_%' \
                         UNION \
                         SELECT ci.id FROM child_miner_identity ci \
                         JOIN rsk_merge_mining_evidence r ON r.event_id = e.id \
                         WHERE ci.namespace = $3 AND ci.identifier = encode(r.rsk_miner, 'hex') \
                     ) attributed ORDER BY id) AS child_identity_ids, \
                     e.child_miner_pool_id AS child_pool_id, \
                     (SELECT CASE WHEN count(DISTINCT a.pool_id) = 1 THEN min(a.pool_id) END \
                        FROM event_pool_attribution a \
                       WHERE a.event_id = e.id AND a.side = 'btc_parent' \
                         AND a.pool_id IS NOT NULL) AS btc_parent_pool_id \
                 FROM merge_mining_event e WHERE e.id = $1 \
             ) n \
             WHERE o.id = $2 \
               AND (o.child_identity_ids, o.child_pool_id, o.btc_parent_pool_id) IS DISTINCT FROM \
                   (n.child_identity_ids, n.child_pool_id, \
                    CASE WHEN o.observation_kind = 'near' THEN n.btc_parent_pool_id END)",
            &[&event_id, &observation_id, &RSK_MINER_ADDRESS_NAMESPACE],
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
    // A block with no event never overwrites the observation of one that has an event.
    let event_guard = if observation.kind.is_child_only() {
        "AND NOT EXISTS (SELECT 1 FROM merge_mining_event e \
                         WHERE e.source_id = o.source_id AND e.child_block_hash = o.child_block_hash)"
    } else {
        ""
    };
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
            child_identity_ids, child_pool_id, recorded_at \
         ) VALUES ( \
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
            $13, $14, $15, $16, $17, $18, $19, $20, $21 \
         ) \
         {conflict} \
            observation_kind = EXCLUDED.observation_kind, \
            {fill}, \
            btc_parent_header_hash = EXCLUDED.btc_parent_header_hash, \
            btc_parent_header_bytes = EXCLUDED.btc_parent_header_bytes, \
            btc_parent_header_time = EXCLUDED.btc_parent_header_time, \
            {near_coinbase}, \
            child_pool_id = EXCLUDED.child_pool_id, \
            btc_parent_pool_id = NULL \
         WHERE (o.child_height IS NULL OR EXCLUDED.child_height IS NULL OR o.child_height = EXCLUDED.child_height) \
           AND (o.child_block_time IS NULL OR EXCLUDED.child_block_time IS NULL OR o.child_block_time = EXCLUDED.child_block_time) \
           AND (o.child_header_bytes IS NULL OR EXCLUDED.child_header_bytes IS NULL OR o.child_header_bytes = EXCLUDED.child_header_bytes) \
           AND (o.child_nbits IS NULL OR EXCLUDED.child_nbits IS NULL OR o.child_nbits = EXCLUDED.child_nbits) \
           AND (o.child_prev_block_hash IS NULL OR EXCLUDED.child_prev_block_hash IS NULL \
                OR o.child_prev_block_hash = EXCLUDED.child_prev_block_hash) \
           {event_guard} \
         RETURNING id",
        fill = fill_child_evidence_from("EXCLUDED"),
        near_coinbase = fill_near_coinbase_from("EXCLUDED", "EXCLUDED"),
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
                &observation.recorded_at,
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
