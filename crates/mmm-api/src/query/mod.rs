//! Endpoint query parsing and validation.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};

use super::error::ApiError;
use super::normalize::{
    Classification, ParentKind, normalize_sources, parse_classifications, parse_kinds,
};

pub use navigator::{
    NavigatorAxis, NavigatorCursor, NavigatorDirection, NavigatorMode, NavigatorQuery,
    NavigatorTarget, parse_navigator_query,
};

const TREE_HEIGHT_LIMIT: u64 = 2_016;
const UNHEIGHTED_DAY_LIMIT: u64 = 31;

const NAVIGATOR_DEFAULT_LIMIT: usize = 1;
const NAVIGATOR_LIMIT_MAX: u64 = 2_000;

#[derive(Debug, Clone)]
pub struct TreeQuery {
    pub from_height: Option<i32>,
    pub to_height: Option<i32>,
    pub at_height: Option<i32>,
    pub at_time: Option<i64>,
    pub context: TreeContextPolicy,
    pub kinds: Vec<ParentKind>,
    pub source_filter: Vec<String>,
    /// Orphan-class filter for unknown parents (defaults to `strict+weak`).
    pub classification: Vec<Classification>,
    pub include_near: bool,
    pub include_unheighted: bool,
    pub unheighted_from: Option<Date>,
    pub unheighted_to: Option<Date>,
    pub unheighted_anchor: Option<String>,
    pub min_sources: usize,
    pub query: Value,
}

struct ExactTreeLookup {
    at_height: Option<i32>,
    at_time: Option<i64>,
    at_time_query: Option<String>,
}

struct TreeLookupControls {
    from_height: Option<i32>,
    to_height: Option<i32>,
    exact: ExactTreeLookup,
    context: TreeContextPolicy,
    include_near: bool,
    include_unheighted: bool,
    unheighted_from: Option<Date>,
    unheighted_to: Option<Date>,
    unheighted_anchor: Option<String>,
    min_sources: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeWindowMode {
    Tip,
    Explicit,
    Height,
    Time,
    UnheightedAnchor,
}

impl TreeWindowMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tip => "tip",
            Self::Explicit => "explicit",
            Self::Height => "height",
            Self::Time => "time",
            Self::UnheightedAnchor => "unheighted_anchor",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TreeContextPolicy {
    Exact,
    Compact,
}

impl TreeContextPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exact => "exact",
            Self::Compact => "compact",
        }
    }

    pub fn is_compact(self) -> bool {
        self == Self::Compact
    }
}

/// Parse and validate a `/api/v1/tree` query string before DB checkout.
pub fn parse_tree_query(raw: Option<&str>) -> Result<TreeQuery, ApiError> {
    let params = parse_params(
        raw,
        &[
            "from_height",
            "to_height",
            "at_height",
            "at_time",
            "context",
            "kinds",
            "classification",
            "source",
            "include_near",
            "include_unheighted",
            "unheighted_from",
            "unheighted_to",
            "unheighted_anchor",
            "min_sources",
        ],
    )?;
    let controls = parse_tree_lookup_controls(&params)?;
    let kinds = normalized_kinds(params.get("kinds").map(String::as_str))?;
    let classification =
        normalized_classifications(params.get("classification").map(String::as_str))?;
    let raw_source = params.get("source").map(String::as_str);
    let source_filter = normalize_sources(raw_source)?;
    let query_sources = query_sources(raw_source, &source_filter);
    let window_mode = tree_window_mode(&controls);
    let query = tree_query_echo(
        &controls,
        window_mode,
        &kinds,
        &classification,
        &query_sources,
    );

    Ok(TreeQuery {
        from_height: controls.from_height,
        to_height: controls.to_height,
        at_height: controls.exact.at_height,
        at_time: controls.exact.at_time,
        context: controls.context,
        kinds,
        source_filter,
        classification,
        include_near: controls.include_near,
        include_unheighted: controls.include_unheighted,
        unheighted_from: controls.unheighted_from,
        unheighted_to: controls.unheighted_to,
        unheighted_anchor: controls.unheighted_anchor,
        min_sources: controls.min_sources,
        query,
    })
}

