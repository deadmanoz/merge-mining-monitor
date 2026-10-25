//! Pool snapshot seeding and pool-identity upserts/lookups.

use std::collections::HashMap;

use anyhow::{Context, Result};
use tokio_postgres::types::Json;
use tokio_postgres::{Client, GenericClient};

use mmm_capture::child_payout::{PoolIdentityLookup, PoolIdentityRef};
use mmm_capture::identity_registry::{IdentityRegistryEntry, distinct_pool_definitions};
use mmm_capture::pool_resolver::PoolSnapshot;

use crate::relationship_queue::QUEUE_CHANGED_IDENTITIES;

/// One namespaced identity-to-pool mapping to upsert: `identifier` is the
/// case-/format-preserved key stored in `pool_identity.identifier`, and
/// `pool_slug` resolves to `pool.id` via the caller's slug map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolIdentitySeed {
    pub identifier: String,
    pub pool_slug: String,
}

impl PoolIdentitySeed {
    pub fn new(identifier: impl Into<String>, pool_slug: impl Into<String>) -> Self {
        Self {
            identifier: identifier.into(),
            pool_slug: pool_slug.into(),
        }
    }
}

/// Upsert every pool in the snapshot, keyed by `slug`; on conflict the canonical
/// name, coinbase tags and payout addresses are overwritten from the snapshot
/// (the snapshot owns the BTC attribution fields, unlike
/// [`upsert_registry_only_pools`] which preserves them). Returns the resulting
/// slug -> pool.id map that the identity upserts key into.
pub async fn upsert_pool_snapshot<C: GenericClient>(
    client: &C,
    snapshot: &PoolSnapshot,
) -> Result<HashMap<String, i64>> {
    let mut ids = HashMap::new();
    for pool in &snapshot.pools {
        let coinbase_tags = serde_json::to_value(&pool.coinbase_tags)?;
        let payout_addresses = serde_json::to_value(&pool.payout_addresses)?;
        let row = client
            .query_one(
                "INSERT INTO pool (slug, canonical_name, coinbase_tags, payout_addresses) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (slug) DO UPDATE SET \
                   canonical_name = EXCLUDED.canonical_name, \
                   coinbase_tags = EXCLUDED.coinbase_tags, \
                   payout_addresses = EXCLUDED.payout_addresses \
                 RETURNING id",
                &[
                    &pool.slug,
                    &pool.canonical_name,
                    &Json(&coinbase_tags),
                    &Json(&payout_addresses),
                ],
            )
            .await
            .with_context(|| format!("upsert pool {}", pool.slug))?;
        ids.insert(pool.slug.clone(), row.get(0));
    }

    Ok(ids)
}

/// Ensure `pool` rows exist for slugs referenced by an identity registry but
/// not by the BTC pool snapshot. Existing slugs are preserved untouched;
/// missing slugs are created with empty BTC attribution fields because they are
/// reachable only through namespaced `pool_identity` rows.
pub async fn upsert_registry_only_pools(
    client: &Client,
    registry_name: &str,
    pool_definitions: &[(&str, &str)],
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<()> {
    let empty: serde_json::Value = serde_json::json!([]);
    for (slug, canonical_name) in pool_definitions {
        if pool_ids_by_slug.contains_key(*slug) {
            continue;
        }
        let row = client
            .query_one(
                "INSERT INTO pool (slug, canonical_name, coinbase_tags, payout_addresses) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (slug) DO UPDATE SET slug = EXCLUDED.slug \
                 RETURNING id",
                &[slug, canonical_name, &Json(&empty), &Json(&empty)],
            )
            .await
            .with_context(|| format!("upsert {registry_name} pool {slug}"))?;
        pool_ids_by_slug.insert((*slug).to_owned(), row.get(0));
    }
    Ok(())
}

pub(crate) async fn upsert_pool_identities_for_namespace_with_policy(
    client: &Client,
    namespace: &str,
    seeds: &[PoolIdentitySeed],
    pool_ids_by_slug: &HashMap<String, i64>,
    remap_existing: bool,
    conflict_hint: &str,
) -> Result<HashMap<String, i64>> {
    let mut identity_ids = HashMap::new();
    for seed in seeds {
        let pool_id = pool_ids_by_slug
            .get(&seed.pool_slug)
            .copied()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{namespace} identity registry references pool slug {slug} that is not in the pool table",
                    slug = seed.pool_slug
                )
            })?;
        let row = client
            .query_opt(
                &format!(
                    "WITH changed AS ( \
                         INSERT INTO pool_identity (pool_id, namespace, identifier) \
                         VALUES ($1, $2, $3) \
                         ON CONFLICT (namespace, identifier) DO NOTHING \
                         RETURNING id, namespace, identifier \
                     ), {QUEUE_CHANGED_IDENTITIES} \
                     SELECT id FROM changed"
                ),
                &[&pool_id, &namespace, &seed.identifier],
            )
            .await
            .with_context(|| format!("insert pool_identity ({namespace}, {})", seed.identifier))?;
        let identity_id = match row {
            Some(row) => row.get(0),
            None => {
                existing_pool_identity_id(
                    client,
                    namespace,
                    &seed.identifier,
                    pool_id,
                    remap_existing,
                    conflict_hint,
                )
                .await?
            }
        };
        identity_ids.insert(seed.identifier.clone(), identity_id);
    }
    Ok(identity_ids)
}

