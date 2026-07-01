//! Live producer chains as declared data plus the shared implementations
//! that consume them.
//!
//! `spec` is the static per-chain table (`CHAINS`); `config` is the only
//! env-reading module under `src/chains/`. Adding a Namecoin-family chain is
//! a spec row plus a `source_registry` entry - never a cloned module (see the
//! Architecture Rules in `AGENTS.md` and `docs/capture.md`).

use mmm_capture::capture::MergeMiningEventPayload;
use reqwest::StatusCode;

pub(crate) mod auxpow_family;
pub(crate) mod backfill;
pub(crate) mod bitcoind_rpc;
pub(crate) mod child_payout_registry;
pub(crate) mod config;
#[cfg(any(test, feature = "db-integration"))]
pub mod elastos;
#[cfg(not(any(test, feature = "db-integration")))]
pub(crate) mod elastos;
#[cfg(any(test, feature = "db-integration"))]
pub mod hathor;
#[cfg(not(any(test, feature = "db-integration")))]
pub(crate) mod hathor;
pub(crate) mod nbits_horizon;
#[cfg(any(test, feature = "db-integration"))]
pub mod rsk;
#[cfg(not(any(test, feature = "db-integration")))]
pub(crate) mod rsk;
pub(crate) mod spec;

#[cfg(any(test, feature = "db-integration"))]
pub use spec::{CHAINS, ChainId, ChainSpec, ReorgPolicy, by_id};

#[derive(Debug)]
struct OfflineValidClassifierConflict;

impl std::fmt::Display for OfflineValidClassifierConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "offline nBits verdict conflicts with classifier")
    }
}

impl std::error::Error for OfflineValidClassifierConflict {}

fn ensure_offline_valid_not_classifier_conflict(
    payload: &MergeMiningEventPayload,
) -> anyhow::Result<()> {
    match payload.difficulty_epoch_ok {
        Some(false) => Err(anyhow::Error::new(OfflineValidClassifierConflict)),
        _ => Ok(()),
    }
}

fn is_offline_valid_classifier_conflict(err: &anyhow::Error) -> bool {
    err.downcast_ref::<OfflineValidClassifierConflict>()
        .is_some()
}

fn is_transient_http_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

/// Which registry-dispatched command a producer subcommand names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommandKind {
    Poll,
    Backfill,
}

/// Pure resolution of `poll-<slug>` / `backfill-<slug>` onto a spec row.
/// Returns `None` for every non-producer command (including
/// `backfill-hathor-cache`, which has its own command facade). Runtime
/// bootstrap and command invocation happen through `run_producer_command`.
fn resolve(command: &str) -> Option<(&'static spec::ChainSpec, CommandKind)> {
    for spec in &spec::CHAINS {
        if command == format!("poll-{}", spec.slug) {
            return Some((spec, CommandKind::Poll));
        }
        if command == format!("backfill-{}", spec.slug) {
            return Some((spec, CommandKind::Backfill));
        }
    }
    None
}

/// True when `command` names one of the registry-dispatched producer commands.
pub fn is_producer_command(command: &str) -> bool {
    resolve(command).is_some()
}

/// Run a registry-dispatched producer command. Resolution stays pure and
/// backfill args are parsed before runtime bootstrap, but command behavior
/// belongs to this facade rather than the spec data rows.
pub async fn run_producer_command<I, S>(command: &str, args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let (spec, kind) =
        resolve(command).ok_or_else(|| anyhow::anyhow!(unknown_command_message(command)))?;
    match kind {
        CommandKind::Poll => {
            let rt = crate::producer_runtime::ProducerRuntime::from_env().await?;
            run_poll_command(spec, rt).await
        }
        CommandKind::Backfill => {
            let config = backfill::BackfillConfig::from_args(spec, args)?;
            let rt = crate::producer_runtime::ProducerRuntime::from_env().await?;
            run_backfill_command(rt, config).await
        }
    }
}

async fn run_poll_command(
    spec: &'static spec::ChainSpec,
    rt: crate::producer_runtime::ProducerRuntime,
) -> anyhow::Result<()> {
    match spec.id {
        spec::ChainId::Namecoin | spec::ChainId::Syscoin | spec::ChainId::Fractal => {
            auxpow_family::poll(spec, rt).await
        }
        spec::ChainId::Rsk => rsk::backfill::poll(spec, rt).await,
        spec::ChainId::Hathor => hathor::backfill::poll(spec, rt).await,
        spec::ChainId::Elastos => elastos::backfill::poll(spec, rt).await,
    }
}

async fn run_backfill_command(
    rt: crate::producer_runtime::ProducerRuntime,
    config: backfill::BackfillConfig,
) -> anyhow::Result<()> {
    match config.spec.id {
        spec::ChainId::Namecoin | spec::ChainId::Syscoin | spec::ChainId::Fractal => {
            auxpow_family::backfill(rt, config).await
        }
        spec::ChainId::Rsk => rsk::backfill::backfill(rt, config).await,
        spec::ChainId::Hathor => hathor::backfill::backfill(rt, config).await,
        spec::ChainId::Elastos => elastos::backfill::backfill(rt, config).await,
    }
}