fn parse_tree_lookup_controls(
    params: &BTreeMap<String, String>,
) -> Result<TreeLookupControls, ApiError> {
    let from_height = optional_i32(params, "from_height")?;
    let to_height = optional_i32(params, "to_height")?;
    validate_tree_height_window(from_height, to_height)?;
    let exact = parse_exact_tree_lookup(params)?;
    let include_near = optional_bool(params, "include_near")?.unwrap_or(false);
    let include_unheighted = optional_bool(params, "include_unheighted")?.unwrap_or(false);
    let context = parse_tree_context(params)?;
    let min_sources = optional_usize(params, "min_sources")?.unwrap_or(1);
    if min_sources == 0 {
        return Err(ApiError::invalid_query(
            "min_sources must be at least 1",
            json!({ "min_sources": 0 }),
        ));
    }
    let (unheighted_from, unheighted_to) = parse_unheighted_window(params, include_unheighted)?;
    let unheighted_anchor = params
        .get("unheighted_anchor")
        .map(|raw| validate_hash_param("unheighted_anchor", raw))
        .transpose()?;
    validate_tree_lookup_modes(
        from_height,
        to_height,
        exact.at_height,
        exact.at_time,
        &unheighted_anchor,
        include_unheighted,
    )?;
    validate_tree_context(
        context,
        exact.at_height,
        exact.at_time,
        include_unheighted,
        params.contains_key("unheighted_from"),
        params.contains_key("unheighted_to"),
    )?;
    Ok(TreeLookupControls {
        from_height,
        to_height,
        exact,
        context,
        include_near,
        include_unheighted,
        unheighted_from,
        unheighted_to,
        unheighted_anchor,
        min_sources,
    })
}

fn tree_window_mode(controls: &TreeLookupControls) -> TreeWindowMode {
    if controls.unheighted_anchor.is_some() {
        TreeWindowMode::UnheightedAnchor
    } else if controls.exact.at_height.is_some() {
        TreeWindowMode::Height
    } else if controls.exact.at_time.is_some() {
        TreeWindowMode::Time
    } else if controls.from_height.is_some() {
        TreeWindowMode::Explicit
    } else {
        TreeWindowMode::Tip
    }
}

fn tree_query_echo(
    controls: &TreeLookupControls,
    window_mode: TreeWindowMode,
    kinds: &[ParentKind],
    classification: &[Classification],
    query_sources: &[String],
) -> Value {
    let mut query = json!({
        "from_height": controls.from_height,
        "to_height": controls.to_height,
        "at_height": controls.exact.at_height,
        "at_time": controls.exact.at_time_query,
        "window_mode": window_mode.as_str(),
        "context": controls.context.as_str(),
        "kinds": kind_strings(kinds),
        "classification": classification_strings(classification),
        "sources": query_sources,
        "include_near": controls.include_near,
        "min_sources": controls.min_sources,
        "include_unheighted": controls.include_unheighted,
    });
    if controls.include_unheighted {
        query["unheighted_from"] = json!(date_string(controls.unheighted_from.expect("set above")));
        query["unheighted_to"] = json!(date_string(controls.unheighted_to.expect("set above")));
    }
    if let Some(anchor) = &controls.unheighted_anchor {
        query["unheighted_anchor"] = json!(anchor);
    }
    query
}

fn parse_tree_context(params: &BTreeMap<String, String>) -> Result<TreeContextPolicy, ApiError> {
    match params.get("context").map(String::as_str).unwrap_or("exact") {
        "exact" => Ok(TreeContextPolicy::Exact),
        "compact" => Ok(TreeContextPolicy::Compact),
        other => Err(ApiError::invalid_query(
            "context must be exact or compact",
            json!({ "context": other }),
        )),
    }
}

fn validate_tree_context(
    context: TreeContextPolicy,
    at_height: Option<i32>,
    at_time: Option<i64>,
    include_unheighted: bool,
    has_unheighted_from: bool,
    has_unheighted_to: bool,
) -> Result<(), ApiError> {
    if !context.is_compact() {
        return Ok(());
    }
    if at_height.is_none() && at_time.is_none() {
        return Err(ApiError::invalid_query(
            "context=compact requires at_height or at_time",
            json!({ "context": context.as_str() }),
        ));
    }
    if include_unheighted || has_unheighted_from || has_unheighted_to {
        return Err(ApiError::invalid_query(
            "context=compact cannot be combined with unheighted date-window parameters",
            json!({
                "context": context.as_str(),
                "include_unheighted": include_unheighted,
                "unheighted_from": has_unheighted_from,
                "unheighted_to": has_unheighted_to,
            }),
        ));
    }
    Ok(())
}

fn validate_tree_height_window(
    from_height: Option<i32>,
    to_height: Option<i32>,
) -> Result<(), ApiError> {
    match (from_height, to_height) {
        (Some(from_height), Some(to_height)) => {
            if from_height < 0 || to_height < 0 {
                return Err(ApiError::invalid_query(
                    "tree heights must be non-negative",
                    json!({ "from_height": from_height, "to_height": to_height }),
                ));
            }
            if from_height > to_height {
                return Err(ApiError::invalid_query(
                    "from_height must be less than or equal to to_height",
                    json!({ "from_height": from_height, "to_height": to_height }),
                ));
            }

            let height_count = (to_height as i64 - from_height as i64 + 1) as u64;
            if height_count > TREE_HEIGHT_LIMIT {
                return Err(ApiError::range_too_large(
                    "height_window",
                    TREE_HEIGHT_LIMIT,
                    height_count,
                    "requested tree window exceeds the first-wave limit",
                ));
            }
            Ok(())
        }
        (None, None) => Ok(()),
        _ => Err(ApiError::invalid_query(
            "from_height and to_height must be supplied together",
            json!({ "from_height": from_height, "to_height": to_height }),
        )),
    }
}

