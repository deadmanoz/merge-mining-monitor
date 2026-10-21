//! `protocol_commitment` derived rows: the CORE and exSat declarations decoded from the
//! highest-priority complete coinbase outputs stored for each Bitcoin header (Core's `block`
//! coinbase, else the lowest-id active AuxPoW event's parent coinbase, else the lowest-id near
//! observation's). Inline callers hold the header's read-model lock or the canonical-view
//! barrier; the rebuild pages under the exclusive barrier.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use bitcoin::{Amount, ScriptBuf, TxOut};
use tokio_postgres::{Client, GenericClient};
use tracing::{info, warn};

use mmm_capture::progress::ProgressReporter;
use mmm_capture::protocol_commitment::{
    CommitmentPayload, DECODER_VERSION, DecodeStatus, ProtocolCommitment,
    decode_protocol_commitments, decode_serialized_protocol_commitments,
};

use crate::mutation::run_exclusive_core_canonical_view_transaction;

/// Each requested header's highest-priority complete outputs and header time.
const EVIDENCE_SQL: &str = "\
    SELECT h.hash, e.evidence_source, e.outputs, e.header_time \
      FROM unnest($1::bytea[]) AS h(hash) \
      JOIN LATERAL ( \
           SELECT 'bitcoin_core' AS evidence_source, b.btc_coinbase_outputs AS outputs, \
                  b.btc_header_time AS header_time, 0 AS priority \
             FROM block b \
            WHERE b.btc_header_hash = h.hash AND b.btc_coinbase_outputs IS NOT NULL \
           UNION ALL ( \
           SELECT 'auxpow_event', e.btc_parent_coinbase_outputs, e.btc_parent_header_time, 1 \
             FROM merge_mining_event e \
            WHERE e.btc_parent_header_hash = h.hash AND e.revoked_at IS NULL \
              AND e.btc_parent_coinbase_outputs IS NOT NULL \
            ORDER BY e.id LIMIT 1) \
           UNION ALL ( \
           SELECT 'near_observation', o.btc_parent_coinbase_outputs, o.btc_parent_header_time, 2 \
             FROM child_block_observation o \
            WHERE o.btc_parent_header_hash = h.hash AND o.observation_kind = 'near' \
              AND o.btc_parent_coinbase_outputs IS NOT NULL \
            ORDER BY o.id LIMIT 1) \
           ORDER BY priority LIMIT 1 \
      ) e ON TRUE";

const SCRIPT_COLUMNS: &str = "protocol, decode_status, malformed_reason, version, \
     core_validator, core_reward_address, core_binding_hash, exsat_account";

fn insert_scripts_sql(on_conflict: &str) -> String {
    format!(
        "INSERT INTO protocol_commitment_script AS s (script, {SCRIPT_COLUMNS}) \
         SELECT * FROM unnest($1::bytea[], $2::text[], $3::text[], $4::text[], $5::int2[], \
                              $6::bytea[], $7::bytea[], $8::bytea[], $9::text[]) \
         ON CONFLICT (script_sha256) {on_conflict}"
    )
}

