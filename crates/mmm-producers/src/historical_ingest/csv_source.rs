//! Pure CSV-row to `ImportCandidate` parsing for the historical importer.
//!
//! No DB or RPC: this layer decodes one recovered-evidence row into a standard
//! `NormalizedEventEvidence` (the same format live producers emit), rejecting bad
//! rows with a typed `SkipReason` rather than an error. It also enforces the
//! orphan-relevance gate: orphan rows are admitted only when the relevance
//! inventory pre-selected their parent hash, so unproven orphans never enter the
//! pipeline. The runner layers the live Core classifier on top of this.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use bitcoin::block::Header;
use bitcoin::consensus::deserialize;
use bitcoin::hashes::{Hash as _, sha256d};
use mmm_capture::auxpow::{parse_bip34_height, validates_target};
use mmm_capture::btc_orphan::{BtcOrphanVerdict, classify_btc_orphan, is_strict_bip34_chain};
use mmm_capture::capture::NormalizedEventEvidence;

use super::config::HistoricalChainSpec;

/// The classification stated by the source dataset's `classification` column.
///
/// This is the dataset's own verdict, not the project's: `Stale` folds the
/// dataset's `stale` and `stale_descendant` labels, and `Orphan` rows are only
/// trusted after the relevance gate and (in the runner) live Core attestation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceClassification {
    Canonical,
    Stale,
    Orphan,
}

/// Why the relevance inventory pre-selected a parent hash, in priority order.
///
/// `KnownStaleDescendant` and `KnownDirectStale` are known-branch attestations
/// (admitted regardless of the local orphan verdict); the two BTC-orphan
/// variants still require a Strict/Weak orphan verdict. `selection_priority`
/// keeps the strongest reason when a hash appears more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RelevanceSelection {
    StrictBtcOrphan,
    WeakBtcOrphan,
    KnownDirectStale,
    KnownStaleDescendant,
}

/// One accepted row, normalized into producer evidence plus the metadata the
/// runner needs to decide and tally it.
///
/// `evidence` is ready for `build_event_payload_from_evidence`;
/// `btc_parent_display_hash` is the reversed-hex parent id used in error context
/// and relevance lookups; `orphan_verdict`/`relevance_selection` carry the
/// orphan provenance the runner re-checks against live classification.
#[derive(Debug, Clone)]
pub(super) struct ImportCandidate {
    pub(super) source_classification: SourceClassification,
    pub(super) evidence: NormalizedEventEvidence,
    /// Reversed-hex (Display) parent block hash, for relevance lookup and logs.
    pub(super) btc_parent_display_hash: String,
    pub(super) orphan_verdict: Option<BtcOrphanVerdict>,
    pub(super) relevance_selection: Option<RelevanceSelection>,
}

/// Typed reason a row is dropped instead of ingested.
///
/// Carried in place of an error so a single bad row never aborts the import;
/// the runner tallies these by `as_str` into the summary's `skipped` map. The
/// `as_str` values are the stable keys printed in that summary, treat them as a
/// reporting contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SkipReason {
    EmptyField,
    Malformed,
    HashMismatch,
    TargetInvalid,
    UnsupportedClassification,
    Near,
    OrphanNotSelected,
    OrphanExcluded,
    OrphanPending,
    Unclassified,
    KnownBranchNotClassified,
}

impl SkipReason {
    /// Stable snake_case key for this reason, used as the summary `skipped` map
    /// key and in printed output. Changing a value changes that reporting contract.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::EmptyField => "empty_field",
            Self::Malformed => "malformed",
            Self::HashMismatch => "hash_mismatch",
            Self::TargetInvalid => "target_invalid",
            Self::UnsupportedClassification => "unsupported_classification",
            Self::Near => "near",
            Self::OrphanNotSelected => "orphan_not_selected",
            Self::OrphanExcluded => "orphan_excluded",
            Self::OrphanPending => "orphan_pending",
            Self::Unclassified => "unclassified",
            Self::KnownBranchNotClassified => "known_branch_not_classified",
        }
    }
}

/// The set of parent hashes the relevance inventory pre-cleared for orphan rows,
/// keyed by reversed-hex (Display) hash to the strongest selection reason.
///
/// Empty by default (no inventory), which means no orphan row is admitted.
#[derive(Debug, Default, Clone)]
pub(super) struct RelevanceFilter {
    selected_orphans: BTreeMap<String, RelevanceSelection>,
}

