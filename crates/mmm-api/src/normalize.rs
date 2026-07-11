//! Shared query-parameter normalization for the read API. These helpers are the
//! shared implementation of the cross-cutting 4xx contracts (invalid_query,
//! unsupported_source, invalid_hash) that every source-filtered endpoint
//! reuses. See `docs/api-contract.md` (Source Codes, Errors).

use serde_json::json;

use super::error::ApiError;
use mmm_capture::source_registry;

/// Normalize the `source` filter per the contract's three-tier rejection ladder:
///
/// 1. `None` -> no filter (empty vec).
/// 2. empty members (after splitting on `,` and trimming) -> invalid_query.
/// 3. malformed / non-lowercase syntax -> invalid_query.
/// 4. unknown source-code kind -> invalid_query.
/// 5. well-formed, known-kind, but unregistered -> unsupported_source.
///
/// The surviving members are deduplicated and sorted lexicographically.
pub(crate) fn normalize_sources(raw: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };

    let mut members: Vec<String> = Vec::new();
    // Split FIRST, then trim, so empty members are detected and never dropped.
    for member in raw.split(',') {
        let member = member.trim();
        if member.is_empty() {
            return Err(ApiError::invalid_query(
                "source filter members must not be empty",
                json!({ "source": raw }),
            ));
        }
        if !is_valid_source_syntax(member) {
            return Err(ApiError::invalid_query(
                format!("source code {member:?} is malformed"),
                json!({ "source": member }),
            ));
        }
        let kind = member.split(':').next().unwrap_or_default();
        if !source_registry::SourceKind::ALL
            .iter()
            .any(|source_kind| source_kind.as_str() == kind)
        {
            return Err(ApiError::invalid_query(
                format!("source code {member:?} has an unknown kind"),
                json!({ "source": member }),
            ));
        }
        if source_registry::by_code(member).is_none() {
            return Err(ApiError::unsupported_source(member));
        }
        members.push(member.to_owned());
    }

    members.sort();
    members.dedup();
    Ok(members)
}

/// Normalize a 64-hex block hash to lowercase, or reject as invalid_hash.
pub(crate) fn normalize_hash(raw: &str) -> Result<String, ApiError> {
    if raw.len() == 64 && raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(raw.to_ascii_lowercase())
    } else {
        Err(ApiError::invalid_hash(raw))
    }
}

/// The parent-kind enum used by `kinds=` filters. Distinct from the source-code
/// `kind` enum in `source_registry::SourceKind`. Shared by first-wave endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentKind {
    Near,
    Unknown,
    Canonical,
    Stale,
}

/// Parse a comma-delimited `kinds=` value into the parent-kind enum. An unknown
/// member is invalid_query.
pub(crate) fn parse_kinds(raw: Option<&str>) -> Result<Vec<ParentKind>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut kinds = Vec::new();
    for member in raw.split(',') {
        let kind = match member.trim() {
            "near" => ParentKind::Near,
            "unknown" => ParentKind::Unknown,
            "canonical" => ParentKind::Canonical,
            "stale" => ParentKind::Stale,
            other => {
                return Err(ApiError::invalid_query(
                    format!("unknown kind {other:?}"),
                    json!({ "kinds": raw }),
                ));
            }
        };
        kinds.push(kind);
    }
    Ok(kinds)
}

/// The BTC-orphan classification enum used by the `classification=` filter. This
/// is the derived refinement of `kind='unknown'` (see `block.btc_orphan_class`),
/// NOT a structural parent kind, so it is deliberately a SEPARATE parameter from
/// `kinds=` and never smuggled through [`ParentKind`]. `Pending` is the
/// `btc_orphan_class IS NULL` transient (never Core-checked, or beyond the
/// committed nBits table horizon).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    StrictBtcOrphan,
    WeakBtcOrphan,
    BtcStaleExcluded,
    Pending,
}