/// Run the bespoke Hathor archive-cache import command. Keeping this command
/// inside `mmm-producers` lets normal builds hide the Hathor cache/capture
/// modules while preserving the CLI behavior and db-integration test access.
pub async fn run_hathor_cache_command<I, S>(args: I) -> anyhow::Result<()>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let config = hathor::cache::HathorCacheConfig::from_args(args)?;
    let rt = crate::producer_runtime::ProducerRuntime::from_env().await?;
    let mut pg_client = rt.pg_client;
    let context = hathor::capture::HathorCaptureContext::new_with_classifier(
        &pg_client,
        rt.parent_classifier,
    )
    .await?;
    crate::producer_runtime::warn_backfill_classifier_enabled(
        "Hathor cache",
        context.parent_classifier(),
    );

    let csv = std::fs::File::open(&config.csv_path).map(std::io::BufReader::new)?;
    let ledger_path = config.skip_ledger_path();
    let mut ledger = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&ledger_path)
        .map(std::io::BufWriter::new)?;
    let summary =
        hathor::cache::run_hathor_cache_ingest(&mut pg_client, &context, csv, &mut ledger, &config)
            .await?;
    use std::io::Write as _;
    ledger.flush()?;
    println!(
        "hathor cache ingest: rows_seen={} absent_heights={} auxpow_written={} skip ledger at {}",
        summary.rows_seen,
        summary.absent_heights,
        summary.auxpow_written,
        ledger_path.display()
    );

    Ok(())
}

/// The non-producer commands, in the order the unknown-command listing has
/// always shown them.
const NON_PRODUCER_COMMANDS: &str = "import-dataset, reclassify-unknown-parents, reclassify-pools, \
                                     sync-bitcoin-core, reconcile-read-model, \
                                     revoke-merge-mining-event, restore-merge-mining-event, or serve";