impl RelevanceFilter {
    /// Whether `display_hash` was pre-selected (orphan rows are gated on this).
    fn allows_orphan(&self, display_hash: &str) -> bool {
        self.selected_orphans.contains_key(display_hash)
    }

    /// The winning selection reason for `display_hash`, if pre-selected.
    fn selection_for_orphan(&self, display_hash: &str) -> Option<RelevanceSelection> {
        self.selected_orphans.get(display_hash).copied()
    }
}

/// Load the relevance filter from an optional inventory CSV, scoped to `chain`.
/// `None` path yields an empty filter (every orphan row will be skipped).
pub(super) fn load_relevance_filter(path: Option<&Path>, chain: &str) -> Result<RelevanceFilter> {
    let Some(path) = path else {
        return Ok(RelevanceFilter::default());
    };
    let file = std::fs::File::open(path)
        .with_context(|| format!("open relevance inventory {}", path.display()))?;
    read_relevance_filter(file, chain)
        .with_context(|| format!("read relevance inventory {}", path.display()))
}

/// Parse a relevance inventory from any reader, keeping only rows for `chain`
/// whose `btc_stale_relevance`/`relevance_reason` map to a `RelevanceSelection`
/// (other rows, including `*_excluded`, are dropped). Duplicate parent hashes
/// resolve to the highest-priority selection via `insert_selection`.
fn read_relevance_filter<R: Read>(reader: R, chain: &str) -> Result<RelevanceFilter> {
    let mut csv = csv::Reader::from_reader(reader);
    let mut selected_orphans = BTreeMap::new();
    for row in csv.deserialize::<BTreeMap<String, String>>() {
        let row = row.context("parse relevance inventory row")?;
        if row.get("chain").map(String::as_str) != Some(chain) {
            continue;
        }
        let relevance = row
            .get("btc_stale_relevance")
            .map(|value| value.trim())
            .unwrap_or_default();
        let reason = row
            .get("relevance_reason")
            .map(|value| value.trim())
            .unwrap_or_default();
        let selection = match (relevance, reason) {
            ("strict_btc_orphan", _) => RelevanceSelection::StrictBtcOrphan,
            ("weak_btc_orphan", _) => RelevanceSelection::WeakBtcOrphan,
            (_, "known_direct_stale_hash") => RelevanceSelection::KnownDirectStale,
            (_, "known_stale_descendant_hash") => RelevanceSelection::KnownStaleDescendant,
            _ => continue,
        };
        let hash = required_value(&row, "btc_header_hash")?;
        insert_selection(&mut selected_orphans, hash.to_owned(), selection);
    }
    Ok(RelevanceFilter { selected_orphans })
}

/// Resolved column indices for one dataset's header row.
///
/// Computed once per file so per-row parsing is positional. Required columns
/// (`header_hex`, `classification`, a height column) error at construction;
/// optional ones are `None` and silently absent. Height accepts the chain's
/// `height_column` or a normalized `child_height`; the cross-check column accepts
/// either `btc_header_hash` or `btc_hash`.
pub(super) struct CsvLayout {
    height: usize,
    child_hash: Option<usize>,
    header_hex: usize,
    coinbase_script: Option<usize>,
    classification: usize,
    hash_cross_check: Option<usize>,
}

impl CsvLayout {
    /// Resolve column indices against `headers`, applying the chain-specific and
    /// fallback column-name rules. Errors if a required column is missing.
    pub(super) fn new(headers: &csv::StringRecord, spec: &HistoricalChainSpec) -> Result<Self> {
        Ok(Self {
            height: required_header(headers, spec.height_column)
                .or_else(|_| required_header(headers, "child_height"))?,
            child_hash: optional_header(headers, "child_block_hash"),
            header_hex: required_header(headers, "btc_header_hex")?,
            coinbase_script: optional_header(headers, "coinbase_scriptsig_hex"),
            classification: required_header(headers, "classification")?,
            hash_cross_check: optional_header(headers, "btc_header_hash")
                .or_else(|| optional_header(headers, "btc_hash")),
        })
    }
}

