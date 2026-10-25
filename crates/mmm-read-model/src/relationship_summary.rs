//! `relationship_summary` derived rows: each `block` row's relationship signals summarized by the
//! API's rule (`mmm_capture::relationship`), keyed for the navigator by height and time. Writers
//! queue headers or pool identities in `relationship_refresh_queue`; the follower's drain
//! refreshes queued headers, the scheduled sweep expands queued identities and re-derives every
//! row on a persisted cursor, reporting drift (rows a write path failed to queue), and the rebuild
//! re-derives all rows when the compiled rules change. Every summary write holds one advisory
//! lock from its input reads through its upsert, so a refresh that read older inputs never
//! overwrites a newer one.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use bitcoin::hashes::{Hash as _, sha256};
use tokio_postgres::{Client, GenericClient};
use tracing::{info, warn};

use crate::ReconcileReadModelConfig;
use crate::writers::load_pool_ids_by_slug;
use mmm_capture::capture::now_epoch_seconds;
use mmm_capture::pool_resolver::DEFAULT_POOL_SNAPSHOT_JSON;
use mmm_capture::progress::ProgressReporter;
use mmm_capture::protocol_commitment::DECODER_VERSION;
use mmm_capture::relationship::{
    CoinbaseEvidence, SignalDraft, SignalPool, bitcoin_drafts, choose_coinbase_evidence, finish,
    protocol_account_draft,
};

/// Bump when the derivation or what the summary stores changes, so `--if-needed` rebuilds.
const DERIVATION_VERSION: u32 = 1;

const SUMMARY_LOCK_CLASS: i32 = 0x5253; // 'RS' - relationship summary writers
const SUMMARY_LOCK_OBJECT: i32 = 0;

/// Queue rows one live-follower tick drains, as `RELATIONSHIP_DRAIN_BATCHES` batches of this many.
pub const RELATIONSHIP_DRAIN_BATCH: i64 = 500;
pub const RELATIONSHIP_DRAIN_BATCHES: usize = 4;

/// A sweep run: the rows that differed without a queued refresh (drift), and whether it
/// finished a pass over every block.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RelationshipSweep {
    pub drifted: u64,
    pub pass_completed: bool,
}

const BLOCKS_SQL: &str = "\
    SELECT btc_header_hash, btc_header_time, btc_height, bitcoin_miner_pool_id, \
           btc_coinbase_status = 'complete', btc_coinbase_script, btc_coinbase_outputs \
      FROM block WHERE btc_header_hash = ANY($1::bytea[])";

/// Active events' parent coinbase copies; [`choose_coinbase_evidence`] picks one per header.
const EVENT_COINBASES_SQL: &str = "\
    SELECT btc_parent_header_hash, id, btc_parent_coinbase_script, \
           btc_parent_coinbase_outputs, btc_parent_coinbase_outputs_text \
      FROM merge_mining_event \
     WHERE btc_parent_header_hash = ANY($1::bytea[]) AND revoked_at IS NULL \
       AND (btc_parent_coinbase_script IS NOT NULL OR btc_parent_coinbase_outputs IS NOT NULL \
            OR btc_parent_coinbase_outputs_text IS NOT NULL)";

/// Child attribution rows in the tree's and block page's event order, then attribution order.
const CHILD_SIGNALS_SQL: &str = "\
    SELECT e.btc_parent_header_hash, s.code, a.namespace, a.matched_value, p.id \
      FROM merge_mining_event e \
      JOIN source s ON s.id = e.source_id \
      JOIN event_pool_attribution a ON a.event_id = e.id AND a.side = 'child_block' \
      LEFT JOIN pool_identity pi ON pi.id = a.pool_identity_id \
      LEFT JOIN pool p ON p.id = COALESCE(pi.pool_id, a.pool_id) \
     WHERE e.btc_parent_header_hash = ANY($1::bytea[]) AND e.revoked_at IS NULL \
     ORDER BY e.btc_parent_header_hash, e.confirmed_at, s.code, e.child_height NULLS LAST, \
              e.child_block_hash NULLS LAST, e.id, a.id";

