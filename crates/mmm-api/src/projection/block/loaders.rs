//! SQL loaders and row types for the block-detail projection.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio_postgres::Client;

use super::super::shared::{
    PoolObject, SourceRecord, TreeCompetition, display_hash, parent_kind_from_db, pool_from_columns,
};
use super::detail::first_contributing_id;
use super::{
    ApiProof, BlockStaleBranch, EventPoolAttributionProjection, EventPoolAttributions,
    PoolIdentityRef, SourceRef,
};
use crate::normalize::ParentKind;

/// In-memory `block` row for the detail projection (already
/// pub(super)-scoped to the private block module). `header_bytes` and
/// `btc_coinbase_script` hold stored wire-order bytes, decoded downstream
/// (constraint #5). `core_attested`/`pow_validated`/`live_observed` drive the
/// source summary.
#[derive(Debug)]
pub(super) struct BlockDetailRow {
    pub(super) hash: Vec<u8>,
    pub(super) prev_hash: Vec<u8>,
    pub(super) height: Option<i32>,
    pub(super) kind: ParentKind,
    pub(super) btc_orphan_class: Option<String>,
    pub(super) header_bytes: Vec<u8>,
    pub(super) header_time: i64,
    pub(super) bitcoin_miner_pool: PoolObject,
    pub(super) live_observed: bool,
    pub(super) core_attested: bool,
    pub(super) pow_validated: bool,
    pub(super) btc_coinbase_script: Option<Vec<u8>>,
}

/// In-memory merge_mining_event row (with joined source + RSK sidecar) for the
/// detail projection. pub(super) only. All hash/script/proof fields hold stored
/// wire-order bytes; `render_event_details` and `derive_commitment` decode
/// them. `kind` is the per-event btc_parent_kind, distinct from the block's.
#[derive(Debug)]
pub(super) struct EventDetailRow {
    pub(super) id: i64,
    pub(super) source: SourceRecord,
    pub(super) child_height: i32,
    pub(super) child_block_hash: Vec<u8>,
    pub(super) child_block_time: i64,
    pub(super) parent_hash: Vec<u8>,
    pub(super) prev_hash: Vec<u8>,
    pub(super) parent_header_bytes: Vec<u8>,
    pub(super) parent_header_time: i64,
    pub(super) kind: ParentKind,
    pub(super) btc_parent_coinbase_txid: Option<Vec<u8>>,
    pub(super) btc_parent_coinbase_script: Option<Vec<u8>>,
    pub(super) btc_parent_coinbase_outputs: Option<Vec<u8>>,
    pub(super) child_coinbase_txid: Option<Vec<u8>>,
    pub(super) child_coinbase_script: Option<Vec<u8>>,
    pub(super) aux_merkle_proof: Option<Vec<u8>>,
    pub(super) pow_validates_btc_target: bool,
    pub(super) pow_validates_child_target: Option<bool>,
    pub(super) difficulty_epoch_ok: Option<bool>,
    pub(super) event_discovered_at: i64,
    pub(super) event_confirmed_at: i64,
    pub(super) event_revoked_at: Option<i64>,
    pub(super) event_revocation_reason: Option<String>,
    pub(super) child_miner_pool: PoolObject,
    pub(super) rsk: Option<RskEvidenceRow>,
}

/// RSK merge-mining sidecar row (rsk_merge_mining_evidence join) for an
/// `EventDetailRow`. pub(super) only. Present iff the event source is RSK
/// (loader bails if an RSK event has no sidecar). Bytes are stored wire-order,
/// hex-encoded by `render_rsk_detail`.
#[derive(Debug)]
pub(super) struct RskEvidenceRow {
    pub(super) block_hash: Vec<u8>,
    pub(super) height: i32,
    pub(super) is_uncle: bool,
    pub(super) uncle_index: Option<i32>,
    pub(super) uncle_parent_height: Option<i32>,
    pub(super) miner: Vec<u8>,
    pub(super) pool_identity: Option<PoolIdentityRef>,
    pub(super) merge_mining_hash: Vec<u8>,
    pub(super) merkle_proof: Option<Vec<u8>>,
    pub(super) coinbase_tail: Option<Vec<u8>>,
    pub(super) proof_format: String,
}