/// Decode one CSV row into an `ImportCandidate`, or a `SkipReason` if it should
/// be dropped.
///
/// Validation order matters: parse height and classification, deserialize the
/// 80-byte parent header, cross-check the stated hash against the derived one,
/// reject headers whose PoW does not meet their own target, then run the
/// orphan-relevance gate. The child hash falls back to a deterministic
/// `synthetic_child_hash` when the dataset omits it, so re-imports stay
/// idempotent on the same `(source, child_height)`.
pub(super) fn candidate_from_record(
    spec: &HistoricalChainSpec,
    layout: &CsvLayout,
    record: &csv::StringRecord,
    relevance: &RelevanceFilter,
) -> Result<ImportCandidate, SkipReason> {
    let child_height = parse_child_height(record.get(layout.height))?;
    let source_classification = parse_source_classification(record.get(layout.classification))?;
    let header = parse_parent_header(record.get(layout.header_hex))?;
    let display_hash = header.block_hash().to_string();
    check_hash_cross_reference(record, layout.hash_cross_check, &display_hash)?;
    if !validates_target(header.block_hash(), header.bits) {
        return Err(SkipReason::TargetInvalid);
    }
    let coinbase_script =
        parse_optional_hex_field(layout.coinbase_script.and_then(|index| record.get(index)))?;
    let orphan_verdict = orphan_verdict(spec.chain, &header, coinbase_script.as_deref());
    let relevance_selection = relevance.selection_for_orphan(&display_hash);
    filter_source_classification(
        source_classification,
        &display_hash,
        orphan_verdict,
        relevance_selection,
        relevance,
    )?;
    let evidence = NormalizedEventEvidence {
        child_height,
        child_block_hash: parse_optional_hash_field(
            layout.child_hash.and_then(|index| record.get(index)),
        )?
        .unwrap_or_else(|| synthetic_child_hash(spec.chain, child_height)),
        child_block_time: header.time as i64,
        btc_parent_header: header,
        pow_validates_child_target: None,
        btc_parent_coinbase_txid: None,
        btc_parent_coinbase_script: coinbase_script,
        btc_parent_coinbase_outputs: None,
        child_coinbase_txid: None,
        child_coinbase_script: None,
        child_coinbase_outputs: None,
        aux_merkle_proof: None,
    };
    Ok(ImportCandidate {
        source_classification,
        evidence,
        btc_parent_display_hash: display_hash,
        orphan_verdict: Some(orphan_verdict),
        relevance_selection,
    })
}

/// The orphan-relevance gate. Non-orphan rows pass unconditionally. Orphan rows
/// must be in the relevance filter; known-branch selections are admitted
/// outright, while BTC-orphan selections additionally require a Strict/Weak
/// local verdict (Excluded/Pending map to their own skip reasons). This is what
/// keeps unproven orphans out of the ingest pipeline.
fn filter_source_classification(
    classification: SourceClassification,
    display_hash: &str,
    orphan_verdict: BtcOrphanVerdict,
    relevance_selection: Option<RelevanceSelection>,
    relevance: &RelevanceFilter,
) -> Result<(), SkipReason> {
    if classification != SourceClassification::Orphan {
        return Ok(());
    }
    if !relevance.allows_orphan(display_hash) {
        return Err(SkipReason::OrphanNotSelected);
    }
    match relevance_selection {
        Some(RelevanceSelection::KnownDirectStale | RelevanceSelection::KnownStaleDescendant) => {
            return Ok(());
        }
        Some(RelevanceSelection::StrictBtcOrphan | RelevanceSelection::WeakBtcOrphan) => {}
        None => return Err(SkipReason::OrphanNotSelected),
    }
    match orphan_verdict {
        BtcOrphanVerdict::Strict | BtcOrphanVerdict::Weak => Ok(()),
        BtcOrphanVerdict::Excluded => Err(SkipReason::OrphanExcluded),
        BtcOrphanVerdict::Pending => Err(SkipReason::OrphanPending),
    }
}

/// Insert keeping the strongest reason: a new selection overwrites only when its
/// priority strictly exceeds the existing one, so inventory row order does not
/// affect the result.
fn insert_selection(
    selected_orphans: &mut BTreeMap<String, RelevanceSelection>,
    hash: String,
    selection: RelevanceSelection,
) {
    match selected_orphans.get(&hash).copied() {
        Some(existing) if selection_priority(existing) >= selection_priority(selection) => {}
        _ => {
            selected_orphans.insert(hash, selection);
        }
    }
}