impl Classification {
    /// The wire/query value, equal to the `block.btc_orphan_class` DB string for
    /// the three concrete classes and `"pending"` for the NULL transient.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StrictBtcOrphan => "strict_btc_orphan",
            Self::WeakBtcOrphan => "weak_btc_orphan",
            Self::BtcStaleExcluded => "btc_stale_excluded",
            Self::Pending => "pending",
        }
    }

    /// The `block.btc_orphan_class` DB value for the three concrete classes;
    /// `None` (SQL NULL) for `Pending`. Used to build the orphan-class SQL filter.
    pub fn as_db_str(self) -> Option<&'static str> {
        match self {
            Self::StrictBtcOrphan => Some("strict_btc_orphan"),
            Self::WeakBtcOrphan => Some("weak_btc_orphan"),
            Self::BtcStaleExcluded => Some("btc_stale_excluded"),
            Self::Pending => None,
        }
    }
}

/// Parse a comma-delimited `classification=` value into the orphan-class enum. An
/// unknown member is invalid_query. Distinct from [`parse_kinds`]: the orphan
/// classes are not structural parent kinds and travel on their own parameter.
pub(crate) fn parse_classifications(raw: Option<&str>) -> Result<Vec<Classification>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut classifications = Vec::new();
    for member in raw.split(',') {
        let classification = match member.trim() {
            "strict_btc_orphan" => Classification::StrictBtcOrphan,
            "weak_btc_orphan" => Classification::WeakBtcOrphan,
            "btc_stale_excluded" => Classification::BtcStaleExcluded,
            "pending" => Classification::Pending,
            other => {
                return Err(ApiError::invalid_query(
                    format!("unknown classification {other:?}"),
                    json!({ "classification": raw }),
                ));
            }
        };
        classifications.push(classification);
    }
    Ok(classifications)
}

/// The contract source-code syntax:
/// `^[a-z][a-z0-9]*(-[a-z0-9]+)*(:[a-z][a-z0-9]*(-[a-z0-9]+)*){1,2}$`.
/// Implemented directly to avoid pulling in a regex dependency.
fn is_valid_source_syntax(code: &str) -> bool {
    let segments: Vec<&str> = code.split(':').collect();
    // <kind>:<chain> or <kind>:<chain>:<instance> => 2 or 3 segments.
    if segments.len() < 2 || segments.len() > 3 {
        return false;
    }
    segments.iter().all(|segment| is_valid_segment(segment))
}

