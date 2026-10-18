//! `import-known-stales`: load the upstream stale-blocks dataset into
//! `known_stale_block` keyed in internal byte order.

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

const DEFAULT_BATCH_SIZE: usize = 5_000;

#[derive(Debug, Clone)]
pub struct KnownStaleImportConfig {
    pub csv_path: PathBuf,
    pub source_label: String,
    pub batch_size: usize,
    pub skip_malformed: bool,
}

impl KnownStaleImportConfig {
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KnownStaleImportSummary {
    pub rows_seen: u64,
    pub inserted: u64,
    pub already_present: u64,
    pub skipped: u64,
}

impl KnownStaleImportSummary {
    pub fn print(&self) {
        println!(
            "import-known-stales: rows_seen={} inserted={} already_present={} skipped={}",
            self.rows_seen, self.inserted, self.already_present, self.skipped
        );
    }
}

/// Upsert every stale-blocks CSV hash into `known_stale_block` and demote any
/// existing strict/weak classifications atomically under sorted per-parent
/// advisory locks. Malformed rows fail the import unless `--skip-malformed` is set.
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
    let demoted = mmm_read_model::reclassify_known_stale_hashes_in_transaction(&txn, &hashes)
        .await
        .context("repair known-stale classifications in import transaction")?;
    txn.commit()
        .await
        .context("commit known-stale import transaction")?;
    info!(
        imported = rows.len(),
        demoted, "atomically repaired known-stale classifications during membership import"
    );
    Ok(summary)
}

/// Parse display-hex `hash` into internal byte order and optional `height`
/// (a non-empty unparseable height rejects the row).
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