/// Total order over selection reasons (higher wins): known-branch attestations
/// outrank BTC-orphan verdicts, descendant outranks direct, strict outranks weak.
fn selection_priority(selection: RelevanceSelection) -> u8 {
    match selection {
        RelevanceSelection::KnownStaleDescendant => 4,
        RelevanceSelection::KnownDirectStale => 3,
        RelevanceSelection::StrictBtcOrphan => 2,
        RelevanceSelection::WeakBtcOrphan => 1,
    }
}

/// Compute the local BTC-orphan verdict for a parent header. BIP34 height is
/// only parsed from the coinbase for strict-BIP34 chains; otherwise the verdict
/// rests on timestamp and target alone.
fn orphan_verdict(
    chain: &str,
    header: &Header,
    coinbase_script: Option<&[u8]>,
) -> BtcOrphanVerdict {
    let strict_height = is_strict_bip34_chain(chain)
        .then(|| coinbase_script.and_then(parse_bip34_height))
        .flatten();
    classify_btc_orphan(header.time as i64, header.bits, strict_height).0
}

/// Locate a required column index, erroring (aborting the import) if absent.
fn required_header(headers: &csv::StringRecord, name: &str) -> Result<usize> {
    optional_header(headers, name)
        .ok_or_else(|| anyhow::anyhow!("CSV missing required column {name}"))
}

/// Locate a column index by trimmed exact name, `None` if not present.
fn optional_header(headers: &csv::StringRecord, name: &str) -> Option<usize> {
    headers.iter().position(|header| header.trim() == name)
}

/// Parse the child height; empty is `EmptyField`, non-integer is `Malformed`.
fn parse_child_height(value: Option<&str>) -> Result<i32, SkipReason> {
    let value = non_empty(value)?;
    value.parse().map_err(|_| SkipReason::Malformed)
}

/// Map the dataset's `classification` label to a `SourceClassification`. `stale`
/// and `stale_descendant` both fold to `Stale`; `near` and unrecognized labels
/// become their own skip reasons rather than being silently treated as orphan.
fn parse_source_classification(value: Option<&str>) -> Result<SourceClassification, SkipReason> {
    match non_empty(value)?.trim() {
        "canonical" => Ok(SourceClassification::Canonical),
        "stale" | "stale_descendant" => Ok(SourceClassification::Stale),
        "orphan" => Ok(SourceClassification::Orphan),
        "near" => Err(SkipReason::Near),
        _ => Err(SkipReason::UnsupportedClassification),
    }
}

/// Decode and consensus-deserialize the parent header, enforcing the exact
/// 80-byte `Header::SIZE` before deserializing so a wrong-length hex is rejected.
fn parse_parent_header(value: Option<&str>) -> Result<Header, SkipReason> {
    let raw = parse_hex_field(value)?;
    if raw.len() != Header::SIZE {
        return Err(SkipReason::Malformed);
    }
    deserialize(&raw).map_err(|_| SkipReason::Malformed)
}

/// Decode a required hex field; empty is `EmptyField`, bad hex is `Malformed`.
fn parse_hex_field(value: Option<&str>) -> Result<Vec<u8>, SkipReason> {
    hex::decode(non_empty(value)?).map_err(|_| SkipReason::Malformed)
}

/// Decode an optional hex field: missing/blank yields `Ok(None)`, bad hex is `Malformed`.
fn parse_optional_hex_field(value: Option<&str>) -> Result<Option<Vec<u8>>, SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        Ok(None)
    } else {
        hex::decode(value)
            .map(Some)
            .map_err(|_| SkipReason::Malformed)
    }
}

/// Decode an optional 32-byte hash field, enforcing the byte length. Returned
/// bytes are stored as-is (wire/internal order): callers must not re-reverse.
fn parse_optional_hash_field(value: Option<&str>) -> Result<Option<Vec<u8>>, SkipReason> {
    let Some(bytes) = parse_optional_hex_field(value)? else {
        return Ok(None);
    };
    if bytes.len() == 32 {
        Ok(Some(bytes))
    } else {
        Err(SkipReason::Malformed)
    }
}