/// Declared protocol accounts, only once the rebuild has recorded the running decoder (the API
/// shows none before then), in output order.
const PROTOCOL_ACCOUNTS_SQL: &str = "\
    SELECT c.btc_header_hash, s.protocol, s.account_identifier, p.id \
      FROM read_model_invariant g \
      JOIN protocol_commitment c ON c.btc_header_hash = ANY($1::bytea[]) \
      JOIN protocol_commitment_script s ON s.id = c.script_id \
      LEFT JOIN pool_identity i \
             ON (i.namespace, i.identifier) = (s.account_namespace, s.account_identifier) \
      LEFT JOIN pool p ON p.id = i.pool_id \
     WHERE g.id = TRUE AND g.protocol_commitments_decoder_version IS NOT DISTINCT FROM $2 \
     ORDER BY c.btc_header_hash, c.vout";

/// Take every queued pool identity off the queue and queue each block header it reaches: through
/// child attribution rows, and through the protocol declarations whose account it keys.
const EXPAND_IDENTITIES_SQL: &str = "\
    WITH identities AS ( \
        DELETE FROM relationship_refresh_queue WHERE identity_namespace IS NOT NULL \
        RETURNING identity_namespace AS namespace, identity_identifier AS identifier \
    ) \
    INSERT INTO relationship_refresh_queue (btc_header_hash) \
    SELECT DISTINCT hash FROM ( \
        SELECT e.btc_parent_header_hash AS hash \
          FROM identities k \
          JOIN pool_identity pi ON (pi.namespace, pi.identifier) = (k.namespace, k.identifier) \
          JOIN event_pool_attribution a ON a.pool_identity_id = pi.id AND a.side = 'child_block' \
          JOIN merge_mining_event e ON e.id = a.event_id AND e.revoked_at IS NULL \
        UNION \
        SELECT c.btc_header_hash \
          FROM identities k \
          JOIN protocol_commitment_script s \
            ON (s.account_namespace, s.account_identifier) = (k.namespace, k.identifier) \
          JOIN protocol_commitment c ON c.script_id = s.id \
    ) affected \
     WHERE EXISTS (SELECT 1 FROM block b WHERE b.btc_header_hash = affected.hash)";

const BLOCK_PAGE_SQL: &str = "\
    SELECT btc_header_hash FROM block WHERE btc_header_hash > $1 \
     ORDER BY btc_header_hash LIMIT $2";

/// One header's block row as loaded.
struct Header {
    hash: Vec<u8>,
    header_time: i64,
    height: Option<i32>,
    miner: Option<i64>,
    complete_coinbase: Option<CoreCoinbase>,
}

/// The block row's complete Core coinbase: `(script, outputs)`.
type CoreCoinbase = (Option<Vec<u8>>, Option<Vec<u8>>);

/// One event's parent coinbase copy: `(event id, script, outputs, output text)`.
type EventCoinbase = (i64, Option<Vec<u8>>, Option<Vec<u8>>, Option<String>);

/// Signal drafts by header hash.
type DraftsByHash = HashMap<Vec<u8>, Vec<SignalDraft<Option<i64>>>>;

/// Take the summary writers' lock for this transaction; with `wait` false, return whether it
/// was free instead of waiting.
async fn lock_summaries<C: GenericClient>(client: &C, wait: bool) -> Result<bool> {
    let key: [&(dyn tokio_postgres::types::ToSql + Sync); 2] =
        [&SUMMARY_LOCK_CLASS, &SUMMARY_LOCK_OBJECT];
    if wait {
        client
            .execute("SELECT pg_advisory_xact_lock($1, $2)", &key)
            .await
            .context("take the relationship summary lock")?;
        return Ok(true);
    }
    Ok(client
        .query_one("SELECT pg_try_advisory_xact_lock($1, $2)", &key)
        .await
        .context("try the relationship summary lock")?
        .get(0))
}

