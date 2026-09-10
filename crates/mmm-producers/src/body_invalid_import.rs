//! `import-body-invalid-stales`: load the pinned body-invalid stales mirror
//! into the `body_invalid_stale` annotation table.
//!
//! The mirror (`data/consensus/body_invalid_stales.csv`, generated from the
//! research overlay at the pinned commit) annotates stale blocks whose complete
//! body is known consensus-invalid from external full-block evidence. The
//! annotation is display-only: it is joined at API projection time and never
//! consulted by classification, orphan derivation, or reconciliation, and it
//! must never promote a row to `error_block`. The importer therefore refuses
//! any hash that is ALSO in the pinned error-block catalogue: the overlay and
//! the catalogue are disjoint by construction on the research side, and a
//! violation here means the two pins are out of step.
//!
//! Unlike `import-known-stales` (a 2,000-plus-row upstream dataset with
//! `--skip-malformed` and progress batching), this mirror is a small curated
//! artifact, so the importer is strict always: any malformed row, a missing
//! column, an empty file, or a catalogue overlap is fatal. The mirror is an
//! authoritative snapshot: re-imports replace rows in place AND prune any
//! annotation the newest pin withdrew, so a corrected rule, a corrected
//! evidence URL, or a removed row propagates without an operator delete.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use mmm_capture::capture::now_epoch_seconds;
use mmm_store::{delete_body_invalid_stales_not_in, upsert_body_invalid_stale};
use tokio_postgres::Client;
use tracing::info;

/// Resolved parameters for one `import-body-invalid-stales` run.
#[derive(Debug, Clone)]
pub struct BodyInvalidImportConfig {
    pub csv_path: PathBuf,
    /// Provenance recorded on every imported row (e.g. the mirror name plus
    /// pinned research commit), required so the annotation's origin is
    /// auditable.
    pub source_label: String,
}

impl BodyInvalidImportConfig {
    /// Parse `import-body-invalid-stales --csv PATH --source-label LABEL`.
    /// Both arguments are required.
    pub fn from_args(mut args: std::env::Args) -> Result<Self> {
        let mut csv_path = None;
        let mut source_label = None;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--csv" => {
                    csv_path = Some(
                        args.next()
                            .map(PathBuf::from)
                            .ok_or_else(|| anyhow::anyhow!("--csv requires a path"))?,
                    );
                }
                "--source-label" => {
                    source_label = Some(
                        args.next()
                            .ok_or_else(|| anyhow::anyhow!("--source-label requires a value"))?,
                    );
                }
                "-h" | "--help" => bail!(usage_message()),
                other => bail!(
                    "unknown import-body-invalid-stales argument {other:?}\n{}",
                    usage_message()
                ),
            }
        }
        let source_label: String = source_label.ok_or_else(|| anyhow::anyhow!(usage_message()))?;
        if source_label.trim().is_empty() {
            bail!(
                "--source-label must not be empty or whitespace: it records the mirror's provenance"
            );
        }
        Ok(Self {
            csv_path: csv_path.ok_or_else(|| anyhow::anyhow!(usage_message()))?,
            source_label,
        })
    }
}

fn usage_message() -> &'static str {
    "usage: import-body-invalid-stales --csv PATH --source-label LABEL"
}

/// Running tallies for one `import-body-invalid-stales` run. Counters
/// reconcile: rows_seen = inserted + updated (any other row is fatal), and
/// `removed` counts previously imported annotations the new mirror withdrew.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BodyInvalidImportSummary {
    pub rows_seen: u64,
    pub inserted: u64,
    pub updated: u64,
    pub removed: u64,
}

impl BodyInvalidImportSummary {
    /// Print the one-line operator-facing summary to stdout.
    pub fn print(&self) {
        println!(
            "import-body-invalid-stales: rows_seen={} inserted={} updated={} removed={}",
            self.rows_seen, self.inserted, self.updated, self.removed
        );
    }
}

