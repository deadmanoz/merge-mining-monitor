//! Cache-backed Hathor historical ingest (`backfill-hathor-cache`).
//!
//! Streams the joined merge-mining-research archive CSV
//! (`hathor_height,hathor_block_hash,hathor_timestamp,funds_graph_hex,aux_pow_hex`,
//! height-sorted, one row per best-chain merge-mined block) and drives every
//! row through the UNCHANGED self-verifying capture path
//! ([`process_hathor_height`]: reconstruct, sha256d hash guard, nBits verdict)
//! via a cache-backed [`HathorRpc`]. This is the Hathor analog of the Elastos
//! producer-backfill-with-oracle pattern: the archive is untrusted input and
//! the public REST API is never called.
//!
//! Error policy is two-tier:
//!
//! - ADAPTER-LEVEL (fatal): structural archive corruption - a wrong header,
//!   short/long rows, non-hex bytes, out-of-order or duplicate heights - is
//!   detected by [`CacheReader`] in the row loop BEFORE the capture call
//!   and aborts the run with the offending line number. The [`HathorRpc`]
//!   impl serves pre-validated rows infallibly and never returns `Err`,
//!   because [`process_hathor_height`] deliberately converts RPC errors into
//!   hold outcomes that would silently swallow corruption.
//! - CAPTURE-LEVEL (counted): rows that validate structurally but fail the capture
//!   path's own reconstruct/guard checks return the ordinary observable
//!   [`HathorHeightOutcome`] variants, exactly as a live REST response would.
//!   The dominant expected bucket is `MalformedSkipped`: sub-BTC-target near
//!   shares (the normal case for ~all merge-mined blocks) return from
//!   reconstruction before the nBits verdict.
//!
//! Heights with no archive row are counted as `absent_heights` by the runner
//! itself (compact ranges in the skip ledger); the capture path is not driven
//! for them, because its `Ok(None)` semantic is a transient *hold*, not a
//! definitive "no merge-mined block here".

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use tokio_postgres::Client;
use tracing::{info, warn};

use crate::chains::hathor::capture::{
    HathorCaptureContext, HathorHeightOutcome, process_hathor_height,
};
use crate::chains::hathor::rpc::{HathorBlockMeta, HathorRpc, HathorTransaction};
use crate::producer_runtime::run_post_backfill_repair;

/// The exact header line the joined archive CSV must carry. A different header
/// means the wrong file (e.g. the raw two-CSV inputs) was passed; fatal.
pub const CACHE_CSV_HEADER: &str =
    "hathor_height,hathor_block_hash,hathor_timestamp,funds_graph_hex,aux_pow_hex";

/// Hathor merge-mined block version (`HathorBlockMeta.version`); the capture
/// state machine treats every other version as non-merge-mined. Every archive
/// row carries an aux_pow, so the constant is uniform for cache rows.
const HATHOR_MERGE_MINED_VERSION: i32 = 3;

/// One validated archive row.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheRow {
    height: i32,
    /// Hathor block hash, 64 display-hex chars (equals the BTC parent hash).
    hash: String,
    timestamp: i64,
    /// The funds||graph prefix hex; concatenated with `aux_pow_hex` it is the
    /// byte-exact live REST `raw` field.
    funds_graph_hex: String,
    aux_pow_hex: String,
}

impl CacheRow {
    /// Synthesize the capture-path inputs. `raw` is the byte-exact
    /// concatenation `funds_graph_hex || aux_pow_hex` (the live REST `raw`
    /// field), verified against the committed fixtures in the unit tests.
    /// `is_voided: false` is correct because the extraction walked the
    /// best chain.
    fn capture_inputs(&self) -> (HathorBlockMeta, HathorTransaction) {
        let meta = HathorBlockMeta {
            tx_id: self.hash.clone(),
            version: HATHOR_MERGE_MINED_VERSION,
            is_voided: false,
        };
        let tx = HathorTransaction {
            raw: format!("{}{}", self.funds_graph_hex, self.aux_pow_hex),
            aux_pow: Some(self.aux_pow_hex.clone()),
            hash: self.hash.clone(),
            timestamp: self.timestamp,
        };
        (meta, tx)
    }
}

