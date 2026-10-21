//! Generic pool-identity registry validation and seed preparation.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, Copy)]
pub struct IdentityRegistryEntry<'a> {
    pub identifier: &'a str,
    pub pool_slug: &'a str,
    pub pool_canonical_name: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityRegistryError {
    UnsupportedSchemaVersion(u32),
    EmptyField {
        field: &'static str,
        pool_slug: String,
    },
    WhitespaceField {
        field: &'static str,
        pool_slug: String,
    },
    DuplicateIdentifier {
        field: &'static str,
        value: String,
        first_pool: String,
        duplicate_pool: String,
    },
    SlugCanonicalNameConflict {
        slug: String,
        first_canonical_name: String,
        duplicate_canonical_name: String,
    },
    InvalidIdentifier {
        field: &'static str,
        value: String,
        pool_slug: String,
        reason: String,
    },
}

impl fmt::Display for IdentityRegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported identity registry schema_version {version}"
                )
            }
            Self::EmptyField { field, pool_slug } => {
                write!(
                    formatter,
                    "identity registry {field} is empty for pool {pool_slug}"
                )
            }
            Self::WhitespaceField { field, pool_slug } => write!(
                formatter,
                "identity registry {field} has surrounding whitespace for pool {pool_slug}"
            ),
            Self::DuplicateIdentifier {
                field,
                value,
                first_pool,
                duplicate_pool,
            } => write!(
                formatter,
                "duplicate identity registry {field} {value:?} in pools {first_pool} and {duplicate_pool}"
            ),
            Self::SlugCanonicalNameConflict {
                slug,
                first_canonical_name,
                duplicate_canonical_name,
            } => write!(
                formatter,
                "identity registry slug {slug} has conflicting canonical names \
                 {first_canonical_name:?} and {duplicate_canonical_name:?}"
            ),
            Self::InvalidIdentifier {
                field,
                value,
                pool_slug,
                reason,
            } => write!(
                formatter,
                "invalid identity registry {field} {value:?} for pool {pool_slug}: {reason}"
            ),
        }
    }
}

impl Error for IdentityRegistryError {}

/// Validate `schema_version == 1`, non-empty trimmed fields, per-chain identifier format,
/// global identifier uniqueness after `identifier_key`, and consistent `pool_slug -> pool_canonical_name`.
pub fn validate_identity_registry<'a, E>(
    schema_version: u32,
    entries: E,
    identifier_field: &'static str,
    identifier_validator: impl Fn(&str) -> Result<(), String>,
    identifier_key: impl Fn(&str) -> String,
) -> Result<(), IdentityRegistryError>
where
    E: IntoIterator<Item = IdentityRegistryEntry<'a>>,
{
    if schema_version != 1 {
        return Err(IdentityRegistryError::UnsupportedSchemaVersion(
            schema_version,
        ));
    }

    let mut identifier_owners: HashMap<String, String> = HashMap::new();
    let mut slug_canonical: HashMap<String, String> = HashMap::new();

    for entry in entries {
        validate_field(identifier_field, entry.identifier, entry.pool_slug)?;
        validate_field("pool_slug", entry.pool_slug, entry.pool_slug)?;
        validate_field(
            "pool_canonical_name",
            entry.pool_canonical_name,
            entry.pool_slug,
        )?;

        if let Err(reason) = identifier_validator(entry.identifier) {
            return Err(IdentityRegistryError::InvalidIdentifier {
                field: identifier_field,
                value: entry.identifier.to_owned(),
                pool_slug: entry.pool_slug.to_owned(),
                reason,
            });
        }

        let key = identifier_key(entry.identifier);
        if let Some(first_pool) = identifier_owners.insert(key.clone(), entry.pool_slug.to_owned())
        {
            return Err(IdentityRegistryError::DuplicateIdentifier {
                field: identifier_field,
                value: key,
                first_pool,
                duplicate_pool: entry.pool_slug.to_owned(),
            });
        }

        if let Some(existing) = slug_canonical.get(entry.pool_slug) {
            if existing != entry.pool_canonical_name {
                return Err(IdentityRegistryError::SlugCanonicalNameConflict {
                    slug: entry.pool_slug.to_owned(),
                    first_canonical_name: existing.clone(),
                    duplicate_canonical_name: entry.pool_canonical_name.to_owned(),
                });
            }
        } else {
            slug_canonical.insert(
                entry.pool_slug.to_owned(),
                entry.pool_canonical_name.to_owned(),
            );
        }
    }

    Ok(())
}

/// Distinct `(pool_slug, pool_canonical_name)` pairs in first-seen order.
pub fn distinct_pool_definitions<'a, E>(entries: E) -> Vec<(&'a str, &'a str)>
where
    E: IntoIterator<Item = IdentityRegistryEntry<'a>>,
{
    let mut seen: HashMap<&str, ()> = HashMap::new();
    let mut order = Vec::new();
    for entry in entries {
        if seen.insert(entry.pool_slug, ()).is_none() {
            order.push((entry.pool_slug, entry.pool_canonical_name));
        }
    }
    order
}

