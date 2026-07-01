//! Shared query-parameter primitives: date/int/hash/bool parsing and
//! kind/classification normalization.

use super::*;

/// Render a `Date` as the `YYYY-MM-DD` string used in the unheighted-window
/// query echo (the tree.json wire form).
pub(crate) fn date_string(date: Date) -> String {
    date.to_string()
}

/// Convert a unix epoch (a block's `btc_header_time`) to its UTC calendar
/// `Date`. Used by the unheighted-anchor projection to locate the anchor's day.
pub(crate) fn date_from_epoch(epoch: i64) -> anyhow::Result<Date> {
    Ok(OffsetDateTime::from_unix_timestamp(epoch)?.date())
}

/// The unix epoch at UTC midnight starting `date`. Used to translate the
/// unheighted date window into the inclusive `[start, start+86_399]`
/// btc_header_time bounds in the tree window SQL.
pub fn epoch_start_of_day(date: Date) -> i64 {
    date.with_hms(0, 0, 0)
        .expect("midnight is valid")
        .assume_utc()
        .unix_timestamp()
}

/// The serialized parent-`kind` string (canonical/near/stale/unknown) emitted
/// in tree and block payloads; byte-locked by fixtures/api/tree.json and
/// block-*.json.
pub(crate) fn kind_as_str(kind: ParentKind) -> &'static str {
    match kind {
        ParentKind::Canonical => "canonical",
        ParentKind::Near => "near",
        ParentKind::Stale => "stale",
        ParentKind::Unknown => "unknown",
    }
}

/// Map the selected parent kinds to their wire strings for the `kinds` array in
/// the tree.json query echo.
pub(crate) fn kind_strings(kinds: &[ParentKind]) -> Vec<&'static str> {
    kinds.iter().copied().map(kind_as_str).collect()
}

/// Whether `kind` is in the request's selected parent-kind filter. The tree
/// projection uses it to decide whether a row is included or rendered as trimmed
/// context.
pub(crate) fn kind_selected(kinds: &[ParentKind], kind: ParentKind) -> bool {
    kinds.contains(&kind)
}

/// Map the resolved orphan-class filter to its wire strings for the
/// `classification` array echoed in tree.json and the orphan navigator
/// fixture payloads.
pub(crate) fn classification_strings(classifications: &[Classification]) -> Vec<&'static str> {
    classifications
        .iter()
        .copied()
        .map(Classification::as_str)
        .collect()
}

/// Decode a query string into a key->value map, rejecting an undecodable string
/// or any key not in `accepted` as invalid_query. The strict allowlist gate
/// every endpoint parser runs first, so unknown params fail before DB checkout.
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

/// Fetch and parse a required `YYYY-MM-DD` date param; a missing key is
/// invalid_query. Used for the unheighted-window bounds.
pub(crate) fn required_date(
    params: &BTreeMap<String, String>,
    key: &str,
) -> Result<Date, ApiError> {
    let raw = params.get(key).ok_or_else(|| {
        ApiError::invalid_query(format!("{key} is required"), json!({ key: null }))
    })?;
    parse_date_param(key, raw)
}

/// Parse a strict `YYYY-MM-DD` calendar date: fixed-position dash check,
/// two-digit month/day, year 1..=9998, and real-calendar validity. Any failure
/// is invalid_query via `invalid_date`.
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

/// Build the invalid_query error for a malformed date param, echoing the
/// offending value under its key.
pub(crate) fn invalid_date(key: &str, raw: &str) -> ApiError {
    ApiError::invalid_query(
        format!("{key} must be a UTC date in YYYY-MM-DD form"),
        json!({ key: raw }),
    )
}

/// Parse an optional base-10 `i32` param (height fields); a non-integer value is
/// invalid_query. Absent key yields `None`.
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

/// Parse an optional base-10 `usize` param (limit, min_sources); a non-integer
/// value is invalid_query. The caller enforces the lower/upper bounds.
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

/// Validate a query-param hash as 64 lowercase-normalized hex. Unlike
/// `normalize::normalize_hash` (which yields `invalid_hash` for the path-hash
/// route), a malformed query parameter is `invalid_query`.
pub(crate) fn validate_hash_param(key: &str, raw: &str) -> Result<String, ApiError> {
    if raw.len() == 64 && raw.chars().all(|ch| ch.is_ascii_hexdigit()) {
        Ok(raw.to_ascii_lowercase())
    } else {
        Err(ApiError::invalid_query(
            format!("{key} must be a 64-character hex hash"),
            json!({ key: raw }),
        ))
    }
}

/// Parse an optional strict `true`/`false` param (include_near,
/// include_unheighted); any other value is invalid_query. Absent key yields
/// `None`.
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

/// Resolve the `kinds=` filter: absent or empty defaults to all four parent
/// kinds, then dedup into a deterministic name order. Mirrors
/// `normalized_classifications`.
pub(crate) fn normalized_kinds(raw: Option<&str>) -> Result<Vec<ParentKind>, ApiError> {
    let parsed = parse_kinds(raw)?;
    let values = if parsed.is_empty() {
        vec![
            ParentKind::Canonical,
            ParentKind::Near,
            ParentKind::Stale,
            ParentKind::Unknown,
        ]
    } else {
        parsed
    };
    let mut by_name = BTreeSet::new();
    for kind in values {
        by_name.insert(kind_as_str(kind));
    }
    Ok(by_name
        .into_iter()
        .map(|name| match name {
            "canonical" => ParentKind::Canonical,
            "near" => ParentKind::Near,
            "stale" => ParentKind::Stale,
            "unknown" => ParentKind::Unknown,
            _ => unreachable!("inserted from known enum"),
        })
        .collect())
}

/// Default the orphan-class filter to the navigable signal (strict+weak) when the
/// `classification=` param is absent, else keep the requested members. The result
/// is deduplicated into a deterministic order (strict, weak, excluded, pending),
/// mirroring `normalized_kinds`.
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

/// The `sources` value for the query echo: the normalized filter when a
/// `source=` param was present, else an empty array. Keeps an absent filter and
/// an explicit empty filter distinguishable in the echoed block.
pub(crate) fn query_sources(raw_source: Option<&str>, source_filter: &[String]) -> Vec<String> {
    raw_source.map_or_else(Vec::new, |_| source_filter.to_vec())
}

/// Inclusive day count between two dates (both endpoints counted). Used to
/// enforce the unheighted-window day limit.
pub(crate) fn inclusive_day_span(from: Date, to: Date) -> u64 {
    (to.to_julian_day() - from.to_julian_day() + 1) as u64
}