/// True iff `value` is non-empty, even-length, all-ASCII-hex (a decodable byte
/// payload); used to reject corrupt funds_graph/aux_pow archive columns.
fn is_hex_payload(value: &str) -> bool {
    !value.is_empty()
        && value.len().is_multiple_of(2)
        && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Streaming, validating reader over the joined archive CSV. Every yielded row
/// is structurally valid and strictly height-ascending; any violation is a
/// fatal error carrying the 1-based line number.
struct CacheReader<R: BufRead> {
    reader: R,
    line_no: usize,
    header_seen: bool,
    prev_height: Option<i32>,
    buf: String,
}

impl<R: BufRead> CacheReader<R> {
    /// Wrap a buffered reader; the header is validated lazily on the first
    /// [`Self::next_row`] call.
    fn new(reader: R) -> Self {
        Self {
            reader,
            line_no: 0,
            header_seen: false,
            prev_height: None,
            buf: String::new(),
        }
    }

    /// Next validated row, or `None` at clean EOF.
    fn next_row(&mut self) -> Result<Option<CacheRow>> {
        loop {
            self.buf.clear();
            let read = self
                .reader
                .read_line(&mut self.buf)
                .with_context(|| format!("read archive CSV line {}", self.line_no + 1))?;
            if read == 0 {
                ensure!(self.header_seen, "archive CSV is empty (no header)");
                return Ok(None);
            }
            self.line_no += 1;
            let line = self.buf.trim_end_matches(['\r', '\n']);
            if !self.header_seen {
                ensure!(
                    line == CACHE_CSV_HEADER,
                    "archive CSV line 1: header {line:?} does not match {CACHE_CSV_HEADER:?}; \
                     is this the joined cache file?"
                );
                self.header_seen = true;
                continue;
            }
            if line.is_empty() {
                // A blank line is legal only as the file's very last line (a
                // trailing newline). Anywhere else it is structural corruption
                // (e.g. a truncated row) and must not silently vanish from the
                // row-count identities.
                let blank_line_no = self.line_no;
                self.buf.clear();
                let more = self
                    .reader
                    .read_line(&mut self.buf)
                    .with_context(|| format!("read archive CSV line {}", blank_line_no + 1))?;
                ensure!(
                    more == 0,
                    "archive CSV line {blank_line_no}: blank line inside the archive \
                     (truncated row?); fix the archive join"
                );
                return Ok(None);
            }
            let row = parse_cache_row(self.line_no, line)?;
            if let Some(prev) = self.prev_height {
                ensure!(
                    row.height > prev,
                    "archive CSV line {}: height {} is not strictly greater than the previous \
                     height {} (out-of-order or duplicate row; fix the archive join)",
                    self.line_no,
                    row.height,
                    prev
                );
            }
            self.prev_height = Some(row.height);
            return Ok(Some(row));
        }
    }
}

/// Parse and structurally validate one CSV data line into a [`CacheRow`]:
/// exactly 5 columns, non-negative height, 64-hex block hash, positive
/// timestamp, even-length-hex payloads. Errors carry the 1-based line number.
fn parse_cache_row(line_no: usize, line: &str) -> Result<CacheRow> {
    let fields: Vec<&str> = line.split(',').collect();
    ensure!(
        fields.len() == 5,
        "archive CSV line {line_no}: expected 5 columns, got {}",
        fields.len()
    );
    let height: i32 = fields[0]
        .parse()
        .with_context(|| format!("archive CSV line {line_no}: hathor_height {:?}", fields[0]))?;
    ensure!(
        height >= 0,
        "archive CSV line {line_no}: negative hathor_height {height}"
    );
    let hash = fields[1].to_owned();
    ensure!(
        hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "archive CSV line {line_no}: hathor_block_hash is not 64 hex chars"
    );
    let timestamp: i64 = fields[2].parse().with_context(|| {
        format!(
            "archive CSV line {line_no}: hathor_timestamp {:?}",
            fields[2]
        )
    })?;
    ensure!(
        timestamp > 0,
        "archive CSV line {line_no}: non-positive hathor_timestamp {timestamp}"
    );
    ensure!(
        is_hex_payload(fields[3]),
        "archive CSV line {line_no}: funds_graph_hex is empty or not even-length hex"
    );
    ensure!(
        is_hex_payload(fields[4]),
        "archive CSV line {line_no}: aux_pow_hex is empty or not even-length hex"
    );
    Ok(CacheRow {
        height,
        hash,
        timestamp,
        funds_graph_hex: fields[3].to_owned(),
        aux_pow_hex: fields[4].to_owned(),
    })
}

/// A [`HathorRpc`] serving exactly one pre-validated archive row. The runner
/// builds one adapter per row immediately before driving [`process_hathor_height`];
/// the archive height is a hit, any other height is absent, and no method can
/// fail.
struct CachedHathorRpc {
    height: i32,
    meta: HathorBlockMeta,
    tx: HathorTransaction,
}

impl CachedHathorRpc {
    fn new(row: &CacheRow) -> Self {
        let (meta, tx) = row.capture_inputs();
        Self {
            height: row.height,
            meta,
            tx,
        }
    }
}

impl HathorRpc for CachedHathorRpc {
    async fn get_block_at_height(&self, height: i32) -> Result<Option<HathorBlockMeta>> {
        Ok((self.height == height).then(|| self.meta.clone()))
    }

    async fn get_transaction(&self, tx_id: &str) -> Result<Option<HathorTransaction>> {
        Ok((self.meta.tx_id == tx_id).then(|| self.tx.clone()))
    }
}

/// Parsed `backfill-hathor-cache` invocation: the input CSV plus the optional
/// inclusive `[start, end]` height bounds and the progress cadence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HathorCacheConfig {
    pub csv_path: PathBuf,
    /// Inclusive lower bound; rows below it are counted out-of-range, and it also
    /// seeds head-absence accounting (no head gaps without an explicit start).
    pub start_height: Option<i32>,
    /// Inclusive upper bound; the height-sorted scan stops once a row exceeds it.
    pub end_height: Option<i32>,
    /// Progress-log interval in processed rows (`--batch-size`).
    pub progress_every: u64,
}