fn parse_exact_tree_lookup(params: &BTreeMap<String, String>) -> Result<ExactTreeLookup, ApiError> {
    let at_height = optional_i32(params, "at_height")?;
    validate_tree_at_height(at_height)?;
    let (at_time, at_time_query) = optional_utc_timestamp(params, "at_time")?;
    Ok(ExactTreeLookup {
        at_height,
        at_time,
        at_time_query,
    })
}

fn validate_tree_at_height(at_height: Option<i32>) -> Result<(), ApiError> {
    if let Some(height) = at_height
        && height < 0
    {
        return Err(ApiError::invalid_query(
            "at_height must be non-negative",
            json!({ "at_height": height }),
        ));
    }
    Ok(())
}

fn optional_utc_timestamp(
    params: &std::collections::BTreeMap<String, String>,
    key: &str,
) -> Result<(Option<i64>, Option<String>), ApiError> {
    let Some(raw) = params.get(key) else {
        return Ok((None, None));
    };
    let timestamp = parse_utc_timestamp_param(key, raw)?;
    Ok((Some(timestamp), Some(raw.to_owned())))
}

/// Parse a strict `YYYY-MM-DDTHH:MM:SSZ` UTC timestamp to unix epoch seconds.
fn parse_utc_timestamp_param(key: &str, raw: &str) -> Result<i64, ApiError> {
    let bytes = raw.as_bytes();
    let valid_shape = bytes.len() == 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19) || byte.is_ascii_digit()
        });
    if !valid_shape {
        return Err(invalid_timestamp(key, raw));
    }
    OffsetDateTime::parse(raw, &Rfc3339)
        .map(|dt| dt.unix_timestamp())
        .map_err(|_| invalid_timestamp(key, raw))
}

fn invalid_timestamp(key: &str, raw: &str) -> ApiError {
    ApiError::invalid_query(
        format!("{key} must be a UTC timestamp in YYYY-MM-DDTHH:MM:SSZ form"),
        json!({ key: raw }),
    )
}

fn parse_unheighted_window(
    params: &std::collections::BTreeMap<String, String>,
    include_unheighted: bool,
) -> Result<(Option<Date>, Option<Date>), ApiError> {
    let supplied_unheighted_from = params
        .contains_key("unheighted_from")
        .then(|| required_date(params, "unheighted_from"))
        .transpose()?;
    let supplied_unheighted_to = params
        .contains_key("unheighted_to")
        .then(|| required_date(params, "unheighted_to"))
        .transpose()?;
    if include_unheighted {
        let from = supplied_unheighted_from.ok_or_else(|| {
            ApiError::invalid_query(
                "unheighted_from is required when include_unheighted=true",
                json!({ "unheighted_from": null }),
            )
        })?;
        let to = supplied_unheighted_to.ok_or_else(|| {
            ApiError::invalid_query(
                "unheighted_to is required when include_unheighted=true",
                json!({ "unheighted_to": null }),
            )
        })?;
        if from > to {
            return Err(ApiError::invalid_query(
                "unheighted_from must be less than or equal to unheighted_to",
                json!({
                    "unheighted_from": date_string(from),
                    "unheighted_to": date_string(to),
                }),
            ));
        }
        let days = inclusive_day_span(from, to);
        if days > UNHEIGHTED_DAY_LIMIT {
            return Err(ApiError::range_too_large(
                "unheighted_days",
                UNHEIGHTED_DAY_LIMIT,
                days,
                "requested unheighted tree window exceeds the first-wave limit",
            ));
        }
        Ok((Some(from), Some(to)))
    } else {
        Ok((None, None))
    }
}

fn validate_tree_lookup_modes(
    from_height: Option<i32>,
    to_height: Option<i32>,
    at_height: Option<i32>,
    at_time: Option<i64>,
    unheighted_anchor: &Option<String>,
    include_unheighted: bool,
) -> Result<(), ApiError> {
    let explicit_window = from_height.is_some() || to_height.is_some();
    let lookup_count = usize::from(explicit_window)
        + usize::from(at_height.is_some())
        + usize::from(at_time.is_some())
        + usize::from(unheighted_anchor.is_some());
    if lookup_count > 1 {
        return Err(ApiError::invalid_query(
            "tree lookup modes are mutually exclusive",
            json!({
                "from_height": from_height,
                "to_height": to_height,
                "at_height": at_height,
                "at_time": at_time,
                "unheighted_anchor": unheighted_anchor,
            }),
        ));
    }
    if unheighted_anchor.is_some() && include_unheighted {
        return Err(ApiError::invalid_query(
            "unheighted_anchor cannot be combined with include_unheighted",
            json!({ "unheighted_anchor": unheighted_anchor }),
        ));
    }
    Ok(())
}

mod navigator;
mod params;
#[cfg(test)]
mod tests;

pub use params::*;