fn script_columns(qualifier: &str) -> String {
    SCRIPT_COLUMNS
        .split(',')
        .map(|column| format!("{qualifier}.{}", column.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Inline writers add only scripts the dimension lacks, never locking existing rows.
static INSERT_SCRIPTS_SQL: LazyLock<String> = LazyLock::new(|| insert_scripts_sql("DO NOTHING"));

/// The rebuild also rewrites each script its pages reach with the compiled decoder.
static REDERIVE_SCRIPTS_SQL: LazyLock<String> = LazyLock::new(|| {
    let decoded = script_columns("EXCLUDED");
    insert_scripts_sql(&format!(
        "DO UPDATE SET ({SCRIPT_COLUMNS}) = ROW({decoded}) \
         WHERE ({}) IS DISTINCT FROM ({decoded})",
        script_columns("s")
    ))
});

/// Make the requested headers' fact and undecodable rows equal the decoded result. Runs after
/// the script insert, so its snapshot sees every script it joins.
const SYNC_SQL: &str = "\
    WITH facts AS ( \
         SELECT s.id AS script_id, f.* \
           FROM unnest($2::bytea[], $3::int4[], $4::bytea[], $5::bool[], $6::int8[], \
                       $7::text[]) \
                AS f(btc_header_hash, vout, script, upstream_selected, btc_header_time, \
                     evidence_source) \
           JOIN protocol_commitment_script s \
             ON s.script_sha256 = sha256(f.script) AND s.script = f.script \
    ), undecodable AS ( \
         SELECT * FROM unnest($8::bytea[], $9::text[], $10::text[]) \
                AS u(btc_header_hash, evidence_source, error) \
    ), deleted_facts AS ( \
         DELETE FROM protocol_commitment c \
          WHERE c.btc_header_hash = ANY($1::bytea[]) \
            AND NOT EXISTS (SELECT 1 FROM facts f \
                             WHERE f.btc_header_hash = c.btc_header_hash AND f.vout = c.vout) \
    ), upserted_facts AS ( \
         INSERT INTO protocol_commitment \
                (btc_header_hash, vout, script_id, upstream_selected, btc_header_time, \
                 evidence_source) \
         SELECT btc_header_hash, vout, script_id, upstream_selected, btc_header_time, \
                evidence_source \
           FROM facts \
         ON CONFLICT (btc_header_hash, vout) DO UPDATE \
            SET script_id = EXCLUDED.script_id, \
                upstream_selected = EXCLUDED.upstream_selected, \
                btc_header_time = EXCLUDED.btc_header_time, \
                evidence_source = EXCLUDED.evidence_source \
          WHERE (protocol_commitment.script_id, protocol_commitment.upstream_selected, \
                 protocol_commitment.btc_header_time, protocol_commitment.evidence_source) \
                IS DISTINCT FROM \
                (EXCLUDED.script_id, EXCLUDED.upstream_selected, EXCLUDED.btc_header_time, \
                 EXCLUDED.evidence_source) \
    ), deleted_undecodable AS ( \
         DELETE FROM protocol_commitment_undecodable d \
          WHERE d.btc_header_hash = ANY($1::bytea[]) \
            AND NOT EXISTS (SELECT 1 FROM undecodable u \
                             WHERE u.btc_header_hash = d.btc_header_hash) \
    ) \
    INSERT INTO protocol_commitment_undecodable (btc_header_hash, evidence_source, error) \
    SELECT btc_header_hash, evidence_source, error FROM undecodable \
    ON CONFLICT (btc_header_hash) DO UPDATE \
       SET evidence_source = EXCLUDED.evidence_source, error = EXCLUDED.error \
     WHERE (protocol_commitment_undecodable.evidence_source, \
            protocol_commitment_undecodable.error) \
           IS DISTINCT FROM (EXCLUDED.evidence_source, EXCLUDED.error)";

/// One rebuild page: the next distinct headers that have a `block` row, a near observation
/// with outputs, or derived rows. Refresh deletes the derived rows of a header that no longer
/// has evidence.
const REBUILD_PAGE_SQL: &str = "\
    SELECT hash FROM ( \
           (SELECT btc_header_hash AS hash FROM block \
             WHERE btc_header_hash > $1 ORDER BY 1 LIMIT $2) \
           UNION \
           (SELECT DISTINCT btc_parent_header_hash FROM child_block_observation \
             WHERE observation_kind = 'near' AND btc_parent_header_hash > $1 \
               AND btc_parent_coinbase_outputs IS NOT NULL \
             ORDER BY 1 LIMIT $2) \
           UNION \
           (SELECT btc_header_hash FROM protocol_commitment \
             WHERE btc_header_hash > $1 ORDER BY 1 LIMIT $2) \
           UNION \
           (SELECT btc_header_hash FROM protocol_commitment_undecodable \
             WHERE btc_header_hash > $1 ORDER BY 1 LIMIT $2) \
    ) headers ORDER BY hash LIMIT $2";

/// `protocol_commitment_script` columns for a batch of decoded scripts.
#[derive(Default)]
struct ScriptColumns {
    script: Vec<Vec<u8>>,
    protocol: Vec<&'static str>,
    decode_status: Vec<&'static str>,
    malformed_reason: Vec<Option<&'static str>>,
    version: Vec<Option<i16>>,
    core_validator: Vec<Option<Vec<u8>>>,
    core_reward_address: Vec<Option<Vec<u8>>>,
    core_binding_hash: Vec<Option<Vec<u8>>>,
    exsat_account: Vec<Option<String>>,
}

impl ScriptColumns {
    fn push(&mut self, commitment: &ProtocolCommitment) {
        let (validator, reward_address, binding_hash, account) = match &commitment.status {
            DecodeStatus::Valid(CommitmentPayload::Core {
                validator,
                reward_address,
                binding_hash,
            }) => (
                Some(validator.to_vec()),
                Some(reward_address.to_vec()),
                binding_hash.map(|hash| hash.to_vec()),
                None,
            ),
            DecodeStatus::Valid(CommitmentPayload::Exsat { account }) => {
                (None, None, None, Some(account.clone()))
            }
            DecodeStatus::Malformed(_) | DecodeStatus::UnsupportedVersion(_) => {
                (None, None, None, None)
            }
        };
        self.script.push(commitment.script.clone());
        self.protocol.push(commitment.protocol.as_db_str());
        self.decode_status.push(commitment.status.as_db_str());
        self.malformed_reason.push(match commitment.status {
            DecodeStatus::Malformed(reason) => Some(reason.as_db_str()),
            _ => None,
        });
        self.version.push(commitment.version().map(i16::from));
        self.core_validator.push(validator);
        self.core_reward_address.push(reward_address);
        self.core_binding_hash.push(binding_hash);
        self.exsat_account.push(account);
    }

    async fn insert<C: GenericClient>(&self, client: &C, sql: &str) -> Result<()> {
        client
            .execute(
                sql,
                &[
                    &self.script,
                    &self.protocol,
                    &self.decode_status,
                    &self.malformed_reason,
                    &self.version,
                    &self.core_validator,
                    &self.core_reward_address,
                    &self.core_binding_hash,
                    &self.exsat_account,
                ],
            )
            .await
            .context("insert protocol commitment scripts")?;
        Ok(())
    }
}

/// Fact and undecodable rows for a batch of headers.
#[derive(Default)]
struct DerivedRows {
    fact_hash: Vec<Vec<u8>>,
    fact_vout: Vec<i32>,
    fact_script: Vec<Vec<u8>>,
    fact_upstream_selected: Vec<Option<bool>>,
    fact_header_time: Vec<i64>,
    fact_evidence_source: Vec<String>,
    undecodable_hash: Vec<Vec<u8>>,
    undecodable_evidence_source: Vec<String>,
    undecodable_error: Vec<String>,
    /// Each distinct script once, for the dimension insert.
    scripts: BTreeMap<Vec<u8>, ProtocolCommitment>,
}

/// Make the derived rows of the distinct `hashes` equal a fresh decode of their stored
/// evidence: an evidence select, a script insert when any declaration was decoded, and a sync.
pub(crate) async fn refresh_protocol_commitments<C: GenericClient>(
    client: &C,
    hashes: &[Vec<u8>],
) -> Result<()> {
    refresh(client, hashes, &INSERT_SCRIPTS_SQL).await
}

async fn refresh<C: GenericClient>(
    client: &C,
    hashes: &[Vec<u8>],
    insert_scripts_sql: &str,
) -> Result<()> {
    let rows = client
        .query(EVIDENCE_SQL, &[&hashes])
        .await
        .context("load protocol commitment evidence")?;
    let mut derived = DerivedRows::default();
    for row in rows {
        let hash: Vec<u8> = row.get(0);
        let evidence_source: String = row.get(1);
        let outputs: &[u8] = row.get(2);
        match decode_serialized_protocol_commitments(outputs) {
            Ok(commitments) => {
                for commitment in commitments {
                    derived.fact_hash.push(hash.clone());
                    derived
                        .fact_vout
                        .push(i32::try_from(commitment.vout).context("commitment vout")?);
                    derived.fact_script.push(commitment.script.clone());
                    derived
                        .fact_upstream_selected
                        .push(commitment.upstream_selected);
                    derived.fact_header_time.push(row.get(3));
                    derived.fact_evidence_source.push(evidence_source.clone());
                    derived
                        .scripts
                        .entry(commitment.script.clone())
                        .or_insert(commitment);
                }
            }
            Err(err) => {
                warn!(
                    btc_header_hash = %hex::encode(&hash),
                    evidence_source,
                    error = %err,
                    "stored coinbase outputs do not deserialize; recording the header as undecodable"
                );
                derived.undecodable_hash.push(hash);
                derived.undecodable_evidence_source.push(evidence_source);
                derived.undecodable_error.push(err.to_string());
            }
        }
    }

    if !derived.scripts.is_empty() {
        let mut columns = ScriptColumns::default();
        for commitment in derived.scripts.values() {
            columns.push(commitment);
        }
        columns.insert(client, insert_scripts_sql).await?;
    }
    client
        .execute(
            SYNC_SQL,
            &[
                &hashes,
                &derived.fact_hash,
                &derived.fact_vout,
                &derived.fact_script,
                &derived.fact_upstream_selected,
                &derived.fact_header_time,
                &derived.fact_evidence_source,
                &derived.undecodable_hash,
                &derived.undecodable_evidence_source,
                &derived.undecodable_error,
            ],
        )
        .await
        .context("sync protocol commitment rows")?;
    Ok(())
}

/// Re-derive every stored script, then refresh every header with evidence or derived rows, a
/// page per exclusive canonical-view hold (rewriting each script those pages reach, so scripts
/// inserted meanwhile are covered too), and record the decoder version once the walk finishes.
pub(crate) async fn rebuild_protocol_commitments(
    client: &mut Client,
    batch_size: i64,
) -> Result<()> {
    rederive_stored_scripts(client, batch_size).await?;
    let progress = ProgressReporter::new("rebuild-protocol-commitments", None);
    let mut cursor: Vec<u8> = Vec::new();
    loop {
        let page = run_exclusive_core_canonical_view_transaction(
            client,
            "rebuild protocol commitments",
            async |txn| {
                let hashes: Vec<Vec<u8>> = txn
                    .query(REBUILD_PAGE_SQL, &[&cursor, &batch_size])
                    .await
                    .context("page protocol commitment headers")?
                    .iter()
                    .map(|row| row.get(0))
                    .collect();
                if !hashes.is_empty() {
                    refresh(txn, &hashes, &REDERIVE_SCRIPTS_SQL).await?;
                }
                Ok(hashes)
            },
        )
        .await?;
        let Some(last) = page.last() else {
            break;
        };
        cursor = last.clone();
        progress.advance(page.len() as u64);
    }

    let now = mmm_capture::capture::now_epoch_seconds()?;
    client
        .execute(
            "UPDATE read_model_invariant \
                SET protocol_commitments_decoder_version = $1, updated_at = $2 \
              WHERE id = TRUE",
            &[&DECODER_VERSION, &now],
        )
        .await
        .context("record the protocol commitment decoder version")?;
    progress.finish();
    let undecodable: i64 = client
        .query_one("SELECT count(*) FROM protocol_commitment_undecodable", &[])
        .await
        .context("count undecodable protocol commitment headers")?
        .get(0);
    info!(
        headers = progress.done(),
        undecodable_headers = undecodable,
        decoder_version = DECODER_VERSION,
        "rebuilt protocol commitments"
    );
    Ok(())
}

/// Rewrite every stored script with the compiled decoder, a page at a time and outside the
/// barrier: inline refreshes only insert scripts the dimension lacks. This reaches scripts no
/// fact refers to, which the header walk cannot, before a later coinbase declares them again.
async fn rederive_stored_scripts(client: &Client, batch_size: i64) -> Result<()> {
    let mut cursor = 0_i64;
    loop {
        let rows = client
            .query(
                "SELECT id, script FROM protocol_commitment_script \
                  WHERE id > $1 ORDER BY id LIMIT $2",
                &[&cursor, &batch_size],
            )
            .await
            .context("page protocol commitment scripts")?;
        let Some(last) = rows.last() else {
            return Ok(());
        };
        cursor = last.get(0);
        let mut columns = ScriptColumns::default();
        for row in &rows {
            let output = TxOut {
                value: Amount::ZERO,
                script_pubkey: ScriptBuf::from_bytes(row.get(1)),
            };
            if let Some(commitment) = decode_protocol_commitments(&[output]).first() {
                columns.push(commitment);
            }
        }
        if !columns.script.is_empty() {
            columns.insert(client, &REDERIVE_SCRIPTS_SQL).await?;
        }
    }
}