/// Re-derive the summaries of `hashes` (headers without a `block` row are skipped) and return
/// the headers whose summary was inserted or changed. Callers hold the summary writers' lock in
/// the same transaction. At most six statements whatever the batch size.
pub async fn refresh_relationship_summaries<C: GenericClient>(
    client: &C,
    hashes: &[Vec<u8>],
) -> Result<Vec<Vec<u8>>> {
    let hashes: Vec<&Vec<u8>> = hashes.iter().collect::<HashSet<_>>().into_iter().collect();
    let headers = load_headers(client, &hashes).await?;
    if headers.is_empty() {
        return Ok(Vec::new());
    }
    let found: Vec<&Vec<u8>> = headers.iter().map(|header| &header.hash).collect();
    // Event copies matter only where Core's own coinbase is not complete.
    let incomplete: Vec<&Vec<u8>> = headers
        .iter()
        .filter(|header| header.complete_coinbase.is_none())
        .map(|header| &header.hash)
        .collect();
    let mut coinbases = load_event_coinbases(client, &incomplete).await?;
    let mut children = load_child_signals(client, &found).await?;
    let mut accounts = load_protocol_accounts(client, &found).await?;

    let mut parents = Vec::with_capacity(headers.len());
    for header in headers {
        let copies = coinbases.remove(&header.hash).unwrap_or_default();
        let coinbase = match &header.complete_coinbase {
            Some((script, outputs)) => Some(CoinbaseEvidence {
                script: script.as_deref(),
                outputs: outputs.as_deref(),
                outputs_text: None,
            }),
            None => choose_coinbase_evidence(copies.iter().map(|(id, script, outputs, text)| {
                (
                    *id,
                    CoinbaseEvidence {
                        script: script.as_deref(),
                        outputs: outputs.as_deref(),
                        outputs_text: text.as_deref(),
                    },
                )
            })),
        };
        let mut drafts: Vec<SignalDraft<Option<i64>>> = bitcoin_drafts(coinbase);
        drafts.extend(children.remove(&header.hash).unwrap_or_default());
        drafts.extend(accounts.remove(&header.hash).unwrap_or_default());
        parents.push((header, drafts));
    }
    let pools_by_slug = load_snapshot_pools(client, &parents).await?;
    write_summaries(client, parents, &pools_by_slug).await
}

async fn load_headers<C: GenericClient>(client: &C, hashes: &[&Vec<u8>]) -> Result<Vec<Header>> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }
    Ok(client
        .query(BLOCKS_SQL, &[&hashes])
        .await
        .context("load relationship summary blocks")?
        .iter()
        .map(|row| Header {
            hash: row.get(0),
            header_time: row.get(1),
            height: row.get(2),
            miner: row.get(3),
            complete_coinbase: row.get::<_, bool>(4).then(|| (row.get(5), row.get(6))),
        })
        .collect())
}

async fn load_event_coinbases<C: GenericClient>(
    client: &C,
    hashes: &[&Vec<u8>],
) -> Result<HashMap<Vec<u8>, Vec<EventCoinbase>>> {
    let mut by_hash = HashMap::<Vec<u8>, Vec<EventCoinbase>>::new();
    if hashes.is_empty() {
        return Ok(by_hash);
    }
    for row in client
        .query(EVENT_COINBASES_SQL, &[&hashes])
        .await
        .context("load relationship summary event coinbases")?
    {
        by_hash.entry(row.get(0)).or_default().push((
            row.get(1),
            row.get(2),
            row.get(3),
            row.get(4),
        ));
    }
    Ok(by_hash)
}

async fn load_child_signals<C: GenericClient>(
    client: &C,
    hashes: &[&Vec<u8>],
) -> Result<DraftsByHash> {
    let mut by_hash = DraftsByHash::new();
    for row in client
        .query(CHILD_SIGNALS_SQL, &[&hashes])
        .await
        .context("load relationship summary child signals")?
    {
        by_hash
            .entry(row.get(0))
            .or_default()
            .push(SignalDraft::child(
                row.get(1),
                row.get(2),
                row.get(3),
                &row.get(4),
            ));
    }
    Ok(by_hash)
}