impl HathorCacheConfig {
    /// Parse CLI args (`<csv> [--start H] [--end H] [--batch-size N]`),
    /// validating non-negative bounds, `end >= start`, and a positive batch size.
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let args: Vec<String> = args.into_iter().map(Into::into).collect();
        let usage = "usage: backfill-hathor-cache <joined-cache-csv> [--start H] [--end H] [--batch-size N]";
        let mut iter = args.into_iter();
        let csv_path = PathBuf::from(iter.next().context(usage)?);
        let mut start_height = None;
        let mut end_height = None;
        let mut progress_every = 10_000u64;
        while let Some(flag) = iter.next() {
            let value = iter
                .next()
                .with_context(|| format!("{usage} (missing value for {flag})"))?;
            match flag.as_str() {
                "--start" => {
                    start_height = Some(value.parse::<i32>().context("--start height")?);
                }
                "--end" => {
                    end_height = Some(value.parse::<i32>().context("--end height")?);
                }
                "--batch-size" => {
                    progress_every = value.parse::<u64>().context("--batch-size rows")?;
                    ensure!(progress_every > 0, "--batch-size must be positive");
                }
                other => bail!("{usage} (unknown flag {other:?})"),
            }
        }
        for (flag, value) in [("--start", start_height), ("--end", end_height)] {
            if let Some(height) = value {
                ensure!(height >= 0, "{flag} height must be non-negative");
            }
        }
        if let (Some(start), Some(end)) = (start_height, end_height) {
            ensure!(
                end >= start,
                "--end {end} must be greater than or equal to --start {start}"
            );
        }
        Ok(Self {
            csv_path,
            start_height,
            end_height,
            progress_every,
        })
    }

    /// Default skip-ledger path: alongside the input CSV.
    pub fn skip_ledger_path(&self) -> PathBuf {
        let mut name = self
            .csv_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "hathor-cache".to_owned());
        name.push_str(".skip-ledger.csv");
        self.csv_path.with_file_name(name)
    }
}