/// One segment: `[a-z][a-z0-9]*(-[a-z0-9]+)*` (lowercase, no leading digit, no
/// leading/trailing/double hyphen).
fn is_valid_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_lowercase() {
        return false;
    }
    let mut prev_hyphen = false;
    for (i, &byte) in bytes.iter().enumerate() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => prev_hyphen = false,
            b'-' => {
                // No double hyphen and no trailing hyphen. A leading hyphen is
                // already excluded because the first byte must be [a-z].
                if prev_hyphen || i == bytes.len() - 1 {
                    return false;
                }
                prev_hyphen = true;
            }
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(err: &ApiError) -> &'static str {
        err.code()
    }

    #[test]
    fn none_is_no_filter() {
        assert_eq!(normalize_sources(None).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn empty_value_is_invalid_query() {
        assert_eq!(
            code(&normalize_sources(Some("")).unwrap_err()),
            "invalid_query"
        );
    }

    #[test]
    fn empty_members_after_split_are_invalid_query() {
        for raw in [
            ",",
            " , ",
            "   ",
            "auxpow:namecoin,",
            "auxpow:namecoin,,auxpow:rsk",
        ] {
            assert_eq!(
                code(&normalize_sources(Some(raw)).unwrap_err()),
                "invalid_query",
                "{raw:?}"
            );
        }
    }

    #[test]
    fn malformed_syntax_is_invalid_query() {
        for raw in [
            "AUXPOW:namecoin",
            "auxpow:",
            ":namecoin",
            "live--chaintip:bitcoin",
            "auxpow",
        ] {
            assert_eq!(
                code(&normalize_sources(Some(raw)).unwrap_err()),
                "invalid_query",
                "{raw:?}"
            );
        }
    }

    #[test]
    fn unknown_kind_is_invalid_query() {
        assert_eq!(
            code(&normalize_sources(Some("bogus:namecoin")).unwrap_err()),
            "invalid_query"
        );
    }

    #[test]
    fn valid_kind_but_unregistered_is_unsupported_source() {
        // A well-formed auxpow code for a chain not in SOURCE_REGISTRY: valid
        // kind, but no such source. Synthetic so it cannot become registered by
        // a future chain addition (Doichain, the old example, is now surveyed).
        assert_eq!(
            code(&normalize_sources(Some("auxpow:not-a-registered-chain")).unwrap_err()),
            "unsupported_source"
        );
    }

    #[test]
    fn removed_source_kinds_are_invalid_query() {
        for raw in ["dataset:foo", "harvester:bar"] {
            assert_eq!(
                code(&normalize_sources(Some(raw)).unwrap_err()),
                "invalid_query",
                "{raw:?}"
            );
        }
    }

    #[test]
    fn registered_sources_dedupe_and_sort() {
        let got = normalize_sources(Some("auxpow:rsk,auxpow:syscoin,auxpow:namecoin,auxpow:rsk"))
            .unwrap();
        assert_eq!(
            got,
            vec![
                "auxpow:namecoin".to_owned(),
                "auxpow:rsk".to_owned(),
                "auxpow:syscoin".to_owned()
            ]
        );
    }

    #[test]
    fn all_registered_codes_validate() {
        // Every registered source code must pass the full filter ladder.
        let input = source_registry::SOURCE_REGISTRY
            .iter()
            .map(|source| source.code)
            .collect::<Vec<_>>()
            .join(",");
        let got = normalize_sources(Some(&input)).unwrap();
        let mut expected: Vec<String> = source_registry::SOURCE_REGISTRY
            .iter()
            .map(|source| source.code.to_owned())
            .collect();
        expected.sort();
        assert_eq!(got, expected);
        // Spot-check both axes: a live producer code and a historical one.
        assert!(got.iter().any(|c| c == "auxpow:namecoin"));
        assert!(got.iter().any(|c| c == "auxpow:devcoin"));
    }

    #[test]
    fn hash_normalizes_and_rejects() {
        assert_eq!(normalize_hash(&"AB".repeat(32)).unwrap(), "ab".repeat(32));
        assert_eq!(code(&normalize_hash("xyz").unwrap_err()), "invalid_hash");
        assert_eq!(
            code(&normalize_hash(&"a".repeat(63)).unwrap_err()),
            "invalid_hash"
        );
        assert_eq!(
            code(&normalize_hash(&"g".repeat(64)).unwrap_err()),
            "invalid_hash"
        );
    }

    #[test]
    fn parse_kinds_roundtrip_and_reject() {
        assert_eq!(
            parse_kinds(Some("near,stale")).unwrap(),
            vec![ParentKind::Near, ParentKind::Stale]
        );
        assert_eq!(
            code(&parse_kinds(Some("bogus")).unwrap_err()),
            "invalid_query"
        );
    }

    #[test]
    fn parse_classifications_roundtrip_and_reject() {
        assert_eq!(
            parse_classifications(Some("strict_btc_orphan,weak_btc_orphan")).unwrap(),
            vec![
                Classification::StrictBtcOrphan,
                Classification::WeakBtcOrphan
            ]
        );
        assert_eq!(
            parse_classifications(Some("btc_stale_excluded,pending")).unwrap(),
            vec![Classification::BtcStaleExcluded, Classification::Pending]
        );
        assert_eq!(parse_classifications(None).unwrap(), Vec::new());
        // A structural kind is NOT a classification member: the two parameters
        // are deliberately disjoint.
        assert_eq!(
            code(&parse_classifications(Some("unknown")).unwrap_err()),
            "invalid_query"
        );
        assert_eq!(
            code(&parse_classifications(Some("bogus")).unwrap_err()),
            "invalid_query"
        );
    }

    #[test]
    fn classification_db_str_maps_pending_to_null() {
        assert_eq!(
            Classification::StrictBtcOrphan.as_db_str(),
            Some("strict_btc_orphan")
        );
        assert_eq!(
            Classification::WeakBtcOrphan.as_db_str(),
            Some("weak_btc_orphan")
        );
        assert_eq!(
            Classification::BtcStaleExcluded.as_db_str(),
            Some("btc_stale_excluded")
        );
        assert_eq!(Classification::Pending.as_db_str(), None);
        assert_eq!(Classification::Pending.as_str(), "pending");
    }
}