/// The unknown-command error, generated from the spec table so it cannot
/// drift. Byte-identical to the historical literal (golden-tested below):
/// all poll commands, then all backfill commands with `backfill-hathor-cache`
/// after `backfill-hathor`, then the non-producer commands.
pub fn unknown_command_message(other: &str) -> String {
    let polls = spec::CHAINS
        .iter()
        .map(|spec| format!("poll-{}", spec.slug))
        .collect::<Vec<_>>()
        .join(", ");
    let backfills = spec::CHAINS
        .iter()
        .flat_map(|spec| {
            if spec.slug == "hathor" {
                vec![
                    format!("backfill-{}", spec.slug),
                    "backfill-hathor-cache".to_owned(),
                ]
            } else {
                vec![format!("backfill-{}", spec.slug)]
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "unknown command {other:?}; expected {polls}, {backfills}, {}",
        NON_PRODUCER_COMMANDS
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    )
}

/// The no-command help line, generated from the spec table. Byte-identical to
/// the historical literal (golden-tested below).
pub fn no_command_help() -> String {
    let mut parts = Vec::new();
    for (idx, spec) in spec::CHAINS.iter().enumerate() {
        let joiner = if idx == 0 { "` or `" } else { "` / `" };
        parts.push(format!(
            "`poll-{slug}{joiner}backfill-{slug} <start-height> <end-height>` for the {name} producer",
            slug = spec.slug,
            joiner = joiner,
            name = spec.display_name,
        ));
    }
    format!(
        "No command selected. Use {}, `sync-bitcoin-core` for the Bitcoin Core backbone, or `serve` for the read API.",
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The config boundary, mechanically enforced: nothing under `src/chains/`
    /// reads process env outside `config.rs`, directly (the std env module)
    /// or indirectly (the env-reading rpc_http helpers that take a var NAME
    /// rather than a value). No exception list - the classifier/DB/serve env
    /// families live with their owners outside `chains/`.
    #[test]
    fn no_env_reads_under_chains_outside_config() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/chains");
        let mut offenders = Vec::new();
        let mut stack = vec![root];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src/chains") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }
                if path.file_name().and_then(|n| n.to_str()) == Some("config.rs") {
                    continue;
                }
                let source = std::fs::read_to_string(&path).expect("read source");
                // Needles are assembled at runtime so this test's own source
                // never matches them.
                let needles = [
                    format!("std::{}", "env"),
                    format!("env::{}(", "var"),
                    format!("parse_timeout_{}", "secs"),
                ];
                for needle in &needles {
                    if source.contains(needle.as_str()) {
                        offenders.push(format!("{}: {needle}", path.display()));
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "env reads under src/chains/ outside config.rs:\n{}",
            offenders.join("\n")
        );
    }

    /// Captured byte-for-byte from the pre-consolidation binary
    /// (src/main.rs Some(other) arm). The generator must never drift from it.
    const GOLDEN_UNKNOWN: &str = "unknown command \"bogus\"; expected poll-namecoin, poll-rsk, poll-syscoin, poll-fractal, poll-hathor, poll-elastos, backfill-namecoin, backfill-rsk, backfill-syscoin, backfill-fractal, backfill-hathor, backfill-hathor-cache, backfill-elastos, import-dataset, reclassify-unknown-parents, reclassify-pools, sync-bitcoin-core, reconcile-read-model, revoke-merge-mining-event, restore-merge-mining-event, or serve";

    /// Captured byte-for-byte from the pre-consolidation binary
    /// (src/main.rs None arm).
    const GOLDEN_HELP: &str = "No command selected. Use `poll-namecoin` or `backfill-namecoin <start-height> <end-height>` for the Namecoin producer, `poll-rsk` / `backfill-rsk <start-height> <end-height>` for the RSK producer, `poll-syscoin` / `backfill-syscoin <start-height> <end-height>` for the Syscoin producer, `poll-fractal` / `backfill-fractal <start-height> <end-height>` for the Fractal producer, `poll-hathor` / `backfill-hathor <start-height> <end-height>` for the Hathor producer, `poll-elastos` / `backfill-elastos <start-height> <end-height>` for the Elastos producer, `sync-bitcoin-core` for the Bitcoin Core backbone, or `serve` for the read API.";

    #[test]
    fn unknown_command_message_is_byte_identical_to_the_golden_capture() {
        assert_eq!(unknown_command_message("bogus"), GOLDEN_UNKNOWN);
    }

    #[test]
    fn no_command_help_is_byte_identical_to_the_golden_capture() {
        assert_eq!(no_command_help(), GOLDEN_HELP);
    }

    #[test]
    fn resolve_covers_every_spec_row_and_only_producer_commands() {
        for spec in &CHAINS {
            let (poll_spec, kind) = resolve(&format!("poll-{}", spec.slug)).unwrap();
            assert_eq!(poll_spec.slug, spec.slug);
            assert_eq!(kind, CommandKind::Poll);
            let (bf_spec, kind) = resolve(&format!("backfill-{}", spec.slug)).unwrap();
            assert_eq!(bf_spec.slug, spec.slug);
            assert_eq!(kind, CommandKind::Backfill);
        }
        assert!(resolve("backfill-hathor-cache").is_none());
        assert!(resolve("serve").is_none());
        assert!(resolve("poll-dogecoin").is_none());
    }

    #[test]
    fn transient_http_status_policy_retries_rate_limit_and_5xx() {
        assert!(is_transient_http_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_transient_http_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_transient_http_status(StatusCode::BAD_GATEWAY));
        assert!(is_transient_http_status(StatusCode::SERVICE_UNAVAILABLE));
        assert!(is_transient_http_status(StatusCode::GATEWAY_TIMEOUT));
        assert!(is_transient_http_status(StatusCode::NOT_IMPLEMENTED));
        assert!(!is_transient_http_status(StatusCode::OK));
        assert!(!is_transient_http_status(StatusCode::NOT_FOUND));
        assert!(!is_transient_http_status(StatusCode::BAD_REQUEST));
    }

    /// `docs/configuration.md` is the operator-facing inventory; this
    /// drift test derives the per-chain names from the spec table so a new
    /// chain (or renamed prefix) cannot land without documentation.
    #[test]
    fn configuration_doc_covers_every_spec_prefix() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/configuration.md"),
        )
        .expect("read docs/configuration.md");
        for spec in &CHAINS {
            assert!(
                doc.contains(&format!("`{}`", spec.env_prefix)),
                "configuration.md does not document prefix {}",
                spec.env_prefix
            );
        }
        for family in [
            "_RPC_URL",
            "_RPC_TIMEOUT_SECS",
            "_START_HEIGHT",
            "_POLL_INTERVAL_SECONDS",
            "_BATCH_SIZE",
            "_REORG_DEPTH",
            "_MAX_BACKFILL_RANGE",
            "_ALLOW_LARGE_BACKFILL",
        ] {
            assert!(
                doc.contains(family),
                "configuration.md does not document the {family} family"
            );
        }
        for special in [
            "HATHOR_RPC_FALLBACK_URL",
            "HATHOR_BACKFILL_SKIP_HOLDS",
            "RSK_BACKFILL_FETCH_CONCURRENCY",
            "BITCOIN_RPC_URL",
            "SERVE_BIND_ADDR",
        ] {
            assert!(
                doc.contains(special),
                "configuration.md does not document {special}"
            );
        }
    }

    #[test]
    fn backfill_usage_strings_are_byte_identical_for_every_chain() {
        for spec in &CHAINS {
            let empty: std::collections::HashMap<String, String> = std::collections::HashMap::new();
            let err = crate::chains::backfill::BackfillConfig::from_args_with_lookup(
                spec,
                ["10"],
                |key| empty.get(key).cloned(),
            )
            .unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("usage: backfill-{} <start-height> <end-height>", spec.slug)
            );
        }
    }
}