/// Per-run accounting over the OBSERVABLE [`HathorHeightOutcome`] variants
/// plus the runner-level absent/out-of-range counters. Two identities hold for
/// a clean run (any adapter-level corruption aborts instead):
///
/// - rows: `rows_seen = auxpow_written + non_auxpow_skipped + voided_skipped +
///   malformed_skipped + non_btc_parent_skipped + conflict_skipped +
///   table_horizon_hold` (and `rows_seen + rows_out_of_range` = archive data
///   rows read);
/// - heights: `height_attempts() = rows_seen + absent_heights`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HathorCacheSummary {
    pub rows_seen: u64,
    pub rows_out_of_range: u64,
    pub absent_heights: u64,
    pub auxpow_written: u64,
    pub non_auxpow_skipped: u64,
    pub voided_skipped: u64,
    pub malformed_skipped: u64,
    pub non_btc_parent_skipped: u64,
    pub conflict_skipped: u64,
    pub table_horizon_hold: u64,
    pub first_processed_height: Option<i32>,
    pub last_processed_height: Option<i32>,
}

impl HathorCacheSummary {
    pub fn height_attempts(&self) -> u64 {
        self.rows_seen + self.absent_heights
    }

    /// Tally one observable outcome into its per-variant counter. Absent/
    /// transient holds are no-ops here (the runner treats them as fatal before
    /// recording, since the adapter serves every archive row).
    fn record(&mut self, outcome: HathorHeightOutcome) {
        match outcome {
            HathorHeightOutcome::AuxpowWritten => self.auxpow_written += 1,
            HathorHeightOutcome::NonAuxpowSkipped => self.non_auxpow_skipped += 1,
            HathorHeightOutcome::VoidedSkipped => self.voided_skipped += 1,
            HathorHeightOutcome::MalformedSkipped => self.malformed_skipped += 1,
            HathorHeightOutcome::NonBtcParentSkipped => self.non_btc_parent_skipped += 1,
            HathorHeightOutcome::ConflictSkipped => self.conflict_skipped += 1,
            HathorHeightOutcome::TableHorizonHold => self.table_horizon_hold += 1,
            // Absent/Transient holds are handled (fatally) by the runner
            // before record().
            HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => {}
        }
    }
}

/// Append one `height,Outcome` row to the skip ledger.
fn ledger_skip_line(
    ledger: &mut impl Write,
    height: i32,
    outcome: HathorHeightOutcome,
) -> Result<()> {
    writeln!(ledger, "{height},{outcome:?}").context("write skip-ledger row")
}

/// Append a (compact) absent-height ledger entry: a single `H,Absent` line when
/// `from == to`, else a `from..to,Absent` range, so large gaps stay terse.
fn ledger_absent_range(ledger: &mut impl Write, from: i32, to: i32) -> Result<()> {
    if from == to {
        writeln!(ledger, "{from},Absent").context("write skip-ledger absent row")
    } else {
        writeln!(ledger, "{from}..{to},Absent").context("write skip-ledger absent range")
    }
}