async fn load_protocol_accounts<C: GenericClient>(
    client: &C,
    hashes: &[&Vec<u8>],
) -> Result<DraftsByHash> {
    let mut by_hash = DraftsByHash::new();
    for row in client
        .query(PROTOCOL_ACCOUNTS_SQL, &[&hashes, &DECODER_VERSION])
        .await
        .context("load relationship summary protocol accounts")?
    {
        if let Some(draft) = protocol_account_draft(row.get(1), row.get(2), &row.get(3)) {
            by_hash.entry(row.get(0)).or_default().push(draft);
        }
    }
    Ok(by_hash)
}

/// The `pool` ids the drafts' snapshot slugs resolve to; no statement when they name none.
async fn load_snapshot_pools<C: GenericClient>(
    client: &C,
    parents: &[(Header, Vec<SignalDraft<Option<i64>>>)],
) -> Result<HashMap<String, Option<i64>>> {
    let names_snapshot_pool = parents
        .iter()
        .any(|(_, drafts)| drafts.iter().any(|draft| draft.snapshot_slug().is_some()));
    if !names_snapshot_pool {
        return Ok(HashMap::new());
    }
    Ok(load_pool_ids_by_slug(client)
        .await?
        .into_iter()
        .map(|(slug, id)| (slug, Some(id)))
        .collect())
}

