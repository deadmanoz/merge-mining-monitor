//! Shared query-parameter primitives: date/int/hash/bool parsing and kind/classification normalization.

use super::*;
use crate::normalize::lowercase_hex_64;

pub(crate) fn date_string(date: Date) -> String {
    date.to_string()
}

pub(crate) fn date_from_epoch(epoch: i64) -> anyhow::Result<Date> {
    Ok(OffsetDateTime::from_unix_timestamp(epoch)?.date())
}

pub fn epoch_start_of_day(date: Date) -> i64 {
    date.with_hms(0, 0, 0)
        .expect("midnight is valid")
        .assume_utc()
        .unix_timestamp()
}

pub(crate) fn kind_as_str(kind: ParentKind) -> &'static str {
    match kind {
        ParentKind::Canonical => "canonical",
        ParentKind::InvalidBlock => "invalid_block",
        ParentKind::Near => "near",
        ParentKind::Stale => "stale",
        ParentKind::Unknown => "unknown",
    }
}

pub(crate) fn kind_strings(kinds: &[ParentKind]) -> Vec<&'static str> {
    kinds.iter().copied().map(kind_as_str).collect()
}

pub(crate) fn kind_selected(kinds: &[ParentKind], kind: ParentKind) -> bool {
    kinds.contains(&kind)
}

pub(crate) fn classification_strings(classifications: &[Classification]) -> Vec<&'static str> {
    classifications
        .iter()
        .copied()
        .map(Classification::as_str)
        .collect()
}

/// Decode a query string into a key->value map, rejecting malformed syntax or unknown keys.
pub(crate) fn parse_params(
    raw: Option<&str>,
    accepted: &[&str],
) -> Result<BTreeMap<String, String>, ApiError> {
    let mut params = BTreeMap::new();
    let Some(raw) = raw else {
        return Ok(params);
    };
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(raw).map_err(|_| {
        ApiError::invalid_query("query string is malformed", json!({ "query": raw }))
    })?;
    for (key, value) in pairs {
        if !accepted.contains(&key.as_str()) {
            return Err(ApiError::invalid_query(
                format!("unknown query parameter {key:?}"),
                json!({ "parameter": key }),
            ));
        }
        params.insert(key, value);
    }
    Ok(params)
}

pub(crate) fn required_date(
    params: &BTreeMap<String, String>,
    key: &str,
) -> Result<Date, ApiError> {
    let raw = params.get(key).ok_or_else(|| {
        ApiError::invalid_query(format!("{key} is required"), json!({ key: null }))
    })?;
    parse_date_param(key, raw)
}

pub(crate) fn parse_date_param(key: &str, raw: &str) -> Result<Date, ApiError> {
    let valid_shape = raw.len() == 10
        && raw.as_bytes()[4] == b'-'
        && raw.as_bytes()[7] == b'-'
        && raw
            .as_bytes()
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    if !valid_shape {
        return Err(invalid_date(key, raw));
    }
    let year = raw[0..4].parse::<i32>().ok();
    let month = raw[5..7].parse::<u8>().ok();
    let day = raw[8..10].parse::<u8>().ok();
    let (Some(year), Some(month), Some(day)) = (year, month, day) else {
        return Err(invalid_date(key, raw));
    };
    if !(1..=9998).contains(&year) {
        return Err(invalid_date(key, raw));
    }
    let Ok(month) = Month::try_from(month) else {
        return Err(invalid_date(key, raw));
    };
    Date::from_calendar_date(year, month, day).map_err(|_| invalid_date(key, raw))
}

pub(crate) fn invalid_date(key: &str, raw: &str) -> ApiError {
    ApiError::invalid_query(
        format!("{key} must be a UTC date in YYYY-MM-DD form"),
        json!({ key: raw }),
    )
}

pub(crate) fn optional_i32(
    params: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<i32>, ApiError> {
    let Some(raw) = params.get(key) else {
        return Ok(None);
    };
    raw.parse::<i32>().map(Some).map_err(|_| {
        ApiError::invalid_query(
            format!("{key} must be a base-10 integer"),
            json!({ key: raw }),
        )
    })
}

pub(crate) fn optional_usize(
    params: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<usize>, ApiError> {
    let Some(raw) = params.get(key) else {
        return Ok(None);
    };
    raw.parse::<usize>().map(Some).map_err(|_| {
        ApiError::invalid_query(
            format!("{key} must be a base-10 positive integer"),
            json!({ key: raw }),
        )
    })
}

pub(crate) fn validate_hash_param(key: &str, raw: &str) -> Result<String, ApiError> {
    lowercase_hex_64(raw).ok_or_else(|| {
        ApiError::invalid_query(
            format!("{key} must be a 64-character hex hash"),
            json!({ key: raw }),
        )
    })
}

pub(crate) fn optional_bool(
    params: &BTreeMap<String, String>,
    key: &str,
) -> Result<Option<bool>, ApiError> {
    let Some(raw) = params.get(key) else {
        return Ok(None);
    };
    match raw.as_str() {
        "true" => Ok(Some(true)),
        "false" => Ok(Some(false)),
        _ => Err(ApiError::invalid_query(
            format!("{key} must be true or false"),
            json!({ key: raw }),
        )),
    }
}

/// The tree's node kinds, in canonical order. A near parent is not a tree node: its page is
/// `/api/v1/block/{hash}`.
const TREE_KINDS: [ParentKind; 4] = [
    ParentKind::Canonical,
    ParentKind::InvalidBlock,
    ParentKind::Stale,
    ParentKind::Unknown,
];

pub(crate) fn normalized_kinds(raw: Option<&str>) -> Result<Vec<ParentKind>, ApiError> {
    let parsed = parse_kinds(raw)?;
    if parsed.contains(&ParentKind::Near) {
        return Err(ApiError::invalid_query(
            "near parents are not tree nodes; open /api/v1/block/{hash}",
            json!({ "kinds": raw }),
        ));
    }
    Ok(TREE_KINDS
        .into_iter()
        .filter(|kind| parsed.is_empty() || parsed.contains(kind))
        .collect())
}

/// Default the orphan-class filter to `strict+weak` when `classification=` is absent,
/// deduplicated in fixed order `(strict, weak, excluded, pending)`.
pub(crate) fn normalized_classifications(
    raw: Option<&str>,
) -> Result<Vec<Classification>, ApiError> {
    let parsed = parse_classifications(raw)?;
    let values = if parsed.is_empty() {
        vec![
            Classification::StrictBtcOrphan,
            Classification::WeakBtcOrphan,
        ]
    } else {
        parsed
    };
    Ok([
        Classification::StrictBtcOrphan,
        Classification::WeakBtcOrphan,
        Classification::BtcStaleExcluded,
        Classification::Pending,
    ]
    .into_iter()
    .filter(|class| values.contains(class))
    .collect())
}

pub(crate) fn query_sources(raw_source: Option<&str>, source_filter: &[String]) -> Vec<String> {
    raw_source.map_or_else(Vec::new, |_| source_filter.to_vec())
}

pub(crate) fn inclusive_day_span(from: Date, to: Date) -> u64 {
    (to.to_julian_day() - from.to_julian_day() + 1) as u64
}