/// One member of a stale branch during `load_stale_branch` assembly (recursive
/// walk). pub(super) only. `canonical_competitor_hash` is the canonical block
/// that displaced this stale member; all hashes are stored wire-order,
/// display-converted at projection time.
#[derive(Debug, Clone)]
pub(super) struct StaleMemberRow {
    pub(super) hash: Vec<u8>,
    pub(super) prev_hash: Vec<u8>,
    pub(super) height: i32,
    pub(super) canonical_competitor_hash: Vec<u8>,
}

/// Hard cap on stale-branch members rendered in the block detail. The SQL
/// queries LIMIT+1 and `load_stale_branch` bails if exceeded, so an oversized
/// branch is a 500 (a deliberate signal), never a silently truncated payload.
pub(super) const STALE_BRANCH_DETAIL_LIMIT: usize = 500;

/// Load the `block` row for a hash (stored byte order), LEFT JOIN pool. `None`
/// means no read-model row exists, which routes `block()` to the direct-event
/// path. Returns the joined miner-pool object resolved via `pool_from_columns`.
pub(super) async fn load_block_detail(
    client: &Client,
    hash: &[u8],
) -> Result<Option<BlockDetailRow>> {
    let row = client
        .query_opt(
            "SELECT b.btc_header_hash, b.btc_prev_header_hash, b.btc_height, b.kind, \
                    b.btc_header_bytes, b.btc_header_time, b.live_observed, \
                    b.core_attested, b.pow_validated, p.id, p.slug, p.canonical_name, \
                    b.btc_orphan_class, b.btc_coinbase_script \
             FROM block b \
             LEFT JOIN pool p ON p.id = b.bitcoin_miner_pool_id \
             WHERE b.btc_header_hash = $1",
            &[&hash],
        )
        .await
        .context("load block detail")?;
    row.map(|row| {
        let kind: String = row.get(3);
        Ok(BlockDetailRow {
            hash: row.get(0),
            prev_hash: row.get(1),
            height: row.get(2),
            kind: parent_kind_from_db(&kind)?,
            btc_orphan_class: row.get(12),
            header_bytes: row.get(4),
            header_time: row.get(5),
            live_observed: row.get(6),
            core_attested: row.get(7),
            pow_validated: row.get(8),
            btc_coinbase_script: row.get(13),
            bitcoin_miner_pool: pool_from_columns(row.get(9), row.get(10), row.get(11)),
        })
    })
    .transpose()
}

/// Load active (non-revoked) attestation proofs for a parent hash. Sorted by
/// source code then first contributing event id, so the wire `proofs` ordering
/// is deterministic across runs (pinned by block-*.json). Returns wire-ready
/// `ApiProof`.
pub(super) async fn load_proof_details_for_hash(
    client: &Client,
    hash: &[u8],
) -> Result<Vec<ApiProof>> {
    let rows = client
        .query(
            "SELECT ap.proof_kind, s.id, s.code, s.kind, s.chain, s.instance, \
                    ap.discovered_at, ap.confirmed_at, ap.revoked_at, \
                    ap.revocation_reason, ap.pow_validated, ap.evidence \
             FROM attestation_proof ap \
             JOIN source s ON s.id = ap.source_id \
             WHERE ap.revoked_at IS NULL \
               AND ap.btc_header_hash = $1",
            &[&hash],
        )
        .await
        .context("load block proof details")?;
    let mut proofs = rows
        .into_iter()
        .map(|row| ApiProof {
            kind: row.get(0),
            source: SourceRef {
                id: row.get(1),
                code: row.get(2),
                kind: row.get(3),
                chain: row.get(4),
                instance: row.get(5),
            },
            discovered_at: row.get(6),
            confirmed_at: row.get(7),
            revoked_at: row.get(8),
            revocation_reason: row.get(9),
            pow_validates_btc_target: row.get(10),
            evidence: row.get(11),
        })
        .collect::<Vec<_>>();
    proofs.sort_by(|a, b| {
        a.source.code.cmp(&b.source.code).then_with(|| {
            first_contributing_id(&a.evidence).cmp(&first_contributing_id(&b.evidence))
        })
    });
    Ok(proofs)
}