/// Completion logging plus the post-ingest read-model repair over the
/// processed height range (budget exhaustion downgrades to a warn with the
/// documented operator follow-up).
async fn finish_cache_ingest(
    client: &mut Client,
    context: &HathorCaptureContext,
    summary: &HathorCacheSummary,
) -> Result<()> {
    info!(
        rows_seen = summary.rows_seen,
        rows_out_of_range = summary.rows_out_of_range,
        absent_heights = summary.absent_heights,
        height_attempts = summary.height_attempts(),
        auxpow_written = summary.auxpow_written,
        non_auxpow_skipped = summary.non_auxpow_skipped,
        voided_skipped = summary.voided_skipped,
        malformed_skipped = summary.malformed_skipped,
        non_btc_parent_skipped = summary.non_btc_parent_skipped,
        conflict_skipped = summary.conflict_skipped,
        table_horizon_hold = summary.table_horizon_hold,
        "completed Hathor cache ingest"
    );

    if let (Some(first), Some(last)) = (
        summary.first_processed_height,
        summary.last_processed_height,
    ) {
        run_post_backfill_repair(
            client,
            context.parent_classifier(),
            Some(mmm_capture::source_registry::HATHOR_SOURCE_CODE),
            first,
            last,
            "Hathor cache ingest",
        )
        .await?;
    }

    Ok(())
}

