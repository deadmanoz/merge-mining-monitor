//! Retroactive known-stale demotion: re-evaluate EXISTING orphan-classified
//! `block` rows against the known-stale membership.
//!
//! The `compute_block_orphan_class` membership gate keeps NEW classifications
//! from labelling a known stale strict/weak, but a database populated before the
//! membership was imported can still hold contaminated rows (the production bug:
//! a known stale served as `strict_btc_orphan`). This pass finds every
//! `kind = 'unknown'` block that is strict/weak AND in the membership and demotes
//! it to `excluded`, maintaining `source_health` through the same before/after
//! snapshot bracket the reconciler uses. Idempotent (a second run finds nothing)
//! and Core-free (the demotion is a pure membership fact, not a re-classification).

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, GenericClient};
use tracing::warn;

use crate::mutation::PrimarySourceHealthBracket;
use crate::{cli_args, lock_block_hash};

const DEFAULT_BATCH_SIZE: i64 = 100;

/// Config for `run_reclassify_known_stales`. `batch_size` bounds the page of
/// affected parents pulled per iteration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclassifyKnownStalesConfig {
    pub batch_size: i64,
}

impl Default for ReclassifyKnownStalesConfig {
    fn default() -> Self {
        Self {
            batch_size: DEFAULT_BATCH_SIZE,
        }
    }
}

impl ReclassifyKnownStalesConfig {
    /// Parse `reclassify-known-stales` CLI flags via the shared `cli_args`
    /// flag-walk. Rejects non-positive `--batch-size`.
    pub fn from_args<I, S>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut config = Self::default();
        cli_args::drive_args("reclassify-known-stales", args, |flag, cur| {
            Ok(match flag {
                "--batch-size" => {
                    config.batch_size = cur.parse("--batch-size")?;
                    true
                }
                _ => false,
            })
        })?;
        if config.batch_size <= 0 {
            bail!("--batch-size must be positive");
        }
        Ok(config)
    }
}

/// Outcome of one `run_reclassify_known_stales` pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KnownStaleReclassifySummary {
    /// Rows in `known_stale_block` at the start of the pass.
    pub membership_size: i64,
    /// Contaminated `unknown` blocks (strict/weak AND in the membership) demoted
    /// to `excluded` by this pass.
    pub demoted: u64,
}

impl KnownStaleReclassifySummary {
    /// Print the one-line operator-facing summary. Demotions are surfaced loudly
    /// so a non-zero count is obvious in operator logs.
    pub fn print(&self) {
        println!(
            "reclassify-known-stales: membership_size={} demoted_to_excluded={}",
            self.membership_size, self.demoted
        );
    }
}

/// Demote every contaminated orphan-classified block to `excluded`.
///
/// If the membership is EMPTY this is the degraded state the research repo warns
/// about: nothing can be demoted, so it warns loudly and returns without a scan
/// rather than silently reporting success. Otherwise it pages contaminated
/// parents (strict/weak `unknown` blocks whose hash is in `known_stale_block`)
/// and demotes each inside its own transaction, holding the parent advisory lock
/// and applying the `source_health` before/after diff so the strict/weak
/// counters stay consistent. No cascade is needed: `btc_orphan_class` is a leaf
/// property no dependent row keys on.
pub async fn run_reclassify_known_stales(
    client: &mut Client,
    config: ReclassifyKnownStalesConfig,
) -> Result<KnownStaleReclassifySummary> {
    let membership_size = mmm_store::count_known_stale_blocks(client).await?;
    if membership_size == 0 {
        warn!(
            "reclassify-known-stales: known_stale_block is EMPTY; the upstream stale-blocks \
             dataset was never imported, so no known stale can be demoted. Import it with \
             import-known-stales before relying on the strict/weak orphan counts."
        );
        return Ok(KnownStaleReclassifySummary {
            membership_size,
            demoted: 0,
        });
    }

    let mut demoted = 0u64;
    loop {
        let hashes = client
            .query(
                "SELECT b.btc_header_hash \
                 FROM block b \
                 WHERE b.kind = 'unknown' \
                   AND b.btc_orphan_class IN ('strict_btc_orphan', 'weak_btc_orphan') \
                   AND EXISTS ( \
                       SELECT 1 FROM known_stale_block k WHERE k.hash = b.btc_header_hash \
                   ) \
                 ORDER BY b.btc_header_hash \
                 LIMIT $1",
                &[&config.batch_size],
            )
            .await
            .context("scan contaminated known-stale orphan blocks")?;
        if hashes.is_empty() {
            break;
        }
        for row in &hashes {
            let hash: Vec<u8> = row.get(0);
            demoted += demote_one(client, &hash).await?;
        }
    }

    Ok(KnownStaleReclassifySummary {
        membership_size,
        demoted,
    })
}

/// Repair strict/weak orphan rows for membership hashes in an existing
/// transaction.
///
/// The caller must hold the parent advisory locks for `hashes` before changing
/// the membership. This keeps the membership update, derived demotion, and
/// source-health delta atomic for `import-known-stales`.
pub async fn reclassify_known_stale_hashes_in_transaction<C: GenericClient>(
    client: &C,
    hashes: &[Vec<u8>],
) -> Result<u64> {
    let mut demoted = 0;
    for hash in hashes {
        let health = PrimarySourceHealthBracket::open(client, hash).await?;
        demoted += client
            .execute(
                "UPDATE block \
                    SET btc_orphan_class = 'excluded', \
                        updated_at = extract(epoch from now())::bigint \
                  WHERE btc_header_hash = $1 \
                    AND kind = 'unknown' \
                    AND btc_orphan_class IN ('strict_btc_orphan', 'weak_btc_orphan')",
                &[&hash],
            )
            .await
            .context("demote known-stale orphan block to excluded")?;
        health.close(client).await?;
    }
    Ok(demoted)
}

/// Demote one parent's block row inside a transaction, maintaining
/// `source_health`. Returns the rows demoted (0 if a concurrent pass already
/// cleared it, keeping the outer scan safe to re-run).
async fn demote_one(client: &mut Client, hash: &[u8]) -> Result<u64> {
    let txn = client
        .transaction()
        .await
        .context("begin known-stale demotion transaction")?;
    lock_block_hash(&txn, hash).await?;
    let demoted = reclassify_known_stale_hashes_in_transaction(&txn, &[hash.to_vec()]).await?;
    txn.commit()
        .await
        .context("commit known-stale demotion transaction")?;
    Ok(demoted)
}

#[cfg(test)]
mod config_args_tests {
    use super::ReclassifyKnownStalesConfig;

    #[test]
    fn defaults_and_flags() {
        let config =
            ReclassifyKnownStalesConfig::from_args(Vec::<String>::new()).expect("defaults");
        assert_eq!(config.batch_size, 100);
        let config = ReclassifyKnownStalesConfig::from_args(["--batch-size", "7"]).expect("flags");
        assert_eq!(config.batch_size, 7);
    }

    #[test]
    fn pins_error_text() {
        let unknown = ReclassifyKnownStalesConfig::from_args(["--nope"]).expect_err("unknown flag");
        assert_eq!(
            unknown.to_string(),
            "unknown reclassify-known-stales argument \"--nope\""
        );
        let zero = ReclassifyKnownStalesConfig::from_args(["--batch-size", "0"]).expect_err("zero");
        assert_eq!(zero.to_string(), "--batch-size must be positive");
    }
}