/// Load active merge_mining_event rows whose btc_parent_header_hash matches
/// (stored byte order). The hash-keyed variant used for unknown read-model
/// blocks and the direct-event path; the id-keyed variant
/// (`load_event_details_by_ids`) is used for canonical/stale via proof
/// contributing ids.
pub(super) async fn load_event_details_by_hash(
    client: &Client,
    hash: &[u8],
) -> Result<Vec<EventDetailRow>> {
    let rows = client
        .query(&event_detail_sql("e.btc_parent_header_hash = $1"), &[&hash])
        .await
        .context("load block event details by hash")?;
    map_event_detail_rows(rows)
}

/// Load active merge_mining_event rows by explicit id set (the proof
/// `contributing_event_ids` for canonical/stale blocks). Empty ids
/// short-circuit to empty. Sibling of `load_event_details_by_hash`; the two
/// share `event_detail_sql` and differ only in predicate.
pub(super) async fn load_event_details_by_ids(
    client: &Client,
    ids: &[i64],
) -> Result<Vec<EventDetailRow>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let rows = client
        .query(&event_detail_sql("e.id = ANY($1::bigint[])"), &[&ids])
        .await
        .context("load block event details by ids")?;
    map_event_detail_rows(rows)
}

/// Load event_pool_attribution rows for an event id set, bucketed by side
/// (btc_parent / child_block) into `EventPoolAttributions`. Empty ids
/// short-circuit. `pool` resolves via COALESCE(pool_identity.pool_id,
/// attribution.pool_id); an unrecognized side is a hard error (invariant
/// guard). ORDER BY event_id, side, id pins the per-event wire ordering.
pub(super) async fn load_pool_attributions_by_event(
    client: &Client,
    ids: &[i64],
) -> Result<HashMap<i64, EventPoolAttributions>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = client
        .query(
            "SELECT a.event_id, a.side, a.namespace, a.match_kind, a.matched_value, \
                    p.id, p.slug, p.canonical_name, pi.id, pi.namespace, pi.identifier, \
                    a.source, a.confidence, a.details \
             FROM event_pool_attribution a \
             LEFT JOIN pool_identity pi ON pi.id = a.pool_identity_id \
             LEFT JOIN pool p ON p.id = COALESCE(pi.pool_id, a.pool_id) \
             WHERE a.event_id = ANY($1::bigint[]) \
             ORDER BY a.event_id, a.side, a.id",
            &[&ids],
        )
        .await
        .context("load event pool attributions")?;

    let mut by_event: HashMap<i64, EventPoolAttributions> = HashMap::new();
    for row in rows {
        let event_id: i64 = row.get(0);
        let side: String = row.get(1);
        let pool_identity_id: Option<i64> = row.get(8);
        let projection = EventPoolAttributionProjection {
            namespace: row.get(2),
            match_kind: row.get(3),
            matched_value: row.get(4),
            pool: pool_from_columns(row.get(5), row.get(6), row.get(7)),
            pool_identity: pool_identity_id.map(|id| PoolIdentityRef {
                id,
                namespace: row.get(9),
                identifier: row.get(10),
            }),
            source: row.get(11),
            confidence: row.get(12),
            details: row.get(13),
        };
        let entry = by_event.entry(event_id).or_default();
        match side.as_str() {
            "btc_parent" => entry.btc_parent.push(projection),
            "child_block" => entry.child_block.push(projection),
            other => bail!("invalid event_pool_attribution side {other}"),
        }
    }
    Ok(by_event)
}

