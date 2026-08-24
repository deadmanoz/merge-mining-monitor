//! `import-known-stales`: load the upstream stale-blocks dataset into the
//! `known_stale_block` membership table.
//!
//! The membership is consulted at `block.btc_orphan_class` derivation so a known
//! stale is excluded, never labelled strict/weak. This importer reads a
//! stale-blocks.csv-shaped file (`height,hash,header`; the upstream
//! bitcoin-data/stale-blocks schema) and records each hash keyed in rust-bitcoin
//! internal byte order to match `block.btc_header_hash`. Only the `hash` column
//! is authoritative here: the upstream dataset leaves `header` empty for many
//! rows, so the hash (not a re-derivation from the header) is the membership key.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use bitcoin::BlockHash;
use bitcoin::hashes::Hash as _;
use mmm_capture::capture::now_epoch_seconds;
use mmm_store::upsert_known_stale_block;
use tokio_postgres::Client;
use tracing::info;

/// Progress-log cadence (rows) when `--batch-size` is omitted.
const DEFAULT_BATCH_SIZE: usize = 5_000;

/// Resolved parameters for one `import-known-stales` run.
#[derive(Debug, Clone)]
pub struct KnownStaleImportConfig {
    pub csv_path: PathBuf,
    /// Provenance recorded on every imported row (e.g. a dataset name plus
    /// commit), required so the membership's origin is auditable.
    pub source_label: String,
    /// Progress-log cadence in rows processed.
    pub batch_size: usize,
    /// When set, tally malformed rows as skips and import the valid subset.
    /// Default: any malformed row is fatal, so a corrupt or wrong dataset can
    /// never initialize a partial membership.
    pub skip_malformed: bool,
}

impl KnownStaleImportConfig {
    /// Parse `import-known-stales --csv PATH --source-label LABEL [--batch-size N]`.
    /// Both `--csv` and `--source-label` are required.
    pub fn from_args(mut args: std::env::Args) -> Result<Self> {
        let mut csv_path = None;
        let mut source_label = None;
        let mut batch_size = DEFAULT_BATCH_SIZE;
        let mut skip_malformed = false;
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
                "--batch-size" => {
                    let value = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("--batch-size requires a value"))?;
                    batch_size = value
                        .parse()
                        .context("--batch-size must be a positive integer")?;
                    if batch_size == 0 {
                        bail!("--batch-size must be greater than zero");
                    }
                }
                "--skip-malformed" => skip_malformed = true,
                "-h" | "--help" => bail!(usage_message()),
                other => bail!(
                    "unknown import-known-stales argument {other:?}\n{}",
                    usage_message()
                ),
            }
        }
        let source_label: String = source_label.ok_or_else(|| anyhow::anyhow!(usage_message()))?;
        if source_label.trim().is_empty() {
            bail!(
                "--source-label must not be empty or whitespace: it records the dataset's provenance"
            );
        }
        Ok(Self {
            csv_path: csv_path.ok_or_else(|| anyhow::anyhow!(usage_message()))?,
            source_label,
            batch_size,
            skip_malformed,
        })
    }
}

fn usage_message() -> &'static str {
    "usage: import-known-stales --csv PATH --source-label LABEL [--batch-size N] [--skip-malformed]"
}

/// Running tallies for one `import-known-stales` run. `rows_seen` counts every
/// CSV record; `inserted` new membership rows; `already_present` idempotent
/// re-imports; `skipped` malformed/empty-hash rows. Counters reconcile:
/// rows_seen = inserted + already_present + skipped.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KnownStaleImportSummary {
    pub rows_seen: u64,
    pub inserted: u64,
    pub already_present: u64,
    pub skipped: u64,
}

impl KnownStaleImportSummary {
    /// Print the one-line operator-facing summary to stdout.
    pub fn print(&self) {
        println!(
            "import-known-stales: rows_seen={} inserted={} already_present={} skipped={}",
            self.rows_seen, self.inserted, self.already_present, self.skipped
        );
    }
}

