//! Retroactive known-stale demotion for `unknown` blocks in `known_stale_block`.

use anyhow::{Context, Result, bail};
use tokio_postgres::{Client, GenericClient};
use tracing::warn;

use crate::mutation::PrimarySourceHealthBracket;
use crate::{cli_args, lock_block_hash};

const DEFAULT_BATCH_SIZE: i64 = 100;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KnownStaleReclassifySummary {
    pub membership_size: i64,
    pub demoted: u64,
}

impl KnownStaleReclassifySummary {
    pub fn print(&self) {
        println!(
            "reclassify-known-stales: membership_size={} demoted_to_excluded={}",
            self.membership_size, self.demoted
        );
    }
}

/// Demote every strict/weak `unknown` block present in `known_stale_block` to `excluded`.
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

/// Demote strict/weak orphan rows for `hashes` inside an existing transaction.
/// Caller must hold the parent advisory locks for `hashes`.
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