/// The shared SELECT for both event-detail loaders, parameterized only by WHERE
/// `predicate` (the rule-of-two extraction that keeps by-hash and by-ids from
/// diverging). Column order is positionally coupled to `map_event_detail_rows`
/// / `rsk_evidence_from_row`; ORDER BY confirmed_at, source code, child height,
/// child hash, id pins deterministic event ordering. The predicate string is a
/// fixed internal literal, never user input (no injection risk).
pub(super) fn event_detail_sql(predicate: &str) -> String {
    format!(
        "SELECT e.id, s.id, s.code, s.kind, s.chain, s.instance, \
                e.child_height, e.child_block_hash, e.child_block_time, \
                e.btc_parent_header_hash, e.btc_parent_prev_header_hash, \
                e.btc_parent_header_bytes, e.btc_parent_header_time, \
                e.btc_parent_kind, \
                e.btc_parent_coinbase_txid, e.btc_parent_coinbase_script, \
                e.btc_parent_coinbase_outputs, e.child_coinbase_txid, \
                e.child_coinbase_script, e.aux_merkle_proof, \
                e.pow_validates_btc_target, e.pow_validates_child_target, \
                e.difficulty_epoch_ok, e.discovered_at, e.confirmed_at, \
                e.revoked_at, e.revocation_reason, cmp.id, cmp.slug, cmp.canonical_name, \
                r.rsk_block_hash, r.rsk_height, \
                r.is_uncle, r.uncle_index, r.uncle_parent_height, r.rsk_miner, \
                r.merge_mining_hash, r.merkle_proof, r.coinbase_tail, \
                r.proof_format, pi.id, pi.namespace, pi.identifier \
         FROM merge_mining_event e \
         JOIN source s ON s.id = e.source_id \
         LEFT JOIN pool cmp ON cmp.id = e.child_miner_pool_id \
         LEFT JOIN rsk_merge_mining_evidence r ON r.event_id = e.id \
         LEFT JOIN pool_identity pi ON pi.id = r.pool_identity_id \
         WHERE e.revoked_at IS NULL AND {predicate} \
         ORDER BY e.confirmed_at, s.code, e.child_height, e.child_block_hash, e.id"
    )
}

/// Decode `event_detail_sql` rows into `EventDetailRow`. Column indices are
/// positionally locked to `event_detail_sql`'s SELECT list; changing either
/// side requires changing both. Bytes stay stored wire-order (decoded later).
/// Delegates the RSK sidecar to `rsk_evidence_from_row`.
pub(super) fn map_event_detail_rows(rows: Vec<tokio_postgres::Row>) -> Result<Vec<EventDetailRow>> {
    rows.into_iter()
        .map(|row| {
            let kind: String = row.get(13);
            let source = SourceRecord {
                id: row.get(1),
                code: row.get(2),
                kind: row.get(3),
                chain: row.get(4),
            };
            let rsk = rsk_evidence_from_row(&row, &source.code)?;
            Ok(EventDetailRow {
                id: row.get(0),
                source,
                child_height: row.get(6),
                child_block_hash: row.get(7),
                child_block_time: row.get(8),
                parent_hash: row.get(9),
                prev_hash: row.get(10),
                parent_header_bytes: row.get(11),
                parent_header_time: row.get(12),
                kind: parent_kind_from_db(&kind)?,
                btc_parent_coinbase_txid: row.get(14),
                btc_parent_coinbase_script: row.get(15),
                btc_parent_coinbase_outputs: row.get(16),
                child_coinbase_txid: row.get(17),
                child_coinbase_script: row.get(18),
                aux_merkle_proof: row.get(19),
                pow_validates_btc_target: row.get(20),
                pow_validates_child_target: row.get(21),
                difficulty_epoch_ok: row.get(22),
                event_discovered_at: row.get(23),
                event_confirmed_at: row.get(24),
                event_revoked_at: row.get(25),
                event_revocation_reason: row.get(26),
                child_miner_pool: pool_from_columns(row.get(27), row.get(28), row.get(29)),
                rsk,
            })
        })
        .collect()
}