/// Drive the archive through the capture path. `reader` and `ledger` are
/// injected so the DB integration test can run the real pipeline over an
/// in-memory CSV; the CLI passes the opened file and skip-ledger handles, and
/// a [`HathorCaptureContext`] built with the caller-chosen classifier.
pub async fn run_hathor_cache_ingest<R: BufRead>(
    client: &mut Client,
    context: &HathorCaptureContext,
    reader: R,
    ledger: &mut impl Write,
    config: &HathorCacheConfig,
) -> Result<HathorCacheSummary> {
    let mut rows = CacheReader::new(reader);
    let mut summary = HathorCacheSummary::default();
    // The next height that would be contiguous; absent heights accumulate
    // between it and each processed row. Seeded by --start when explicit,
    // else by the first in-range row (no head absence without --start).
    let mut next_expected: Option<i32> = config.start_height;

    writeln!(
        ledger,
        "# run start={:?} end={:?}",
        config.start_height, config.end_height
    )
    .context("write skip-ledger run header")?;

    while let Some(row) = rows.next_row()? {
        if config.start_height.is_some_and(|start| row.height < start) {
            summary.rows_out_of_range += 1;
            continue;
        }
        if config.end_height.is_some_and(|end| row.height > end) {
            // Height-sorted input: everything after this is out of range too,
            // but stop counting rather than reading gigabytes past --end.
            summary.rows_out_of_range += 1;
            break;
        }
        if let Some(expected) = next_expected
            && row.height > expected
        {
            let absent = (row.height - expected) as u64;
            summary.absent_heights += absent;
            ledger_absent_range(ledger, expected, row.height - 1)?;
        }
        next_expected = Some(row.height + 1);

        let rpc = CachedHathorRpc::new(&row);
        let outcome = process_hathor_height(client, &rpc, context, row.height)
            .await
            .with_context(|| format!("ingest cached Hathor height {}", row.height))?;
        match outcome {
            // A horizon verdict on a historical archive row: with Bitcoin Core
            // disabled (the cache-ingest default) the parent's beyond-table BIP34
            // height holds and is counted + ledgered (a Core-enabled run would
            // resolve it from Core, like the live poller). Most archive rows decode
            // to a real BTC parent era well inside the table (observed live: Hathor
            // 1118451, a Jan-2021 block whose real BTC parent era is ~667k); a
            // junk/contaminant claim beyond the table is the one that holds here.
            // A genuinely stale table still surfaces in verification: the
            // target-zero check fails with the held rows clustered at the archive end.
            HathorHeightOutcome::TableHorizonHold => {
                warn!(
                    height = row.height,
                    "nBits-table horizon verdict on an archive row; counted + ledgered"
                );
                ledger_skip_line(ledger, row.height, outcome)?;
            }
            // The adapter serves every archive row, so a hold here is a runner
            // bug, not data: fail loudly rather than miscounting.
            HathorHeightOutcome::AbsentHold | HathorHeightOutcome::TransientHold => bail!(
                "Hathor cache ingest got {outcome:?} at archive height {}; \
                 cache adapter invariant violated",
                row.height
            ),
            HathorHeightOutcome::AuxpowWritten => {}
            skipped => ledger_skip_line(ledger, row.height, skipped)?,
        }
        summary.record(outcome);
        summary.rows_seen += 1;
        summary.first_processed_height.get_or_insert(row.height);
        summary.last_processed_height = Some(row.height);
        if summary.rows_seen % config.progress_every == 0 {
            info!(
                rows_seen = summary.rows_seen,
                height = row.height,
                auxpow_written = summary.auxpow_written,
                "Hathor cache ingest progress"
            );
        }
    }

    // Tail absence: only measurable against an explicit --end.
    if let (Some(end), Some(expected)) = (config.end_height, next_expected)
        && end >= expected
    {
        summary.absent_heights += (end - expected + 1) as u64;
        ledger_absent_range(ledger, expected, end)?;
    }

    finish_cache_ingest(client, context, &summary).await?;

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fixture() -> serde_json::Value {
        serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../fixtures/hathor/1971823.json"
        )))
        .unwrap()
    }

    fn fixture_row() -> CacheRow {
        let j = fixture();
        let raw = j["raw_hex"].as_str().unwrap();
        let aux = j["aux_pow_hex"].as_str().unwrap();
        assert!(raw.ends_with(aux), "fixture raw must end with aux_pow");
        CacheRow {
            height: j["hathor_height"].as_i64().unwrap() as i32,
            hash: j["tx_id"].as_str().unwrap().to_owned(),
            timestamp: j["timestamp"].as_i64().unwrap(),
            funds_graph_hex: raw[..raw.len() - aux.len()].to_owned(),
            aux_pow_hex: aux.to_owned(),
        }
    }

    fn csv_of(rows: &[CacheRow]) -> String {
        let mut text = format!("{CACHE_CSV_HEADER}\r\n");
        for r in rows {
            text.push_str(&format!(
                "{},{},{},{},{}\r\n",
                r.height, r.hash, r.timestamp, r.funds_graph_hex, r.aux_pow_hex
            ));
        }
        text
    }

    fn read_all(text: &str) -> Result<Vec<CacheRow>> {
        let mut reader = CacheReader::new(Cursor::new(text.to_owned()));
        let mut rows = Vec::new();
        while let Some(row) = reader.next_row()? {
            rows.push(row);
        }
        Ok(rows)
    }

    #[test]
    fn capture_inputs_are_byte_exact_against_the_committed_fixture() {
        let j = fixture();
        let (meta, tx) = fixture_row().capture_inputs();
        assert_eq!(tx.raw, j["raw_hex"].as_str().unwrap());
        assert_eq!(tx.aux_pow.as_deref(), j["aux_pow_hex"].as_str());
        assert_eq!(tx.hash, j["tx_id"].as_str().unwrap());
        assert_eq!(tx.timestamp, j["timestamp"].as_i64().unwrap());
        assert_eq!(meta.version, HATHOR_MERGE_MINED_VERSION);
        assert!(!meta.is_voided);
    }

    #[tokio::test]
    async fn adapter_serves_only_its_archive_row() {
        let row = fixture_row();
        let rpc = CachedHathorRpc::new(&row);
        let meta = rpc.get_block_at_height(row.height).await.unwrap().unwrap();
        assert_eq!(meta.tx_id, row.hash);
        assert!(
            rpc.get_block_at_height(row.height + 1)
                .await
                .unwrap()
                .is_none()
        );
        assert!(rpc.get_transaction(&row.hash).await.unwrap().is_some());
        assert!(rpc.get_transaction("00ff").await.unwrap().is_none());
    }

    #[test]
    fn reader_yields_valid_rows_and_handles_crlf() {
        let row = fixture_row();
        let rows = read_all(&csv_of(std::slice::from_ref(&row))).unwrap();
        assert_eq!(rows, vec![row]);
    }

    #[test]
    fn reader_rejects_wrong_header() {
        let err = read_all("height,hash\r\n").unwrap_err();
        assert!(err.to_string().contains("header"), "{err}");
    }

    #[test]
    fn reader_rejects_empty_file() {
        let err = read_all("").unwrap_err();
        assert!(err.to_string().contains("empty"), "{err}");
    }

    #[test]
    fn reader_rejects_wrong_column_count() {
        let err = read_all(&format!("{CACHE_CSV_HEADER}\n1,aa,3\n")).unwrap_err();
        assert!(err.to_string().contains("expected 5 columns"), "{err}");
    }

    #[test]
    fn reader_rejects_structurally_bad_fields() {
        let good = fixture_row();
        for (mutate, needle) in [
            (
                Box::new(|r: &mut CacheRow| r.hash.truncate(10)) as Box<dyn Fn(&mut CacheRow)>,
                "64 hex chars",
            ),
            (
                Box::new(|r: &mut CacheRow| r.funds_graph_hex = "xyz!".to_owned()),
                "funds_graph_hex",
            ),
            (
                Box::new(|r: &mut CacheRow| r.aux_pow_hex = "abc".to_owned()),
                "aux_pow_hex",
            ),
            (
                Box::new(|r: &mut CacheRow| r.timestamp = 0),
                "hathor_timestamp",
            ),
        ] {
            let mut row = good.clone();
            mutate(&mut row);
            let err = read_all(&csv_of(&[row])).unwrap_err();
            assert!(err.to_string().contains(needle), "{err}");
        }
    }

    #[test]
    fn reader_allows_one_trailing_blank_line_but_rejects_interior_blanks() {
        let row = fixture_row();
        // csv_of already ends each row with CRLF; one extra blank tail line is
        // the tolerated trailing-newline case.
        let trailing = format!("{}\r\n", csv_of(std::slice::from_ref(&row)));
        assert_eq!(read_all(&trailing).unwrap().len(), 1);

        let mut second = row.clone();
        second.height += 1;
        let interior = format!(
            "{}\r\n{},{},{},{},{}\r\n",
            csv_of(std::slice::from_ref(&row)),
            second.height,
            second.hash,
            second.timestamp,
            second.funds_graph_hex,
            second.aux_pow_hex
        );
        let err = read_all(&interior).unwrap_err();
        assert!(err.to_string().contains("blank line inside"), "{err}");
    }

    #[test]
    fn config_rejects_negative_bounds() {
        for args in [["f", "--start", "-5"], ["f", "--end", "-1"]] {
            let err = HathorCacheConfig::from_args(args).unwrap_err();
            assert!(err.to_string().contains("non-negative"), "{err}");
        }
    }

    #[test]
    fn reader_rejects_duplicate_and_out_of_order_heights() {
        let row = fixture_row();
        let dup = read_all(&csv_of(&[row.clone(), row.clone()])).unwrap_err();
        assert!(dup.to_string().contains("strictly greater"), "{dup}");
        let mut earlier = row.clone();
        earlier.height -= 1;
        let ooo = read_all(&csv_of(&[row, earlier])).unwrap_err();
        assert!(ooo.to_string().contains("strictly greater"), "{ooo}");
    }

    #[test]
    fn config_parses_flags_and_derives_ledger_path() {
        let config = HathorCacheConfig::from_args([
            "/tmp/joined.csv",
            "--start",
            "100",
            "--end",
            "200",
            "--batch-size",
            "5",
        ])
        .unwrap();
        assert_eq!(config.start_height, Some(100));
        assert_eq!(config.end_height, Some(200));
        assert_eq!(config.progress_every, 5);
        assert_eq!(
            config.skip_ledger_path(),
            PathBuf::from("/tmp/joined.csv.skip-ledger.csv")
        );
        assert!(HathorCacheConfig::from_args(["f", "--start", "9", "--end", "3"]).is_err());
        assert!(HathorCacheConfig::from_args(["f", "--bogus", "1"]).is_err());
        assert!(HathorCacheConfig::from_args(Vec::<String>::new()).is_err());
    }
}
