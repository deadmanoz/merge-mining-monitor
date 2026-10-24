//! Shared query-parameter normalization for `source`, `:hash`, `kinds`, and `classification`.

use serde_json::json;

use super::error::ApiError;
use mmm_capture::source_registry;

/// Normalize `source=` into sorted, deduplicated registered source codes, rejecting empty
/// members, malformed syntax, or unknown kinds as `invalid_query` and unregistered codes as
/// `unsupported_source`.
pub(crate) fn normalize_sources(raw: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };

    let mut members: Vec<String> = Vec::new();
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

pub(crate) fn normalize_hash(raw: &str) -> Result<String, ApiError> {
    lowercase_hex_64(raw).ok_or_else(|| ApiError::invalid_hash(raw))
}

pub(crate) fn lowercase_hex_64(raw: &str) -> Option<String> {
    (raw.len() == 64 && raw.chars().all(|ch| ch.is_ascii_hexdigit()))
        .then(|| raw.to_ascii_lowercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParentKind {
    Near,
    Unknown,
    Canonical,
    Stale,
    InvalidBlock,
}

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
            "invalid_block" => ParentKind::InvalidBlock,
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

/// BTC-orphan classification (`classification=` parameter, distinct from structural `kinds=`).
/// `Pending` corresponds to `block.btc_orphan_class IS NULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Classification {
    StrictBtcOrphan,
    WeakBtcOrphan,
    BtcStaleExcluded,
    Pending,
}

impl Classification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StrictBtcOrphan => "strict_btc_orphan",
            Self::WeakBtcOrphan => "weak_btc_orphan",
            Self::BtcStaleExcluded => "excluded",
            Self::Pending => "pending",
        }
    }

    pub fn as_db_str(self) -> Option<&'static str> {
        match self {
            Self::StrictBtcOrphan => Some("strict_btc_orphan"),
            Self::WeakBtcOrphan => Some("weak_btc_orphan"),
            Self::BtcStaleExcluded => Some("excluded"),
            Self::Pending => None,
        }
    }
}

pub(crate) fn parse_classifications(raw: Option<&str>) -> Result<Vec<Classification>, ApiError> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let mut classifications = Vec::new();
    for member in raw.split(',') {
        let classification = match member.trim() {
            "strict_btc_orphan" => Classification::StrictBtcOrphan,
            "weak_btc_orphan" => Classification::WeakBtcOrphan,
            "excluded" => Classification::BtcStaleExcluded,
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

/// Validate `^[a-z][a-z0-9]*(-[a-z0-9]+)*(:[a-z][a-z0-9]*(-[a-z0-9]+)*){1,2}$`.
fn is_valid_source_syntax(code: &str) -> bool {
    let segments: Vec<&str> = code.split(':').collect();
    if segments.len() < 2 || segments.len() > 3 {
        return false;
    }
    segments.iter().all(|segment| is_valid_segment(segment))
}

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
            parse_kinds(Some("near,invalid_block,stale")).unwrap(),
            vec![
                ParentKind::Near,
                ParentKind::InvalidBlock,
                ParentKind::Stale
            ]
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
            parse_classifications(Some("excluded,pending")).unwrap(),
            vec![Classification::BtcStaleExcluded, Classification::Pending]
        );
        assert_eq!(parse_classifications(None).unwrap(), Vec::new());
        assert_eq!(
            code(&parse_classifications(Some("unknown")).unwrap_err()),
            "invalid_query"
        );
        assert_eq!(
            code(&parse_classifications(Some("bogus")).unwrap_err()),
            "invalid_query"
        );
    }
}