/// Extract the optional RSK sidecar from a joined event row. `None` for
/// non-RSK; a missing sidecar on an RSK-coded source is a hard error (data
/// invariant). `identifier` is lowercased. Column indices are positionally
/// locked to `event_detail_sql`.
pub(super) fn rsk_evidence_from_row(
    row: &tokio_postgres::Row,
    source_code: &str,
) -> Result<Option<RskEvidenceRow>> {
    let block_hash: Option<Vec<u8>> = row.get(30);
    if source_code == mmm_capture::source_registry::RSK_SOURCE_CODE && block_hash.is_none() {
        bail!("RSK event is missing rsk_merge_mining_evidence sidecar");
    }
    let Some(block_hash) = block_hash else {
        return Ok(None);
    };
    let pool_identity_id: Option<i64> = row.get(40);
    Ok(Some(RskEvidenceRow {
        block_hash,
        height: row
            .get::<_, Option<i32>>(31)
            .context("RSK height missing")?,
        is_uncle: row
            .get::<_, Option<bool>>(32)
            .context("RSK uncle flag missing")?,
        uncle_index: row.get(33),
        uncle_parent_height: row.get(34),
        miner: row
            .get::<_, Option<Vec<u8>>>(35)
            .context("RSK miner missing")?,
        merge_mining_hash: row
            .get::<_, Option<Vec<u8>>>(36)
            .context("RSK merge-mining hash missing")?,
        merkle_proof: row.get(37),
        coinbase_tail: row.get(38),
        proof_format: row
            .get::<_, Option<String>>(39)
            .context("RSK proof format missing")?,
        pool_identity: pool_identity_id.map(|id| PoolIdentityRef {
            id,
            namespace: row.get(41),
            identifier: row.get::<_, String>(42).to_ascii_lowercase(),
        }),
    }))
}

/// Derive the competition detail for a stale block (its canonical competitor +
/// pool-vs-pool timing). `None` when the canonical competitor is not derivable.
/// `propagation_delta_s` is hard-null until the competition-timing slice lands
/// (no column yet). Reuses the shared `TreeCompetition` DTO.
pub(super) async fn load_competition_detail(
    client: &Client,
    block: &BlockDetailRow,
) -> Result<Option<TreeCompetition>> {
    let Some(row) = client
        .query_opt(
            "SELECT stale.btc_header_hash, canonical.btc_header_hash, \
                    CASE WHEN canonical.btc_header_time - stale.btc_header_time \
                               BETWEEN -2147483648 AND 2147483647 \
                         THEN (canonical.btc_header_time - stale.btc_header_time)::int \
                         ELSE NULL END AS header_time_delta_s, \
                    sp.id, sp.slug, sp.canonical_name, cp.id, cp.slug, cp.canonical_name \
             FROM block stale \
             JOIN block canonical ON canonical.btc_header_hash = stale.canonical_competitor_hash \
             LEFT JOIN pool sp ON sp.id = stale.bitcoin_miner_pool_id \
             LEFT JOIN pool cp ON cp.id = canonical.bitcoin_miner_pool_id \
             WHERE stale.btc_header_hash = $1 \
               AND stale.kind = 'stale' \
               AND canonical.kind = 'canonical'",
            &[&block.hash],
        )
        .await
        .context("derive stale competition detail")?
    else {
        return Ok(None);
    };
    Ok(Some(TreeCompetition {
        btc_height: block.height.unwrap_or_default(),
        stale_hash: display_hash(&row.get::<_, Vec<u8>>(0))?,
        canonical_hash: display_hash(&row.get::<_, Vec<u8>>(1))?,
        stale_bitcoin_miner_pool: pool_from_columns(row.get(3), row.get(4), row.get(5)),
        canonical_bitcoin_miner_pool: pool_from_columns(row.get(6), row.get(7), row.get(8)),
        header_time_delta_s: row.get(2),
        // No read-model column exists for this until the competition timing
        // slice lands.
        propagation_delta_s: None,
    }))
}

/// Walks a stale block's whole branch: ancestors to the root, then every
/// stale descendant, height-ordered and detail-limited.
const STALE_BRANCH_SQL: &str = "WITH RECURSIVE ancestors AS ( \
                SELECT btc_header_hash, btc_prev_header_hash, btc_height, canonical_competitor_hash \
                FROM block \
                WHERE btc_header_hash = $1 AND kind = 'stale' \
                UNION ALL \
                SELECT parent.btc_header_hash, parent.btc_prev_header_hash, parent.btc_height, \
                       parent.canonical_competitor_hash \
                FROM block parent \
                JOIN ancestors child ON child.btc_prev_header_hash = parent.btc_header_hash \
                WHERE parent.kind = 'stale' \
             ), root AS ( \
                SELECT * FROM ancestors \
                ORDER BY btc_height ASC, btc_header_hash ASC \
                LIMIT 1 \
             ), descendants AS ( \
                SELECT * FROM root \
                UNION ALL \
                SELECT child.btc_header_hash, child.btc_prev_header_hash, child.btc_height, \
                       child.canonical_competitor_hash \
                FROM block child \
                JOIN descendants parent ON child.btc_prev_header_hash = parent.btc_header_hash \
                WHERE child.kind = 'stale' \
             ) \
             SELECT btc_header_hash, btc_prev_header_hash, btc_height, canonical_competitor_hash \
             FROM descendants \
             ORDER BY btc_height, btc_header_hash \
             LIMIT $2";