/// Trim and require a non-empty value, mapping blank/missing to `EmptyField`.
fn non_empty(value: Option<&str>) -> Result<&str, SkipReason> {
    let value = value.map(str::trim).unwrap_or_default();
    if value.is_empty() {
        Err(SkipReason::EmptyField)
    } else {
        Ok(value)
    }
}

/// Guard the dataset's stated parent hash against the one derived from the
/// header. No cross-check column or a blank value passes; a present, mismatching
/// (case-insensitive) value is `HashMismatch`, catching corrupted header hex.
fn check_hash_cross_reference(
    record: &csv::StringRecord,
    index: Option<usize>,
    display_hash: &str,
) -> Result<(), SkipReason> {
    let Some(index) = index else {
        return Ok(());
    };
    let stated = record.get(index).map(str::trim).unwrap_or_default();
    if stated.is_empty() || stated.eq_ignore_ascii_case(display_hash) {
        Ok(())
    } else {
        Err(SkipReason::HashMismatch)
    }
}

/// Read a required relevance-inventory field, erroring if blank or absent.
fn required_value<'a>(row: &'a BTreeMap<String, String>, key: &str) -> Result<&'a str> {
    let value = row.get(key).map(|value| value.trim()).unwrap_or_default();
    if value.is_empty() {
        bail!("relevance inventory row missing {key}");
    }
    Ok(value)
}