/// Read the stale-blocks CSV and upsert every hash into `known_stale_block`
/// in one transaction, then repair any existing strict/weak classifications.
///
/// Malformed rows (missing/unparseable `hash`) are FATAL by default, as are a
/// missing `hash` column and a file with no usable row: downstream guards
/// only test membership emptiness, so a corrupt or wrong dataset must never
/// count as initialized while silently omitting known stales.
/// `--skip-malformed` opts into tallying malformed rows as skips and
/// importing the valid subset. The membership write is atomic: all parsed rows commit
/// together while holding the per-parent advisory locks (acquired through the
/// read-model's sorted, deduped `lock_block_hashes`, honoring the global lock
/// order), so the import serializes with any in-flight classification of the
/// same hashes AND a mid-import failure records nothing, never a partial
/// membership that downstream empty-membership guards would treat as
/// complete. Idempotent: a re-run records every already-present hash under
/// `already_present` and inserts none.
pub async fn run_import_known_stales(
    client: &mut Client,
    config: &KnownStaleImportConfig,
) -> Result<KnownStaleImportSummary> {
    let file = std::fs::File::open(&config.csv_path)
        .with_context(|| format!("open stale-blocks CSV {}", config.csv_path.display()))?;
    let mut reader = csv::Reader::from_reader(file);
    let headers = reader.headers().context("read stale-blocks CSV header")?;
    if !headers.iter().any(|header| header.trim() == "hash") {
        bail!(
            "stale-blocks CSV {} has no 'hash' column; expected the upstream \
             stale-blocks.csv schema",
            config.csv_path.display()
        );
    }
    let imported_at = now_epoch_seconds()?;
    let mut summary = KnownStaleImportSummary::default();

    // Pass 1: parse every row up front so the write below can be all-or-nothing.
    let mut rows = Vec::new();
    for row in reader.deserialize::<BTreeMap<String, String>>() {
        summary.rows_seen += 1;
        let row = match row {
            Ok(row) => row,
            Err(_) => {
                summary.skipped += 1;
                continue;
            }
        };
        let Some((hash_bytes, height)) = parse_row(&row) else {
            summary.skipped += 1;
            continue;
        };
        rows.push((hash_bytes, height));
    }
    if summary.skipped > 0 && !config.skip_malformed {
        bail!(
            "{} malformed row(s) in {} ({} rows seen): a corrupt or wrong dataset must not \
             initialize the membership; fix the file or pass --skip-malformed to import only \
             the valid subset",
            summary.skipped,
            config.csv_path.display(),
            summary.rows_seen
        );
    }
    if rows.is_empty() {
        bail!(
            "no valid membership rows in {} ({} rows seen, {} skipped): refusing to \
             record an empty import",
            config.csv_path.display(),
            summary.rows_seen,
            summary.skipped
        );
    }

    // Pass 2: one transaction for the whole membership write.
    let txn = client
        .transaction()
        .await
        .context("begin known-stale import transaction")?;
    let hashes: Vec<Vec<u8>> = rows.iter().map(|(hash, _)| hash.clone()).collect();
    mmm_read_model::lock_block_hashes(&txn, &hashes).await?;
    for (index, (hash_bytes, height)) in rows.iter().enumerate() {
        let inserted =
            upsert_known_stale_block(&txn, hash_bytes, *height, &config.source_label, imported_at)
                .await?;
        if inserted {
            summary.inserted += 1;
        } else {
            summary.already_present += 1;
        }
        if (index + 1).is_multiple_of(config.batch_size) {
            info!(
                processed = index + 1,
                inserted = summary.inserted,
                "import-known-stales progress"
            );
        }
    }
    txn.commit()
        .await
        .context("commit known-stale import transaction")?;
    let repair = mmm_read_model::run_reclassify_known_stales(
        client,
        mmm_read_model::ReclassifyKnownStalesConfig::default(),
    )
    .await
    .context("repair known-stale classifications after membership import")?;
    info!(
        membership_size = repair.membership_size,
        demoted = repair.demoted,
        "repaired known-stale classifications after membership import"
    );
    Ok(summary)
}

/// Parse one row into `(hash_internal_bytes, height)`. The `hash` column is the
/// display (reversed) hex block hash; `BlockHash::from_str` parses that form and
/// `to_byte_array` yields internal byte order, matching `block.btc_header_hash`.
/// `height` is advisory when present (`None` when the column is absent or
/// empty), but a NON-EMPTY unparseable height makes the whole row malformed
/// (`None` return) rather than silently degrading to no height: the strict
/// default documents that any malformed row aborts the import, and corrupt
/// provenance must not be quietly dropped. Returns `None` for a missing or
/// malformed hash as well.
fn parse_row(row: &BTreeMap<String, String>) -> Option<(Vec<u8>, Option<i32>)> {
    let hash_str = row
        .get("hash")
        .map(|value| value.trim())
        .unwrap_or_default();
    if hash_str.is_empty() {
        return None;
    }
    let hash = BlockHash::from_str(hash_str).ok()?;
    let height = match row
        .get("height")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
    {
        Some(raw) => Some(raw.parse::<i32>().ok()?),
        None => None,
    };
    Some((hash.to_byte_array().to_vec(), height))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_display_hash_to_internal_order() {
        // Real stale block 363736: its display hash reverses to this internal
        // byte order, which is what block.btc_header_hash stores.
        let mut row = BTreeMap::new();
        row.insert("height".to_string(), "363736".to_string());
        row.insert(
            "hash".to_string(),
            "000000000000000013fe26675faa8f7dccd55ce5485bb6d0373fa66345901436".to_string(),
        );
        let (hash, height) = parse_row(&row).expect("valid row");
        assert_eq!(height, Some(363_736));
        assert_eq!(
            hex::encode(&hash),
            "3614904563a63f37d0b65b48e55cd5cc7d8faa5f6726fe130000000000000000"
        );
        assert_eq!(hash.len(), 32);
    }

    #[test]
    fn empty_or_bad_hash_is_skipped() {
        let mut empty = BTreeMap::new();
        empty.insert("height".to_string(), "1".to_string());
        empty.insert("hash".to_string(), "".to_string());
        assert!(parse_row(&empty).is_none());

        let mut bad = BTreeMap::new();
        bad.insert("hash".to_string(), "not-a-hash".to_string());
        assert!(parse_row(&bad).is_none());
    }

    #[test]
    fn missing_height_is_advisory_none() {
        let mut row = BTreeMap::new();
        row.insert(
            "hash".to_string(),
            "000000000000000013fe26675faa8f7dccd55ce5485bb6d0373fa66345901436".to_string(),
        );
        let (_, height) = parse_row(&row).expect("valid row");
        assert_eq!(height, None);
    }
}