/// Assemble the `BlockStaleBranch` for a selected stale block: run
/// `STALE_BRANCH_SQL`, bail if over `STALE_BRANCH_DETAIL_LIMIT` (LIMIT+1
/// overflow guard), and render the already height-ordered component. Derives
/// `position` (root/tip/interior) and the branch_id
/// `stale-{root_height}-{root_hash}`. All output hashes are display-order.
pub(super) async fn load_stale_branch(
    client: &Client,
    selected_hash: &[u8],
) -> Result<BlockStaleBranch> {
    let row_limit = (STALE_BRANCH_DETAIL_LIMIT + 1) as i64;
    let rows = client
        .query(STALE_BRANCH_SQL, &[&selected_hash, &row_limit])
        .await
        .context("load stale branch members")?;
    if rows.len() > STALE_BRANCH_DETAIL_LIMIT {
        bail!(
            "stale branch detail exceeds {} members",
            STALE_BRANCH_DETAIL_LIMIT
        );
    }
    let mut by_hash = HashMap::<Vec<u8>, StaleMemberRow>::new();
    let mut children_by_prev = HashMap::<Vec<u8>, Vec<Vec<u8>>>::new();
    let mut members = Vec::<StaleMemberRow>::new();
    for row in rows {
        let hash: Vec<u8> = row.get(0);
        let prev_hash: Vec<u8> = row.get(1);
        children_by_prev
            .entry(prev_hash.clone())
            .or_default()
            .push(hash.clone());
        let member = StaleMemberRow {
            hash: hash.clone(),
            prev_hash,
            height: row
                .get::<_, Option<i32>>(2)
                .context("stale branch member missing height")?,
            canonical_competitor_hash: row
                .get::<_, Option<Vec<u8>>>(3)
                .context("stale branch member missing canonical competitor")?,
        };
        by_hash.insert(hash, member.clone());
        members.push(member);
    }
    let selected = by_hash
        .get(selected_hash)
        .context("selected stale block missing from branch member set")?;
    let root = members.first().context("stale branch has no root")?;
    let tip = members.last().context("stale branch has no tip")?;
    let child_hashes = children_by_prev
        .get(selected_hash)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|hash| by_hash.contains_key(hash))
        .map(|hash| display_hash(&hash))
        .collect::<Result<Vec<_>>>()?;
    let parent_stale_hash = by_hash
        .contains_key(&selected.prev_hash)
        .then(|| display_hash(&selected.prev_hash))
        .transpose()?;
    let position = match (parent_stale_hash.is_some(), child_hashes.is_empty()) {
        (false, true) => "root_and_tip",
        (false, false) => "root",
        (true, true) => "tip",
        (true, false) => "interior",
    };
    Ok(BlockStaleBranch {
        branch_id: format!("stale-{}-{}", root.height, display_hash(&root.hash)?),
        root_hash: display_hash(&root.hash)?,
        tip_hash: display_hash(&tip.hash)?,
        member_hashes: members
            .iter()
            .map(|member| display_hash(&member.hash))
            .collect::<Result<Vec<_>>>()?,
        canonical_competitor_hashes: members
            .iter()
            .map(|member| display_hash(&member.canonical_competitor_hash))
            .collect::<Result<Vec<_>>>()?,
        btc_height_min: root.height,
        btc_height_max: tip.height,
        depth: members.len(),
        position,
        parent_stale_hash,
        child_stale_hashes: child_hashes,
    })
}