/// Upsert each header's summary, rewriting only rows that differ; returns the headers written.
async fn write_summaries<C: GenericClient>(
    client: &C,
    parents: Vec<(Header, Vec<SignalDraft<Option<i64>>>)>,
    pools_by_slug: &HashMap<String, Option<i64>>,
) -> Result<Vec<Vec<u8>>> {
    let mut hashes = Vec::with_capacity(parents.len());
    let mut times = Vec::with_capacity(parents.len());
    let mut heights = Vec::with_capacity(parents.len());
    let mut mixed = Vec::with_capacity(parents.len());
    let mut pool_ids = Vec::with_capacity(parents.len());
    for (header, drafts) in parents {
        let signals = finish(&header.miner, drafts, pools_by_slug);
        let ids = signals
            .pools
            .iter()
            .filter_map(SignalPool::pool_id)
            .map(|id| id.to_string())
            .collect::<Vec<_>>();
        hashes.push(header.hash);
        times.push(header.header_time);
        heights.push(header.height);
        mixed.push(signals.mixed);
        pool_ids.push(format!("{{{}}}", ids.join(",")));
    }
    Ok(client
        .query(
            "INSERT INTO relationship_summary AS rs \
                (btc_header_hash, btc_header_time, btc_height, mixed, pool_ids) \
             SELECT u.hash, u.header_time, u.height, u.mixed, u.pool_ids::bigint[] \
               FROM unnest($1::bytea[], $2::bigint[], $3::int4[], $4::bool[], $5::text[]) \
                    AS u(hash, header_time, height, mixed, pool_ids) \
              WHERE EXISTS (SELECT 1 FROM block b WHERE b.btc_header_hash = u.hash) \
             ON CONFLICT (btc_header_hash) DO UPDATE SET \
                btc_header_time = EXCLUDED.btc_header_time, btc_height = EXCLUDED.btc_height, \
                mixed = EXCLUDED.mixed, pool_ids = EXCLUDED.pool_ids \
              WHERE (rs.btc_header_time, rs.btc_height, rs.mixed, rs.pool_ids) \
                    IS DISTINCT FROM (EXCLUDED.btc_header_time, EXCLUDED.btc_height, \
                                      EXCLUDED.mixed, EXCLUDED.pool_ids) \
             RETURNING rs.btc_header_hash",
            &[&hashes, &times, &heights, &mixed, &pool_ids],
        )
        .await
        .context("write relationship summaries")?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

/// Queue `hashes` (internal byte order) for a refresh. Only headers with a `block` row can have a
/// summary, so others (near parents, deleted blocks) are not queued.
pub(crate) async fn enqueue_relationship_headers<C: GenericClient>(
    client: &C,
    hashes: &[Vec<u8>],
) -> Result<()> {
    if hashes.is_empty() {
        return Ok(());
    }
    client
        .execute(
            "INSERT INTO relationship_refresh_queue (btc_header_hash) \
             SELECT DISTINCT q.hash FROM unnest($1::bytea[]) AS q(hash) \
              WHERE EXISTS (SELECT 1 FROM block b WHERE b.btc_header_hash = q.hash)",
            &[&hashes],
        )
        .await
        .context("queue relationship summary headers")?;
    Ok(())
}

/// Replace every queued pool identity with the headers it reaches, in one statement so no
/// identity row is lost or expanded twice. Its cost follows the identities' history, so only the
/// scheduled sweep runs it, never the follower's tick. Returns how many headers it queued.
async fn expand_queued_identities(client: &Client) -> Result<u64> {
    client
        .execute(EXPAND_IDENTITIES_SQL, &[])
        .await
        .context("expand queued pool identities into headers")
}

/// Drain up to `max_batches` batches of `batch_size` header rows, each one transaction under the
/// summary writers' lock (skipped while a sweep or rebuild holds it, which also leaves this the
/// only drain running): refresh the rows' headers and delete only those rows, so rows queued
/// meanwhile survive.
pub async fn drain_relationship_refresh_queue(
    client: &mut Client,
    batch_size: i64,
    max_batches: usize,
) -> Result<()> {
    for _ in 0..max_batches {
        let txn = client.transaction().await?;
        if !lock_summaries(&txn, false).await? {
            break;
        }
        let rows = txn
            .query(
                "SELECT id, btc_header_hash FROM relationship_refresh_queue \
                  WHERE btc_header_hash IS NOT NULL ORDER BY id LIMIT $1",
                &[&batch_size],
            )
            .await
            .context("read relationship refresh queue rows")?;
        if rows.is_empty() {
            break;
        }
        let ids: Vec<i64> = rows.iter().map(|row| row.get(0)).collect();
        let hashes: Vec<Vec<u8>> = rows.iter().map(|row| row.get(1)).collect();
        refresh_relationship_summaries(&txn, &hashes).await?;
        txn.execute(
            "DELETE FROM relationship_refresh_queue WHERE id = ANY($1::bigint[])",
            &[&ids],
        )
        .await
        .context("delete drained relationship refresh queue rows")?;
        txn.commit().await?;
    }
    Ok(())
}

/// One scheduled sweep run: expand queued pool identities, drain the queue, then re-derive up to
/// `max_pages` pages of `page_size` blocks from the persisted cursor, counting as drift the rows
/// that differed while no refresh of them was queued. A pass ends when the cursor passes the last
/// block; the next run starts a new one.
pub async fn run_relationship_sweep(
    client: &mut Client,
    page_size: i64,
    max_pages: usize,
) -> Result<RelationshipSweep> {
    expand_queued_identities(client).await?;
    drain_relationship_refresh_queue(client, page_size, max_pages).await?;
    let progress = ProgressReporter::new("sweep-relationship-signals", None);
    let mut report = RelationshipSweep::default();
    for _ in 0..max_pages {
        let txn = client.transaction().await?;
        lock_summaries(&txn, true).await?;
        let cursor: Option<Vec<u8>> = txn
            .query_one(
                "SELECT relationship_sweep_cursor FROM read_model_invariant WHERE id = TRUE",
                &[],
            )
            .await
            .context("read the relationship sweep cursor")?
            .get(0);
        let hashes = block_page(&txn, &cursor.unwrap_or_default(), page_size).await?;
        let changed = refresh_relationship_summaries(&txn, &hashes).await?;
        // Read after the inputs: a change committed meanwhile is queued in the same commit, so
        // a summary it moved never counts as drift.
        if !changed.is_empty() {
            let drifted: i64 = txn
                .query_one(
                    "SELECT count(*) FROM unnest($1::bytea[]) AS c(hash) \
                      WHERE NOT EXISTS (SELECT 1 FROM relationship_refresh_queue q \
                                         WHERE q.btc_header_hash = c.hash)",
                    &[&changed],
                )
                .await
                .context("count unqueued sweep changes")?
                .get(0);
            report.drifted += u64::try_from(drifted)?;
        }
        let next = hashes.last().cloned();
        txn.execute(
            "UPDATE read_model_invariant SET relationship_sweep_cursor = $1 WHERE id = TRUE",
            &[&next],
        )
        .await
        .context("advance the relationship sweep cursor")?;
        txn.commit().await?;
        progress.advance(hashes.len() as u64);
        if next.is_none() {
            report.pass_completed = true;
            break;
        }
    }
    progress.finish();
    if report.drifted > 0 {
        warn!(
            checked = progress.done(),
            drifted = report.drifted,
            "relationship summaries differed from their inputs; a write path did not queue them"
        );
    } else {
        info!(
            checked = progress.done(),
            pass_completed = report.pass_completed,
            "relationship summary sweep found no drift"
        );
    }
    Ok(report)
}

async fn block_page<C: GenericClient>(
    client: &C,
    after: &[u8],
    page_size: i64,
) -> Result<Vec<Vec<u8>>> {
    Ok(client
        .query(BLOCK_PAGE_SQL, &[&after, &page_size])
        .await
        .context("page blocks for relationship summaries")?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

/// The compiled rules a summary depends on: the derivation, the protocol decoder and the pool
/// snapshot. Pool identities live in the database, and their writers queue their headers.
fn rules_fingerprint() -> String {
    let rules = format!("{DERIVATION_VERSION}\n{DECODER_VERSION}\n{DEFAULT_POOL_SNAPSHOT_JSON}");
    sha256::Hash::hash(rules.as_bytes()).to_string()
}

/// Whether `config` asks for an `--if-needed` relationship rebuild while the recorded
/// fingerprint already matches the compiled rules; logs the skip.
pub(crate) async fn skip_relationship_summaries_rebuild(
    client: &Client,
    config: &ReconcileReadModelConfig,
) -> Result<bool> {
    if !(config.rebuild_relationship_signals && config.if_needed) {
        return Ok(false);
    }
    let recorded: Option<String> = client
        .query_one(
            "SELECT relationship_rules_fingerprint FROM read_model_invariant WHERE id = TRUE",
            &[],
        )
        .await
        .context("read the relationship rules fingerprint")?
        .get(0);
    let skip = recorded.as_deref() == Some(rules_fingerprint().as_str());
    if skip {
        info!("relationship summaries already match the compiled rules; skipping rebuild");
    }
    Ok(skip)
}

/// Re-derive every summary, `batch_size` blocks per transaction under the summary writers' lock,
/// leaving queued rows to the drain, and record the fingerprint only once the protocol
/// commitments are at the running decoder (before that, summaries omit declared accounts, as the
/// API does). The command upserts the pool snapshot first so new snapshot slugs resolve.
pub(crate) async fn rebuild_relationship_summaries(
    client: &mut Client,
    batch_size: i64,
) -> Result<()> {
    let progress = ProgressReporter::new("rebuild-relationship-signals", None);
    let mut cursor = Vec::new();
    let mut changed = 0;
    loop {
        let txn = client.transaction().await?;
        lock_summaries(&txn, true).await?;
        let hashes = block_page(&txn, &cursor, batch_size).await?;
        let Some(last) = hashes.last().cloned() else {
            break;
        };
        changed += refresh_relationship_summaries(&txn, &hashes).await?.len();
        txn.commit().await?;
        cursor = last;
        progress.advance(hashes.len() as u64);
    }
    // Only a rebuild that saw declared accounts may claim the rules are current.
    let recorded = client
        .execute(
            "UPDATE read_model_invariant SET relationship_rules_fingerprint = $1, updated_at = $2 \
              WHERE id = TRUE \
                AND protocol_commitments_decoder_version IS NOT DISTINCT FROM $3",
            &[
                &rules_fingerprint(),
                &now_epoch_seconds()?,
                &DECODER_VERSION,
            ],
        )
        .await
        .context("record the relationship rules fingerprint")?;
    if recorded == 0 {
        warn!(
            "protocol commitments are not at the running decoder version; summaries omit declared \
             accounts and the fingerprint stays unrecorded, so the next --if-needed run rebuilds"
        );
    }
    progress.finish();
    info!(
        headers = progress.done(),
        changed, "rebuilt relationship summaries"
    );
    Ok(())
}