/// Read the pinned mirror and upsert every annotation into
/// `body_invalid_stale` in one transaction.
///
/// Fatal conditions: an unreadable file, a missing required column, any
/// malformed row, an empty file, and any hash present in the pinned
/// error-block catalogue (the overlay/catalogue disjointness invariant).
/// Idempotent and authoritative: a re-run of the same mirror updates every
/// row in place, and rows absent from the mirror are pruned in the same
/// transaction so a withdrawn annotation stops surfacing.
pub async fn run_import_body_invalid_stales(
    client: &mut Client,
    config: &BodyInvalidImportConfig,
) -> Result<BodyInvalidImportSummary> {
    let file = std::fs::File::open(&config.csv_path)
        .with_context(|| format!("open body-invalid mirror {}", config.csv_path.display()))?;
    let mut reader = csv::ReaderBuilder::new()
        .comment(Some(b'#'))
        .from_reader(file);
    let headers = reader
        .headers()
        .context("read body-invalid mirror header")?;
    for required in ["height", "hash", "rule", "evidence_url"] {
        if !headers.iter().any(|header| header.trim() == required) {
            bail!(
                "body-invalid mirror {} has no '{required}' column; expected the \
                 data/consensus/body_invalid_stales.csv schema",
                config.csv_path.display()
            );
        }
    }
    let imported_at = now_epoch_seconds()?;
    let mut summary = BodyInvalidImportSummary::default();

    // Pass 1: parse and validate every row up front so the write below can be
    // all-or-nothing.
    let mut rows = Vec::new();
    for (index, record) in reader.deserialize::<BTreeMap<String, String>>().enumerate() {
        summary.rows_seen += 1;
        let record =
            record.with_context(|| format!("read body-invalid mirror row {}", index + 1))?;
        let row = parse_row(&record).with_context(|| {
            format!(
                "malformed body-invalid mirror row {} in {}",
                index + 1,
                config.csv_path.display()
            )
        })?;
        if mmm_capture::error_blocks::lookup(&row.hash).is_some() {
            bail!(
                "body-invalid mirror row {} (height {:?}) is also in the pinned error-block \
                 catalogue; the overlay and the catalogue must stay disjoint -- refresh both \
                 pins from one research commit",
                index + 1,
                row.btc_height
            );
        }
        rows.push(row);
    }
    if rows.is_empty() {
        bail!(
            "no annotation rows in {}: refusing to record an empty import",
            config.csv_path.display()
        );
    }

    // Pass 2: one transaction for the whole annotation write.
    let txn = client
        .transaction()
        .await
        .context("begin body-invalid import transaction")?;
    for row in &rows {
        let inserted = upsert_body_invalid_stale(
            &txn,
            &row.hash,
            row.btc_height,
            &row.rule,
            row.evidence_url.as_deref(),
            &config.source_label,
            imported_at,
        )
        .await?;
        if inserted {
            summary.inserted += 1;
        } else {
            summary.updated += 1;
        }
    }
    let keep: Vec<Vec<u8>> = rows.iter().map(|row| row.hash.clone()).collect();
    summary.removed = delete_body_invalid_stales_not_in(&txn, &keep).await?;
    txn.commit()
        .await
        .context("commit body-invalid import transaction")?;
    info!(
        imported = rows.len(),
        "imported body-invalid stale annotations"
    );
    Ok(summary)
}

struct ParsedRow {
    hash: Vec<u8>,
    btc_height: Option<i32>,
    rule: String,
    evidence_url: Option<String>,
}

/// Parse one mirror row. The `hash` column is the display (reversed) hex block
/// hash; `BlockHash::from_str` parses that form and `to_byte_array` yields
/// internal byte order, matching `block.btc_header_hash`. `rule` must be
/// non-empty; `height` and `evidence_url` are advisory, but a NON-EMPTY
/// unparseable height is malformed rather than silently dropped.
fn parse_row(row: &BTreeMap<String, String>) -> Result<ParsedRow> {
    let hash_str = row
        .get("hash")
        .map(|value| value.trim())
        .unwrap_or_default();
    if hash_str.is_empty() {
        bail!("empty hash");
    }
    let hash = BlockHash::from_str(hash_str).context("unparseable hash")?;
    let btc_height = match row
        .get("height")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        Some(raw) => Some(raw.parse::<i32>().context("unparseable height")?),
        None => None,
    };
    let rule = row
        .get("rule")
        .map(|value| value.trim())
        .unwrap_or_default();
    if rule.is_empty() {
        bail!("empty rule");
    }
    let evidence_url = row
        .get("evidence_url")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    Ok(ParsedRow {
        hash: hash.to_byte_array().to_vec(),
        btc_height,
        rule: rule.to_string(),
        evidence_url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_row() -> BTreeMap<String, String> {
        let mut row = BTreeMap::new();
        row.insert("height".to_string(), "783426".to_string());
        row.insert(
            "hash".to_string(),
            "00000000000000000002ec935e245f8ae70fc68cc828f05bf4cfa002668599e4".to_string(),
        );
        row.insert("rule".to_string(), "bad-blk-sigops".to_string());
        row.insert(
            "evidence_url".to_string(),
            "https://b10c.me/observations/11-invalid-blocks-783426-and-784121/".to_string(),
        );
        row
    }

    #[test]
    fn parses_display_hash_to_internal_order() {
        let parsed = parse_row(&valid_row()).expect("valid row");
        assert_eq!(parsed.btc_height, Some(783_426));
        assert_eq!(parsed.rule, "bad-blk-sigops");
        assert_eq!(
            hex::encode(&parsed.hash),
            "e499856602a0cff45bf028c88cc60fe78a5f245e93ec02000000000000000000"
        );
        assert_eq!(parsed.hash.len(), 32);
    }

    #[test]
    fn empty_rule_or_hash_is_malformed() {
        let mut no_rule = valid_row();
        no_rule.insert("rule".to_string(), "  ".to_string());
        assert!(parse_row(&no_rule).is_err());

        let mut no_hash = valid_row();
        no_hash.insert("hash".to_string(), String::new());
        assert!(parse_row(&no_hash).is_err());
    }

    #[test]
    fn missing_evidence_url_is_advisory_none() {
        let mut row = valid_row();
        row.remove("evidence_url");
        let parsed = parse_row(&row).expect("valid row");
        assert_eq!(parsed.evidence_url, None);
    }

    #[test]
    fn nonempty_unparseable_height_is_malformed() {
        let mut row = valid_row();
        row.insert("height".to_string(), "not-a-height".to_string());
        assert!(parse_row(&row).is_err());
    }
}
