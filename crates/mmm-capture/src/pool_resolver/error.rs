//! Shared validation/parse error enum for the BTC pool snapshot and RSK miner identity registry.

use std::error::Error;
use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum PoolResolverError {
    InvalidSnapshotJson(String),
    InvalidRegistryJson(String),
    UnsupportedSchemaVersion(u32),
    EmptyValue {
        field: &'static str,
        pool_slug: String,
    },
    DuplicateValue {
        field: &'static str,
        value: String,
        first_pool: String,
        duplicate_pool: String,
    },
    InvalidRskMinerAddress {
        value: String,
        pool_slug: String,
    },
    WhitespaceValue {
        field: &'static str,
        pool_slug: String,
    },
    SlugCanonicalNameConflict {
        slug: String,
        first_canonical_name: String,
        duplicate_canonical_name: String,
    },
}

impl fmt::Display for PoolResolverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSnapshotJson(err) => {
                write!(formatter, "invalid pool snapshot JSON: {err}")
            }
            Self::InvalidRegistryJson(err) => {
                write!(formatter, "invalid pool identity registry JSON: {err}")
            }
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported pool snapshot schema version {version}"
                )
            }
            Self::EmptyValue { field, pool_slug } => {
                write!(formatter, "pool {pool_slug} has an empty {field} value")
            }
            Self::DuplicateValue {
                field,
                value,
                first_pool,
                duplicate_pool,
            } => write!(
                formatter,
                "duplicate {field} value {value:?} in pools {first_pool} and {duplicate_pool}"
            ),
            Self::InvalidRskMinerAddress { value, pool_slug } => write!(
                formatter,
                "invalid RSK miner address {value:?} for pool {pool_slug}: \
                 expected 40 hex characters (optionally prefixed with 0x)"
            ),
            Self::WhitespaceValue { field, pool_slug } => write!(
                formatter,
                "pool {pool_slug} has surrounding whitespace in its {field} value"
            ),
            Self::SlugCanonicalNameConflict {
                slug,
                first_canonical_name,
                duplicate_canonical_name,
            } => write!(
                formatter,
                "RSK miner registry assigns pool slug {slug} two distinct \
                 canonical_name values: {first_canonical_name:?} and {duplicate_canonical_name:?}"
            ),
        }
    }
}

impl Error for PoolResolverError {}