/// Ensure `pool` rows exist for registry-only slugs and upsert `pool_identity` entries under `namespace`.
pub async fn upsert_identity_registry(
    client: &Client,
    registry_label: &str,
    namespace: &str,
    entries: &[IdentityRegistryEntry<'_>],
    remap_existing: bool,
    conflict_hint: &str,
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<HashMap<String, i64>> {
    let pool_definitions = distinct_pool_definitions(entries.iter().copied());
    upsert_registry_only_pools(client, registry_label, &pool_definitions, pool_ids_by_slug).await?;
    let seeds: Vec<PoolIdentitySeed> = entries
        .iter()
        .map(|entry| PoolIdentitySeed::new(entry.identifier, entry.pool_slug))
        .collect();
    upsert_pool_identities_for_namespace_with_policy(
        client,
        namespace,
        &seeds,
        pool_ids_by_slug,
        remap_existing,
        conflict_hint,
    )
    .await
}

/// Make `namespace`'s `pool_identity` rows equal `entries`, remapping changed identifiers and
/// deleting unlisted ones. Only for namespaces no attribution row references, whose pools are
/// resolved at read time.
pub async fn reconcile_identity_registry(
    client: &Client,
    namespace: &str,
    entries: &[IdentityRegistryEntry<'_>],
    pool_ids_by_slug: &mut HashMap<String, i64>,
) -> Result<()> {
    upsert_identity_registry(
        client,
        namespace,
        namespace,
        entries,
        true,
        "",
        pool_ids_by_slug,
    )
    .await?;
    let listed: Vec<&str> = entries.iter().map(|entry| entry.identifier).collect();
    client
        .execute(
            &format!(
                "WITH changed AS ( \
                     DELETE FROM pool_identity \
                      WHERE namespace = $1 AND identifier <> ALL($2::text[]) \
                     RETURNING namespace, identifier \
                 ), {QUEUE_CHANGED_IDENTITIES} \
                 SELECT 1 FROM changed"
            ),
            &[&namespace, &listed],
        )
        .await
        .with_context(|| format!("delete unlisted {namespace} pool identities"))?;
    Ok(())
}

/// Load `pool_identity` rows for `namespaces` into a `(namespace, identifier)` lookup.
pub async fn load_pool_identities_by_namespace<C: GenericClient>(
    client: &C,
    namespaces: &[&str],
) -> Result<PoolIdentityLookup> {
    let namespace_values = namespaces
        .iter()
        .map(|namespace| (*namespace).to_owned())
        .collect::<Vec<_>>();
    let rows = client
        .query(
            "SELECT namespace, identifier, pool_id, id \
             FROM pool_identity \
             WHERE namespace = ANY($1::text[])",
            &[&namespace_values],
        )
        .await
        .context("load pool identities by namespace")?;

    let mut identities = PoolIdentityLookup::new();
    for row in rows {
        let namespace: String = row.get(0);
        let identifier: String = row.get(1);
        identities.insert(
            (namespace, identifier),
            PoolIdentityRef {
                pool_id: row.get(2),
                pool_identity_id: row.get(3),
                payout_confidence: Default::default(),
            },
        );
    }
    Ok(identities)
}

async fn existing_pool_identity_id(
    client: &Client,
    namespace: &str,
    identifier: &str,
    registry_pool_id: i64,
    remap_existing: bool,
    conflict_hint: &str,
) -> Result<i64> {
    let row = client
        .query_one(
            "SELECT id, pool_id FROM pool_identity WHERE namespace = $1 AND identifier = $2",
            &[&namespace, &identifier],
        )
        .await
        .with_context(|| format!("load pool_identity ({namespace}, {identifier})"))?;
    let identity_id = row.get(0);
    let existing_pool_id: i64 = row.get(1);
    if existing_pool_id == registry_pool_id {
        return Ok(identity_id);
    }
    if !remap_existing {
        anyhow::bail!(
            "{namespace} identity {identifier} is already mapped to pool_id {existing_pool_id}, \
             registry wants pool_id {registry_pool_id}; {conflict_hint}"
        );
    }
    client
        .execute(
            &format!(
                "WITH changed AS ( \
                     UPDATE pool_identity SET pool_id = $1 WHERE id = $2 \
                     RETURNING namespace, identifier \
                 ), {QUEUE_CHANGED_IDENTITIES} \
                 SELECT 1 FROM changed"
            ),
            &[&registry_pool_id, &identity_id],
        )
        .await
        .with_context(|| format!("remap pool_identity ({namespace}, {identifier})"))?;
    Ok(identity_id)
}