/// A registry evidence date (`YYYY-MM-DD`), checked against the calendar.
pub fn parse_registry_date(value: &str) -> Result<time::Date, String> {
    let format =
        time::format_description::parse("[year]-[month]-[day]").expect("static date format parses");
    time::Date::parse(value, &format)
        .map_err(|err| format!("invalid calendar date {value:?}: {err}"))
}

pub fn identity_key(identifier: &str) -> String {
    identifier.to_owned()
}

pub fn accept_any_identifier(_identifier: &str) -> Result<(), String> {
    Ok(())
}

fn validate_field(
    field: &'static str,
    value: &str,
    pool_slug: &str,
) -> Result<(), IdentityRegistryError> {
    if value.trim().is_empty() {
        return Err(IdentityRegistryError::EmptyField {
            field,
            pool_slug: pool_slug.to_owned(),
        });
    }
    if value != value.trim() {
        return Err(IdentityRegistryError::WhitespaceField {
            field,
            pool_slug: pool_slug.to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(id: &'a str, slug: &'a str, name: &'a str) -> IdentityRegistryEntry<'a> {
        IdentityRegistryEntry {
            identifier: id,
            pool_slug: slug,
            pool_canonical_name: name,
        }
    }

    #[test]
    fn accepts_a_valid_registry() {
        let entries = vec![
            entry("a", "f2pool", "F2Pool"),
            entry("b", "antpool", "AntPool"),
        ];
        validate_identity_registry(
            1,
            entries,
            "identifier",
            accept_any_identifier,
            identity_key,
        )
        .unwrap();
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let err = validate_identity_registry(
            2,
            Vec::<IdentityRegistryEntry>::new(),
            "identifier",
            accept_any_identifier,
            identity_key,
        )
        .unwrap_err();
        assert_eq!(err, IdentityRegistryError::UnsupportedSchemaVersion(2));
    }

    #[test]
    fn rejects_empty_and_whitespace_fields() {
        let empty = validate_identity_registry(
            1,
            vec![entry("", "f2pool", "F2Pool")],
            "identifier",
            accept_any_identifier,
            identity_key,
        )
        .unwrap_err();
        assert!(matches!(empty, IdentityRegistryError::EmptyField { .. }));

        let ws = validate_identity_registry(
            1,
            vec![entry("a", " f2pool", "F2Pool")],
            "identifier",
            accept_any_identifier,
            identity_key,
        )
        .unwrap_err();
        assert!(matches!(
            ws,
            IdentityRegistryError::WhitespaceField {
                field: "pool_slug",
                ..
            }
        ));
    }

    #[test]
    fn rejects_duplicate_identifier_after_normalization() {
        let err = validate_identity_registry(
            1,
            vec![
                entry("ABC", "f2pool", "F2Pool"),
                entry("abc", "antpool", "AntPool"),
            ],
            "miner_address",
            accept_any_identifier,
            |id| id.to_ascii_lowercase(),
        )
        .unwrap_err();
        assert_eq!(
            err,
            IdentityRegistryError::DuplicateIdentifier {
                field: "miner_address",
                value: "abc".to_owned(),
                first_pool: "f2pool".to_owned(),
                duplicate_pool: "antpool".to_owned(),
            }
        );
    }

    #[test]
    fn rejects_slug_canonical_conflict() {
        let err = validate_identity_registry(
            1,
            vec![
                entry("a", "f2pool", "F2Pool"),
                entry("b", "f2pool", "Discus Fish"),
            ],
            "identifier",
            accept_any_identifier,
            identity_key,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            IdentityRegistryError::SlugCanonicalNameConflict { .. }
        ));
    }

    #[test]
    fn surfaces_identifier_validator_reason() {
        let err = validate_identity_registry(
            1,
            vec![entry("nope", "f2pool", "F2Pool")],
            "miner_address",
            |_| Err("expected 40 hex characters".to_owned()),
            identity_key,
        )
        .unwrap_err();
        assert_eq!(
            err,
            IdentityRegistryError::InvalidIdentifier {
                field: "miner_address",
                value: "nope".to_owned(),
                pool_slug: "f2pool".to_owned(),
                reason: "expected 40 hex characters".to_owned(),
            }
        );
    }

    #[test]
    fn distinct_pool_definitions_preserves_first_seen_order() {
        let entries = vec![
            entry("a", "f2pool", "F2Pool"),
            entry("b", "antpool", "AntPool"),
            entry("c", "f2pool", "F2Pool"),
        ];
        assert_eq!(
            distinct_pool_definitions(entries),
            vec![("f2pool", "F2Pool"), ("antpool", "AntPool")]
        );
    }
}
