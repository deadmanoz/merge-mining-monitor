use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{
    ApiError, NAVIGATOR_DEFAULT_LIMIT, NAVIGATOR_LIMIT_MAX, classification_strings,
    normalized_classifications, optional_usize, parse_params, validate_hash_param,
};
use crate::normalize::Classification;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NavigatorTarget {
    Stale,
    StaleBranch,
    Orphan,
    OrphanBranch,
}

impl NavigatorTarget {
    pub fn parse(raw: &str) -> Result<Self, ApiError> {
        match raw {
            "stale" => Ok(Self::Stale),
            "stale-branch" => Ok(Self::StaleBranch),
            "orphan" => Ok(Self::Orphan),
            "orphan-branch" => Ok(Self::OrphanBranch),
            other => Err(ApiError::invalid_query(
                format!("unknown navigator target {other:?}"),
                json!({ "target": other }),
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stale => "stale",
            Self::StaleBranch => "stale-branch",
            Self::Orphan => "orphan",
            Self::OrphanBranch => "orphan-branch",
        }
    }

    pub fn accepts_classification(self) -> bool {
        matches!(self, Self::Orphan | Self::OrphanBranch)
    }

    fn axis(self) -> NavigatorAxis {
        match self {
            Self::Stale | Self::StaleBranch => NavigatorAxis::Height,
            Self::Orphan | Self::OrphanBranch => NavigatorAxis::Time,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavigatorDirection {
    Older,
    Newer,
}

impl NavigatorDirection {
    fn parse(raw: &str) -> Result<Self, ApiError> {
        match raw {
            "older" => Ok(Self::Older),
            "newer" => Ok(Self::Newer),
            other => Err(ApiError::invalid_query(
                "direction must be older or newer",
                json!({ "direction": other }),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NavigatorMode {
    Latest,
    Page {
        direction: NavigatorDirection,
        cursor: NavigatorCursor,
    },
    Anchor {
        hash: String,
    },
}

#[derive(Debug, Clone)]
pub struct NavigatorQuery {
    pub target: NavigatorTarget,
    pub mode: NavigatorMode,
    pub classification: Vec<Classification>,
    pub limit: usize,
    pub query: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NavigatorCursor {
    pub target: NavigatorTarget,
    pub axis: NavigatorAxis,
    pub min: i64,
    pub max: i64,
    pub hash: String,
}

impl NavigatorCursor {
    pub fn new(
        target: NavigatorTarget,
        axis: NavigatorAxis,
        min: i64,
        max: i64,
        hash: impl Into<String>,
    ) -> Self {
        Self {
            target,
            axis,
            min,
            max,
            hash: hash.into(),
        }
    }

    pub fn encode(&self) -> String {
        let json = serde_json::to_vec(self).expect("navigator cursor serializes");
        hex::encode(json)
    }

    fn decode(raw: &str, target: NavigatorTarget) -> Result<Self, ApiError> {
        let bytes = hex::decode(raw).map_err(|_| {
            ApiError::invalid_query("cursor is malformed", json!({ "cursor": raw }))
        })?;
        let cursor: Self = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError::invalid_query("cursor is malformed", json!({ "cursor": raw }))
        })?;
        if cursor.target != target {
            return Err(ApiError::invalid_query(
                "cursor target does not match request target",
                json!({
                    "cursor_target": cursor.target.as_str(),
                    "target": target.as_str(),
                }),
            ));
        }
        if cursor.axis != target.axis() {
            return Err(ApiError::invalid_query(
                "cursor axis does not match request target",
                json!({
                    "cursor_axis": cursor.axis,
                    "target": target.as_str(),
                }),
            ));
        }
        if cursor.min < 0 || cursor.max < 0 || cursor.min > cursor.max {
            return Err(ApiError::invalid_query(
                "cursor bounds are invalid",
                json!({
                    "min": cursor.min,
                    "max": cursor.max,
                }),
            ));
        }
        validate_hash_param("cursor.hash", &cursor.hash)?;
        Ok(cursor)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NavigatorAxis {
    Height,
    Time,
}

pub fn parse_navigator_query(
    target: NavigatorTarget,
    raw: Option<&str>,
) -> Result<NavigatorQuery, ApiError> {
    let params = parse_params(
        raw,
        &[
            "limit",
            "cursor",
            "direction",
            "anchor_hash",
            "classification",
        ],
    )?;

    let limit = match optional_usize(&params, "limit")? {
        Some(0) => {
            return Err(ApiError::invalid_query(
                "limit must be at least 1",
                json!({ "limit": 0 }),
            ));
        }
        Some(value) if value as u64 > NAVIGATOR_LIMIT_MAX => {
            return Err(ApiError::range_too_large(
                "limit",
                NAVIGATOR_LIMIT_MAX,
                value as u64,
                "requested navigator page size exceeds the limit",
            ));
        }
        Some(value) => value,
        None => NAVIGATOR_DEFAULT_LIMIT,
    };

    if !target.accepts_classification() && params.contains_key("classification") {
        return Err(ApiError::invalid_query(
            "classification applies only to orphan navigator targets",
            json!({ "target": target.as_str(), "classification": params.get("classification") }),
        ));
    }
    let classification = if target.accepts_classification() {
        normalized_classifications(params.get("classification").map(String::as_str))?
    } else {
        Vec::new()
    };

    let raw_cursor = params.get("cursor");
    let raw_direction = params.get("direction");
    let anchor_hash = params
        .get("anchor_hash")
        .map(|raw| validate_hash_param("anchor_hash", raw))
        .transpose()?;

    let mode = match (raw_cursor, raw_direction, anchor_hash) {
        (None, None, None) => NavigatorMode::Latest,
        (Some(raw_cursor), Some(raw_direction), None) => NavigatorMode::Page {
            direction: NavigatorDirection::parse(raw_direction)?,
            cursor: NavigatorCursor::decode(raw_cursor, target)?,
        },
        (None, None, Some(hash)) => NavigatorMode::Anchor { hash },
        (Some(_), Some(_), Some(hash)) => {
            return Err(ApiError::invalid_query(
                "cursor and anchor_hash are mutually exclusive",
                json!({ "anchor_hash": hash }),
            ));
        }
        (Some(_), None, _) | (None, Some(_), _) => {
            return Err(ApiError::invalid_query(
                "cursor and direction must be supplied together",
                json!({
                    "cursor": raw_cursor,
                    "direction": raw_direction,
                }),
            ));
        }
    };

    let query = json!({
        "target": target.as_str(),
        "mode": match &mode {
            NavigatorMode::Latest => "latest",
            NavigatorMode::Page { .. } => "page",
            NavigatorMode::Anchor { .. } => "anchor",
        },
        "cursor": raw_cursor,
        "direction": raw_direction,
        "anchor_hash": match &mode {
            NavigatorMode::Anchor { hash } => Some(hash.as_str()),
            _ => None,
        },
        "classification": classification_strings(&classification),
        "limit": limit,
    });

    Ok(NavigatorQuery {
        target,
        mode,
        classification,
        limit,
        query,
    })
}
