//! Shared error enum for the BTC pool resolver and the RSK pool identity
//! registry. Lives in a sibling module so neither concern depends on the
//! other through the error type.

use std::error::Error;
use std::fmt;

/// Validation/parse failures shared by the BTC pool snapshot and the RSK miner
/// identity registry. These mirror the DB invariants the embedded fixtures must
/// satisfy (unique slug/tag/address, valid RSK address, consistent slug -> name)
/// so a bad fixture fails at load, before any write.
#[derive(Debug, PartialEq, Eq)]
pub enum PoolResolverError {
    /// The pool snapshot JSON failed to deserialize; carries the serde error string.
    InvalidSnapshotJson(String),
    /// The RSK miner registry JSON failed to deserialize; carries the serde error string.
    InvalidRegistryJson(String),
    /// Snapshot or registry declared a `schema_version` other than 1.
    UnsupportedSchemaVersion(u32),
    /// A required string field (slug, canonical_name, tag, address, miner_address)
    /// was empty after trimming. `field` is the static field name, `pool_slug` the
    /// owning pool.
    EmptyValue {
        field: &'static str,
        pool_slug: String,
    },
    /// Two pools claim the same slug / coinbase tag / payout address / RSK miner
    /// address. Mirrors the DB uniqueness constraints; `field` names which, with
    /// the two owning pools.
    DuplicateValue {
        field: &'static str,
        value: String,
        first_pool: String,
        duplicate_pool: String,
    },
    /// An RSK miner address was not 40 hex chars after normalization (lower-cased,
    /// `0x` stripped). Guards the `rsk_miner_address` namespace before any
    /// pool_identity insert.
    InvalidRskMinerAddress { value: String, pool_slug: String },
    /// A required string field had surrounding whitespace (would silently fork a
    /// slug or break a byte-exact key). `field` is the static field name,
    /// `pool_slug` the owning pool.
    WhitespaceValue {
        field: &'static str,
        pool_slug: String,
    },
    /// The RSK registry maps one `pool_slug` to two different `canonical_name`
    /// values, which would write inconsistent pool rows.
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