/// Deterministic child-block hash for datasets lacking a real one, derived as
/// `sha256d("mmm-dataset:<chain>:<height>")`.
///
/// Stable across re-imports (so the `(source, child_height)` upsert stays
/// idempotent) and source-scoped (so two chains at the same height never
/// collide). Returned in `to_byte_array` (wire/internal) order; never reverse it.
fn synthetic_child_hash(chain: &str, child_height: i32) -> Vec<u8> {
    let material = format!("mmm-dataset:{chain}:{child_height}");
    sha256d::Hash::hash(material.as_bytes())
        .to_byte_array()
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::super::config::historical_chain_spec;
    use super::*;

    const GENESIS_HEADER: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";
    const GENESIS_HASH: &str = "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f";

    #[test]
    fn derives_parent_fields_and_synthetic_child_identity() {
        let spec = historical_chain_spec("devcoin").unwrap();
        let (layout, record) = layout_and_record(&format!(
            "dvc_height,btc_header_hex,coinbase_scriptsig_hex,classification,btc_header_hash\n\
             42,{GENESIS_HEADER},04ffff001d0104,stale,{GENESIS_HASH}\n"
        ));
        let first =
            candidate_from_record(spec, &layout, &record, &RelevanceFilter::default()).unwrap();
        let second =
            candidate_from_record(spec, &layout, &record, &RelevanceFilter::default()).unwrap();

        assert_eq!(first.btc_parent_display_hash, GENESIS_HASH);
        assert_eq!(first.evidence.child_height, 42);
        assert_eq!(
            first.evidence.child_block_hash,
            second.evidence.child_block_hash
        );
        assert!(first.evidence.btc_parent_coinbase_outputs.is_none());
    }

    #[test]
    fn rejects_hash_cross_reference_mismatch() {
        let spec = historical_chain_spec("devcoin").unwrap();
        let (layout, record) = layout_and_record(&format!(
            "dvc_height,btc_header_hex,coinbase_scriptsig_hex,classification,btc_header_hash\n\
             42,{GENESIS_HEADER},04ffff001d0104,stale,{}\n",
            "11".repeat(32)
        ));

        assert_eq!(
            candidate_from_record(spec, &layout, &record, &RelevanceFilter::default()).unwrap_err(),
            SkipReason::HashMismatch
        );
    }

    #[test]
    fn orphan_rows_need_relevance_selection() {
        let spec = historical_chain_spec("devcoin").unwrap();
        let (layout, record) = layout_and_record(&format!(
            "dvc_height,btc_header_hex,coinbase_scriptsig_hex,classification,btc_header_hash\n\
             42,{GENESIS_HEADER},04ffff001d0104,orphan,{GENESIS_HASH}\n"
        ));

        assert_eq!(
            candidate_from_record(spec, &layout, &record, &RelevanceFilter::default()).unwrap_err(),
            SkipReason::OrphanNotSelected
        );
    }

    #[test]
    fn relevance_filter_loads_only_exact_chain_strict_or_weak_rows() {
        let csv = "\
chain,btc_stale_relevance,btc_header_hash\n\
devcoin,strict_btc_orphan,aa\n\
devcoin,weak_btc_orphan,bb\n\
devcoin,btc_stale_excluded,cc\n\
devcoin|ixcoin,strict_btc_orphan,dd\n\
ixcoin,strict_btc_orphan,ee\n";
        let filter = read_relevance_filter(csv.as_bytes(), "devcoin").unwrap();

        assert!(filter.allows_orphan("aa"));
        assert!(filter.allows_orphan("bb"));
        assert!(!filter.allows_orphan("cc"));
        assert!(!filter.allows_orphan("dd"));
        assert!(!filter.allows_orphan("ee"));
    }

    #[test]
    fn relevance_filter_loads_known_branch_attestation_reasons() {
        let csv = "\
chain,source_classification,btc_stale_relevance,relevance_reason,btc_header_hash\n\
devcoin,orphan,btc_stale_excluded,known_direct_stale_hash,aa\n\
devcoin,unknown,btc_stale_excluded,known_stale_descendant_hash,bb\n\
devcoin,orphan,btc_stale_excluded,validation_rejected,cc\n\
devcoin,stale,confirmed_btc_stale,valid_direct_stale,dd\n\
ixcoin,orphan,btc_stale_excluded,known_direct_stale_hash,ee\n";
        let filter = read_relevance_filter(csv.as_bytes(), "devcoin").unwrap();

        assert_eq!(
            filter.selection_for_orphan("aa"),
            Some(RelevanceSelection::KnownDirectStale)
        );
        assert_eq!(
            filter.selection_for_orphan("bb"),
            Some(RelevanceSelection::KnownStaleDescendant)
        );
        assert!(!filter.allows_orphan("cc"));
        assert!(!filter.allows_orphan("dd"));
        assert!(!filter.allows_orphan("ee"));
    }

    #[test]
    fn known_branch_orphan_rows_pass_relevance_selection() {
        let spec = historical_chain_spec("devcoin").unwrap();
        let (layout, record) = layout_and_record(&format!(
            "dvc_height,btc_header_hex,coinbase_scriptsig_hex,classification,btc_header_hash\n\
             42,{GENESIS_HEADER},04ffff001d0104,orphan,{GENESIS_HASH}\n"
        ));
        let csv = format!(
            "chain,btc_stale_relevance,relevance_reason,btc_header_hash\n\
             devcoin,btc_stale_excluded,known_stale_descendant_hash,{GENESIS_HASH}\n"
        );
        let relevance = read_relevance_filter(csv.as_bytes(), "devcoin").unwrap();

        let candidate = candidate_from_record(spec, &layout, &record, &relevance).unwrap();

        assert_eq!(
            candidate.relevance_selection,
            Some(RelevanceSelection::KnownStaleDescendant)
        );
        assert_eq!(
            candidate.source_classification,
            SourceClassification::Orphan
        );
    }

    #[test]
    fn normalized_full_evidence_rows_use_child_height_and_hash() {
        let spec = historical_chain_spec("devcoin").unwrap();
        let child_hash = "11".repeat(32);
        let (layout, record) = layout_and_record(&format!(
            "chain,child_height,child_block_hash,btc_header_hex,coinbase_scriptsig_hex,classification,btc_header_hash\n\
             devcoin,42,{child_hash},{GENESIS_HEADER},,stale,{GENESIS_HASH}\n"
        ));

        let candidate =
            candidate_from_record(spec, &layout, &record, &RelevanceFilter::default()).unwrap();

        assert_eq!(candidate.evidence.child_height, 42);
        assert_eq!(candidate.evidence.child_block_hash, vec![0x11; 32]);
        assert!(candidate.evidence.btc_parent_coinbase_script.is_none());
    }

    #[test]
    fn synthetic_child_hash_is_source_scoped() {
        assert_ne!(
            synthetic_child_hash("devcoin", 10),
            synthetic_child_hash("ixcoin", 10)
        );
    }

    fn layout_and_record(input: &str) -> (CsvLayout, csv::StringRecord) {
        let mut reader = csv::Reader::from_reader(input.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let spec = historical_chain_spec("devcoin").unwrap();
        let layout = CsvLayout::new(&headers, spec).unwrap();
        let record = reader.records().next().unwrap().unwrap();
        (layout, record)
    }
}
